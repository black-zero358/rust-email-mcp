//! Keep SMTP transaction outcomes separate from post-transaction QUIT cleanup.
use crate::config::Config;
use lettre::{
    address::Envelope,
    transport::smtp::{
        Error,
        authentication::{Credentials, DEFAULT_MECHANISMS},
        client::{AsyncSmtpConnection, TlsParameters},
        commands::{Data, Mail, Rcpt},
        extension::{ClientId, Extension, MailBodyParameter, MailParameter},
        response::Severity,
    },
};
use std::time::Duration;

pub(crate) const QUIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Validate DATA representability without rewriting MIME bytes. The codec adds
/// the final CRLF, so a nonempty last line need not already end in CRLF.
pub(crate) fn validate_data(raw: &[u8]) -> crate::error::Result<()> {
    let invalid = || crate::error::MailError {
        code: "UNSUPPORTED_DATA_FORMAT",
        message: "SMTP DATA requires CRLF lines of at most 998 bytes without NUL; encode binary attachments before sending.",
        retryable: false,
        details: None,
    };
    let mut bytes = raw.iter();
    let mut line_bytes = 0;
    while let Some(&byte) = bytes.next() {
        match byte {
            0 | b'\n' => return Err(invalid()),
            b'\r' => {
                if bytes.next() != Some(&b'\n') {
                    return Err(invalid());
                }
                line_bytes = 0;
            }
            _ => {
                line_bytes += 1;
                // Count source bytes, before transparency adds a leading dot.
                if line_bytes > 998 {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("Required SMTP extension unavailable before MAIL")]
struct RequiredExtensionUnavailable;

pub(crate) struct SmtpSender {
    host: String,
    port: u16,
    timeout: Duration,
    tls: Option<TlsParameters>,
    starttls: bool,
    credentials: Option<Credentials>,
}
pub(crate) struct Submission {
    pub status: &'static str,
    connection: Option<AsyncSmtpConnection>,
}
impl Submission {
    pub async fn cleanup(mut self) {
        // The caller has already persisted status. Keep its execution slot until
        // cleanup completes, so slow QUIT cannot accumulate unbounded tasks.
        if let Some(connection) = self.connection.as_mut() {
            let _ = tokio::time::timeout(QUIT_TIMEOUT, connection.abort()).await;
        }
    }
}
impl SmtpSender {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            host: config.smtp_host.clone(),
            port: config.smtp_port,
            timeout: config.timeout,
            tls: Some(TlsParameters::new(config.smtp_host.clone())?),
            starttls: config.smtp_starttls,
            credentials: Some(Credentials::new(
                config.address.clone(),
                config.password.clone(),
            )),
        })
    }
    #[cfg(test)]
    pub fn fixture(port: u16, timeout: Duration) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port,
            timeout,
            tls: None,
            starttls: false,
            credentials: None,
        }
    }
    async fn connect(&self) -> anyhow::Result<AsyncSmtpConnection> {
        let hello = ClientId::default();
        let mut connection = AsyncSmtpConnection::connect_tokio1(
            (&*self.host, self.port),
            Some(self.timeout),
            &hello,
            if self.starttls {
                None
            } else {
                self.tls.clone()
            },
            None,
        )
        .await?;
        if self.starttls {
            connection
                .starttls(self.tls.clone().expect("production TLS parameters"), &hello)
                .await?;
        }
        if let Some(credentials) = &self.credentials {
            connection.auth(DEFAULT_MECHANISMS, credentials).await?;
        }
        Ok(connection)
    }
    pub async fn submit(&self, envelope: &Envelope, raw: &[u8]) -> Submission {
        // Defense in depth for direct callers. The service validates before
        // claiming a send; this path must not connect or report uncertainty.
        if validate_data(raw).is_err() {
            return Submission {
                status: "rejected",
                connection: None,
            };
        }

        let utf8_headers = match crate::mime::requires_smtputf8(raw) {
            Ok(required) => required,
            Err(_) => {
                return Submission {
                    status: "rejected",
                    connection: None,
                };
            }
        };
        let mut connection = None;
        let result = tokio::time::timeout(self.timeout, async {
            connection = Some(self.connect().await?);
            transaction(
                connection.as_mut().expect("connected"),
                envelope,
                raw,
                utf8_headers,
            )
            .await
        })
        .await;
        let status = match &result {
            Ok(Ok(())) => "accepted",
            Ok(Err(error)) if error.is::<RequiredExtensionUnavailable>() => "rejected",
            Ok(Err(error))
                if error
                    .downcast_ref::<Error>()
                    .is_some_and(|e| e.is_permanent() || e.is_transient()) =>
            {
                "rejected"
            }
            _ => "unknown",
        };
        // Unknown means we may still be in DATA. Drop the socket instead of
        // emitting QUIT into message content or ever recycling this connection.
        if status == "unknown" {
            connection = None;
        }
        Submission { status, connection }
    }
}

async fn transaction(
    connection: &mut AsyncSmtpConnection,
    envelope: &Envelope,
    raw: &[u8],
    utf8_headers: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(!envelope.to().is_empty(), "SMTP envelope has no recipients");
    // Match lettre's send() extension policy, but keep cleanup outside the
    // transaction: its send() aborts internally before returning an error.
    let mut options = Vec::new();
    if utf8_headers
        || envelope
            .from()
            .into_iter()
            .chain(envelope.to())
            .any(|address| !AsRef::<str>::as_ref(address).is_ascii())
    {
        if !connection
            .server_info()
            .supports_feature(Extension::SmtpUtfEight)
        {
            return Err(RequiredExtensionUnavailable.into());
        }
        options.push(MailParameter::SmtpUtfEight);
    }
    if !raw.is_ascii() {
        if !connection
            .server_info()
            .supports_feature(Extension::EightBitMime)
        {
            return Err(RequiredExtensionUnavailable.into());
        }
        options.push(MailParameter::Body(MailBodyParameter::EightBitMime));
    }
    let response = connection
        .command(Mail::new(envelope.from().cloned(), options))
        .await?;
    anyhow::ensure!(
        response.code().severity == Severity::PositiveCompletion,
        "Unexpected MAIL response"
    );
    for recipient in envelope.to() {
        let response = connection
            .command(Rcpt::new(recipient.clone(), vec![]))
            .await?;
        anyhow::ensure!(
            response.code().severity == Severity::PositiveCompletion,
            "Unexpected RCPT response"
        );
    }
    let response = connection.command(Data).await?;
    anyhow::ensure!(
        response.code().to_string() == "354",
        "Unexpected DATA response"
    );
    let response = connection.message(raw).await?;
    anyhow::ensure!(
        response.code().to_string() == "250",
        "Unexpected final DATA response"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn data_line_validation_uses_source_octets_and_allows_final_line() {
        let mut dotted = vec![b'.'];
        dotted.extend(std::iter::repeat_n(b'a', 997));
        assert!(validate_data(&dotted).is_ok());
        dotted.push(b'a');
        assert!(validate_data(&dotted).is_err());
        assert!(validate_data(&[0xff, 0xfe, b'\r', b'\n', b'a']).is_ok());
        assert!(validate_data(b"ordinary\r\nsecond line").is_ok());
        assert!(validate_data(b"ordinary\r").is_err());
        assert!(validate_data(b"ordinary\n").is_err());
    }

    async fn scripted_send(
        steps: Vec<(&'static str, &'static str)>,
        unicode: bool,
        credentials: bool,
        starttls: bool,
    ) -> &'static str {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let backend = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            socket.write_all(b"220 synthetic ESMTP\r\n").await.unwrap();
            for (expected, reply) in steps {
                let mut line = String::new();
                if expected == "MESSAGE" {
                    let mut message = String::new();
                    loop {
                        line.clear();
                        assert!(socket.read_line(&mut line).await.unwrap() > 0);
                        if line == ".\r\n" {
                            break;
                        }
                        message.push_str(&line);
                    }
                    assert!(message.contains("..dot-stuffed"));
                } else {
                    let read = socket.read_line(&mut line).await.unwrap();
                    if expected == "EOF" {
                        assert_eq!(read, 0);
                        break;
                    }
                    assert!(
                        read > 0 && line.starts_with(expected),
                        "expected {expected}, got {line:?}"
                    );
                }
                match reply {
                    "disconnect" => break,
                    "stall" => {
                        tokio::time::sleep(Duration::from_millis(800)).await;
                    }
                    _ => socket.write_all(reply.as_bytes()).await.unwrap(),
                }
            }
        });
        let mut sender = SmtpSender::fixture(port, Duration::from_millis(500));
        if credentials {
            sender.credentials = Some(Credentials::new("synthetic".into(), "test-only".into()));
        }
        if starttls {
            sender.starttls = true;
            sender.tls = Some(TlsParameters::new("localhost".into()).unwrap());
        }
        let recipient = if unicode {
            "用户@example.com"
        } else {
            "one@example.com"
        };
        let envelope = Envelope::new(
            Some("sender@example.com".parse().unwrap()),
            vec![
                recipient.parse().unwrap(),
                "two@example.com".parse().unwrap(),
            ],
        )
        .unwrap();
        let raw = if unicode {
            "Subject: synthetic\r\n\r\n中\r\n.dot-stuffed"
        } else {
            "Subject: synthetic\r\n\r\ntext\r\n.dot-stuffed"
        };
        let result = sender.submit(&envelope, raw.as_bytes()).await;
        let status = result.status;
        result.cleanup().await;
        backend.await.unwrap();
        status
    }

    #[tokio::test]
    async fn smtp_final_outcome_survives_quit_and_negative_responses() {
        for (final_reply, cleanup, expected) in [
            ("250 accepted\r\n", "disconnect", "accepted"),
            ("250 accepted\r\n", "stall", "accepted"),
            ("550 rejected\r\n", "stall", "rejected"),
            ("450 deferred\r\n", "221 bye\r\n", "rejected"),
            ("disconnect", "", "unknown"),
            ("stall", "", "unknown"),
            ("354 unexpected continuation\r\n", "", "unknown"),
        ] {
            let mut steps = vec![
                ("EHLO", "250 test\r\n"),
                ("MAIL", "250 ok\r\n"),
                ("RCPT", "250 ok\r\n"),
                ("RCPT", "250 ok\r\n"),
                ("DATA", "354 data\r\n"),
                ("MESSAGE", final_reply),
            ];
            if !cleanup.is_empty() {
                steps.push(("QUIT", cleanup));
            } else if final_reply == "stall" {
                steps.push(("EOF", ""));
            }
            assert_eq!(scripted_send(steps, false, false, false).await, expected);
        }
    }
}
