//! IMAP4rev1 text handling. This client never enables UTF8=ACCEPT.
use crate::error::{MailError, Result};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use imap_proto::{MailboxDatum, Response, ResponseCode, Status};
use std::{collections::HashSet, fmt::Debug};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Encode user-facing Unicode names using RFC 3501 section 5.1.3.
pub(crate) fn mailbox_name(name: &str) -> Result<String> {
    if name.len() > 512 || name.contains(['\r', '\n', '\0']) {
        return Err(MailError::input());
    }
    let mut encoded = String::new();
    let mut shifted = Vec::new();
    fn flush(encoded: &mut String, shifted: &mut Vec<u8>) {
        if !shifted.is_empty() {
            encoded.push('&');
            encoded.push_str(&STANDARD_NO_PAD.encode(&*shifted).replace('/', ","));
            encoded.push('-');
            shifted.clear();
        }
    }
    for ch in name.chars() {
        if (' '..='~').contains(&ch) {
            flush(&mut encoded, &mut shifted);
            if ch == '&' {
                encoded.push_str("&-");
            } else {
                encoded.push(ch);
            }
        } else {
            let mut units = [0; 2];
            for unit in ch.encode_utf16(&mut units) {
                shifted.extend_from_slice(&unit.to_be_bytes());
            }
        }
    }
    flush(&mut encoded, &mut shifted);
    Ok(encoded)
}

/// Reject noncanonical wire names rather than returning a name that encodes to
/// a different mailbox. API names can be fed directly into later folder tools.
pub(crate) fn display_name(wire: &str) -> Result<String> {
    if !wire.is_ascii() || wire.len() > 2048 {
        return Err(MailError::backend());
    }
    let mut rest = wire;
    let mut decoded = String::new();
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        let end = rest.find('-').ok_or_else(MailError::backend)?;
        let part = &rest[..end];
        if part.is_empty() {
            decoded.push('&');
        } else {
            let bytes = STANDARD_NO_PAD
                .decode(part.replace(',', "/"))
                .map_err(|_| MailError::backend())?;
            if bytes.len() % 2 != 0 {
                return Err(MailError::backend());
            }
            let units = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_be_bytes(*p))
                .collect::<Vec<_>>();
            decoded.push_str(&String::from_utf16(&units).map_err(|_| MailError::backend())?);
        }
        rest = &rest[end + 1..];
    }
    decoded.push_str(rest);
    if mailbox_name(&decoded).map_err(|_| MailError::backend())? != wire {
        return Err(MailError::backend());
    }
    Ok(decoded)
}

fn completion_error(code: Option<&ResponseCode<'_>>) -> MailError {
    if matches!(code, Some(ResponseCode::BadCharset(_))) {
        MailError {
            code: "UNSUPPORTED_CHARSET",
            message: "The IMAP server does not support UTF-8 search.",
            retryable: false,
        }
    } else {
        MailError::backend()
    }
}

pub(crate) async fn uid_text_search<S>(
    session: &mut async_imap::Session<S>,
    start: u32,
    end: u32,
    text: &str,
) -> Result<HashSet<u32>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    if text.len() > 512 || text.contains(['\r', '\n', '\0']) {
        return Err(MailError::input());
    }
    if text.is_ascii() {
        return session
            .uid_search(format!(
                "UID {start}:{end} TEXT \"{}\"",
                text.replace('\\', "\\\\").replace('"', "\\\"")
            ))
            .await
            .map(|ids| {
                ids.into_iter()
                    .filter(|uid| *uid >= start && *uid <= end)
                    .collect()
            })
            .map_err(|_| MailError::backend());
    }
    // A rev1 quoted string cannot carry UTF-8 octets. Use a synchronizing
    // literal, and send its bytes only after the server requests continuation.
    let tag = session
        .run_command(format!(
            "UID SEARCH CHARSET UTF-8 UID {start}:{end} TEXT {{{}}}",
            text.len()
        ))
        .await
        .map_err(|_| MailError::backend())?;
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|_| MailError::backend())?
            .ok_or_else(MailError::backend)?;
        match response.parsed() {
            Response::Continue(_) => break,
            Response::Done {
                tag: received,
                outcome,
                ..
            } => {
                return Err(if received == &tag {
                    completion_error(outcome.code.as_ref())
                } else {
                    MailError::backend()
                });
            }
            Response::Data {
                status: Status::Bye,
                ..
            } => return Err(MailError::backend()),
            _ => {}
        }
    }
    session
        .get_mut()
        .write_all(text.as_bytes())
        .await
        .map_err(|_| MailError::backend())?;
    session
        .get_mut()
        .write_all(b"\r\n")
        .await
        .map_err(|_| MailError::backend())?;
    session
        .get_mut()
        .flush()
        .await
        .map_err(|_| MailError::backend())?;
    let mut ids = HashSet::new();
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|_| MailError::backend())?
            .ok_or_else(MailError::backend)?;
        match response.parsed() {
            Response::MailboxData(MailboxDatum::Search(found)) => {
                ids.extend(
                    found
                        .iter()
                        .copied()
                        .filter(|uid| *uid >= start && *uid <= end),
                );
            }
            Response::Done {
                tag: received,
                status,
                outcome,
            } => {
                if received == &tag && *status == Status::Ok {
                    return Ok(ids);
                }
                return Err(if received == &tag {
                    completion_error(outcome.code.as_ref())
                } else {
                    MailError::backend()
                });
            }
            Response::Data {
                status: Status::Bye,
                ..
            }
            | Response::Continue(_) => return Err(MailError::backend()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    #[test]
    fn rev1_names_round_trip_and_reject_ambiguous_wire_names() {
        for (name, wire) in [
            ("INBOX", "INBOX"),
            ("Projects & Reports", "Projects &- Reports"),
            ("项目", "&mHl27g-"),
            ("😀", "&2D3eAA-"),
            ("~peter/mail/台北/日本語", "~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
        ] {
            assert_eq!(mailbox_name(name).unwrap(), wire);
            assert_eq!(display_name(wire).unwrap(), name);
        }
        for wire in [
            "项目",
            "&Jjo!",
            "&AGE-",
            "&2AA-",
            "&AA-",
            "&U,BTFw-&ZeVnLIqe-",
            "&Jjo=-",
        ] {
            assert!(display_name(wire).is_err(), "{wire}");
        }
        assert!(mailbox_name("bad\r\nname").is_err());
        assert!(mailbox_name(&"x".repeat(513)).is_err());
    }

    #[tokio::test]
    async fn rev1_search_literal_continuation_and_tagged_results() {
        for text in ["invoice \"quoted\" \\ path", "发票", "& 日本語 😀"] {
            for (early, status) in [
                (false, "OK"),
                (false, "NO"),
                (false, "BAD"),
                (false, "BADCHARSET"),
                (false, "EOF"),
                (false, "WRONG_TAG"),
                (true, "NO"),
                (true, "BADCHARSET"),
            ] {
                // ASCII remains the rev1 US-ASCII quoted-string path. Its
                // completion semantics are covered separately by issue #2.
                if text.is_ascii() && (early || status != "OK") {
                    continue;
                }
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    let (client, server) = tokio::io::duplex(8192);
                    let backend = tokio::spawn(async move {
                        let mut server = BufReader::new(server);
                        let mut line = String::new();
                        server.read_line(&mut line).await.unwrap();
                        let tag = line.split_whitespace().next().unwrap();
                        server
                            .write_all(format!("{tag} OK login\r\n").as_bytes())
                            .await
                            .unwrap();
                        line.clear();
                        server.read_line(&mut line).await.unwrap();
                        let (tag, command) = line.split_once(' ').unwrap();
                        let tag = tag.to_owned();
                        if text.is_ascii() {
                            assert_eq!(
                                command,
                                "UID SEARCH UID 1:500 TEXT \"invoice \\\"quoted\\\" \\\\ path\"\r\n"
                            );
                        } else {
                            assert_eq!(
                                command,
                                format!(
                                    "UID SEARCH CHARSET UTF-8 UID 1:500 TEXT {{{}}}\r\n",
                                    text.len()
                                )
                            );
                            if !early {
                                assert!(
                                    tokio::time::timeout(
                                        std::time::Duration::from_millis(10),
                                        server.fill_buf()
                                    )
                                    .await
                                    .is_err(),
                                    "literal was sent before continuation"
                                );
                                server
                                    .write_all(b"* 2 EXISTS\r\n+ send literal\r\n")
                                    .await
                                    .unwrap();
                                let mut literal = vec![0; text.len() + 2];
                                server.read_exact(&mut literal).await.unwrap();
                                assert_eq!(literal, format!("{text}\r\n").as_bytes());
                            }
                        }
                        // Partial result must never turn a terminal failure into success.
                        if !early {
                            server.write_all(b"* SEARCH 7 9 501\r\n").await.unwrap();
                        }
                        if status != "EOF" {
                            let response = match status {
                                "BADCHARSET" => {
                                    format!("{tag} NO [BADCHARSET (US-ASCII)] unsupported\r\n")
                                }
                                "WRONG_TAG" => "OTHER OK unrelated\r\n".to_owned(),
                                _ => format!("{tag} {status} done\r\n"),
                            };
                            server.write_all(response.as_bytes()).await.unwrap();
                        }
                        if early {
                            let mut rest = Vec::new();
                            server.read_to_end(&mut rest).await.unwrap();
                            assert!(rest.is_empty(), "literal sent after rejection");
                        }
                    });
                    let mut session = async_imap::Client::new(client)
                        .login("synthetic", "test-only")
                        .await
                        .unwrap();
                    let result = uid_text_search(&mut session, 1, 500, text).await;
                    drop(session);
                    backend.await.unwrap();
                    if status == "OK" {
                        assert_eq!(result.unwrap(), HashSet::from([7, 9]));
                    } else {
                        assert_eq!(
                            result.unwrap_err().code,
                            if status == "BADCHARSET" {
                                "UNSUPPORTED_CHARSET"
                            } else {
                                "MAIL_BACKEND"
                            }
                        );
                    }
                })
                .await
                .unwrap();
            }
        }
    }
}
