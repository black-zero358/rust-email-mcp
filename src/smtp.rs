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
            let _ = tokio::time::timeout(Duration::from_secs(1), connection.abort()).await;
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
        let mut connection = None;
        let result = tokio::time::timeout(self.timeout, async {
            connection = Some(self.connect().await?);
            transaction(connection.as_mut().expect("connected"), envelope, raw).await
        })
        .await;
        let status = match &result {
            Ok(Ok(())) => "accepted",
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
) -> anyhow::Result<()> {
    anyhow::ensure!(!envelope.to().is_empty(), "SMTP envelope has no recipients");
    // Match lettre's send() extension policy, but keep cleanup outside the
    // transaction: its send() aborts internally before returning an error.
    let mut options = Vec::new();
    if envelope
        .from()
        .into_iter()
        .chain(envelope.to())
        .any(|address| !AsRef::<str>::as_ref(address).is_ascii())
    {
        anyhow::ensure!(
            connection
                .server_info()
                .supports_feature(Extension::SmtpUtfEight),
            "SMTPUTF8 unavailable"
        );
        options.push(MailParameter::SmtpUtfEight);
    }
    if !raw.is_ascii() {
        anyhow::ensure!(
            connection
                .server_info()
                .supports_feature(Extension::EightBitMime),
            "8BITMIME unavailable"
        );
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
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                    _ => socket.write_all(reply.as_bytes()).await.unwrap(),
                }
            }
        });
        let mut sender = SmtpSender::fixture(port, Duration::from_millis(150));
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

    #[tokio::test]
    async fn smtp_partial_recipient_rejection_never_sends_data() {
        assert_eq!(
            scripted_send(
                vec![
                    ("EHLO", "250 test\r\n"),
                    ("MAIL", "250 ok\r\n"),
                    ("RCPT", "250 ok\r\n"),
                    ("RCPT", "550 denied\r\n"),
                    ("QUIT", "221 bye\r\n")
                ],
                false,
                false,
                false
            )
            .await,
            "rejected"
        );
        assert!(Envelope::new(None, vec![]).is_err());
    }

    #[tokio::test]
    async fn smtp_extensions_auth_and_required_starttls() {
        assert_eq!(
            scripted_send(
                vec![
                    ("EHLO", "250-test\r\n250-8BITMIME\r\n250 SMTPUTF8\r\n"),
                    (
                        "MAIL FROM:<sender@example.com> SMTPUTF8 BODY=8BITMIME",
                        "250 ok\r\n"
                    ),
                    ("RCPT", "250 ok\r\n"),
                    ("RCPT", "250 ok\r\n"),
                    ("DATA", "354 data\r\n"),
                    ("MESSAGE", "250 accepted\r\n"),
                    ("QUIT", "221 bye\r\n")
                ],
                true,
                false,
                false
            )
            .await,
            "accepted"
        );
        for greeting in ["250 test\r\n", "250-test\r\n250 SMTPUTF8\r\n"] {
            assert_eq!(
                scripted_send(vec![("EHLO", greeting), ("EOF", "")], true, false, false).await,
                "unknown"
            );
        }
        assert_eq!(
            scripted_send(
                vec![
                    ("EHLO", "250-test\r\n250 AUTH PLAIN\r\n"),
                    ("AUTH PLAIN", "235 authenticated\r\n"),
                    ("MAIL", "250 ok\r\n"),
                    ("RCPT", "250 ok\r\n"),
                    ("RCPT", "250 ok\r\n"),
                    ("DATA", "354 data\r\n"),
                    ("MESSAGE", "250 accepted\r\n"),
                    ("QUIT", "221 bye\r\n")
                ],
                false,
                true,
                false
            )
            .await,
            "accepted"
        );
        // No credentials may be sent when required STARTTLS was not offered.
        assert_eq!(
            scripted_send(
                vec![
                    ("EHLO", "250-test\r\n250 AUTH PLAIN\r\n"),
                    ("AUTH PLAIN", "535 denied\r\n"),
                    ("QUIT", "221 bye\r\n")
                ],
                false,
                true,
                false
            )
            .await,
            "rejected"
        );
        assert_eq!(
            scripted_send(
                vec![("EHLO", "250-test\r\n250 AUTH PLAIN\r\n"), ("EOF", "")],
                false,
                true,
                true
            )
            .await,
            "unknown"
        );
    }

    #[tokio::test]
    async fn tls_handshakes_fail_closed_before_credentials_or_mail() {
        use tokio::io::AsyncReadExt;
        for starttls in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let backend = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                if starttls {
                    socket.write_all(b"220 synthetic ESMTP\r\n").await.unwrap();
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    assert!(line.starts_with("EHLO"));
                    socket
                        .write_all(b"250-test\r\n250 STARTTLS\r\n")
                        .await
                        .unwrap();
                    line.clear();
                    socket.read_line(&mut line).await.unwrap();
                    assert_eq!(line, "STARTTLS\r\n");
                    socket.write_all(b"220 upgrade\r\n").await.unwrap();
                }
                let mut first_byte = [0];
                socket.read_exact(&mut first_byte).await.unwrap();
                assert_eq!(first_byte[0], 0x16); // TLS handshake record, never AUTH/MAIL.
            });
            let mut sender = SmtpSender::fixture(port, Duration::from_secs(1));
            sender.tls = Some(TlsParameters::new("localhost".into()).unwrap());
            sender.starttls = starttls;
            sender.credentials = Some(Credentials::new("synthetic".into(), "test-only".into()));
            let envelope = Envelope::new(None, vec!["one@example.com".parse().unwrap()]).unwrap();
            let submission = sender.submit(&envelope, b"synthetic").await;
            assert_eq!(submission.status, "unknown");
            submission.cleanup().await;
            backend.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelling_a_transaction_closes_it_without_quit_or_reuse() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let backend = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            socket.write_all(b"220 synthetic ESMTP\r\n").await.unwrap();
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("EHLO"));
            started_tx.send(()).unwrap();
            line.clear();
            assert_eq!(socket.read_line(&mut line).await.unwrap(), 0);
        });
        let sending = tokio::spawn(async move {
            let sender = SmtpSender::fixture(port, Duration::from_secs(5));
            let envelope = Envelope::new(None, vec!["one@example.com".parse().unwrap()]).unwrap();
            sender.submit(&envelope, b"synthetic").await
        });
        started_rx.await.unwrap();
        sending.abort();
        assert!(matches!(sending.await, Err(error) if error.is_cancelled()));
        backend.await.unwrap();
    }
}
