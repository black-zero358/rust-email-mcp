//! IMAP4rev1 text handling. This client never enables UTF8=ACCEPT.
use crate::error::{MailError, Result};
use crate::input_limits::{InputLimits, ReadInputLimits, read_query};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use imap_proto::{MailboxDatum, Response, ResponseCode, Status};
use std::{collections::HashSet, fmt::Debug};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Encode user-facing Unicode names using RFC 3501 section 5.1.3.
pub(crate) fn mailbox_name(name: &str, limits: InputLimits) -> Result<String> {
    limits.mailbox(name, "/mailbox")?;
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
    if matches!(limits, InputLimits::Read) {
        validate_mailbox_wire_bytes(encoded.len())?;
    }
    Ok(encoded)
}

fn validate_mailbox_wire_bytes(actual: usize) -> Result<()> {
    if actual > ReadInputLimits::MAILBOX_WIRE_BYTES {
        return Err(MailError::input_field(
            crate::error::InputViolation::new("/mailbox", "wire_byte_limit", "shorten_field")
                .with_length(
                    ReadInputLimits::MAILBOX_WIRE_BYTES,
                    actual,
                    "modified_utf7_bytes",
                ),
        ));
    }
    Ok(())
}

/// Reject noncanonical wire names rather than returning a name that encodes to
/// a different mailbox. API names can be fed directly into later folder tools.
pub(crate) fn display_name(wire: &str) -> Result<String> {
    if !wire.is_ascii() || validate_mailbox_wire_bytes(wire.len()).is_err() {
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
    if mailbox_name(&decoded, InputLimits::Read).map_err(|_| MailError::backend())? != wire {
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
            details: None,
        }
    } else {
        MailError::backend()
    }
}

#[cfg(test)]
pub(crate) async fn uid_text_search<S>(
    session: &mut async_imap::Session<S>,
    start: u32,
    end: u32,
    text: &str,
) -> Result<HashSet<u32>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    uid_field_search(session, start, end, "TEXT", text).await
}

fn read_imap_error(error: async_imap::error::Error) -> MailError {
    if async_imap::error::body_structure_limit(&error).is_some() {
        MailError::limit_kind(crate::error::LimitKind::MimeStructure)
    } else if let Some(limit) = async_imap::error::body_section_literal_limit(&error) {
        MailError::limit_with(
            crate::error::LimitKind::PartFetchBytes,
            limit.limit,
            limit.actual,
        )
    } else if matches!(&error, async_imap::error::Error::Io(error) if error.get_ref().is_some_and(|cause| cause.is::<crate::error::ConnectionBudgetExceeded>()))
    {
        MailError::limit_kind(crate::error::LimitKind::ConnectionBytes)
    } else {
        MailError::backend()
    }
}

pub(crate) async fn uid_field_search<S>(
    session: &mut async_imap::Session<S>,
    start: u32,
    end: u32,
    key: &str,
    text: &str,
) -> Result<HashSet<u32>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    if !["TEXT", "FROM", "TO", "SUBJECT"].contains(&key) {
        return Err(MailError::input());
    }
    let field = match key {
        "FROM" => "/from",
        "TO" => "/to",
        "SUBJECT" => "/subject",
        _ => "/query",
    };
    read_query(text, field)?;
    if text.is_ascii() {
        return session
            .uid_search(format!(
                "UID {start}:{end} {key} \"{}\"",
                text.replace('\\', "\\\\").replace('"', "\\\"")
            ))
            .await
            .map(|ids| {
                ids.into_iter()
                    .filter(|uid| *uid >= start && *uid <= end)
                    .collect()
            })
            .map_err(read_imap_error);
    }
    // A rev1 quoted string cannot carry UTF-8 octets. Use a synchronizing
    // literal, and send its bytes only after the server requests continuation.
    let tag = session
        .run_command(format!(
            "UID SEARCH CHARSET UTF-8 UID {start}:{end} {key} {{{}}}",
            text.len()
        ))
        .await
        .map_err(read_imap_error)?;
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|error| read_imap_error(error.into()))?
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
        .map_err(|error| read_imap_error(error.into()))?;
    session
        .get_mut()
        .write_all(b"\r\n")
        .await
        .map_err(|error| read_imap_error(error.into()))?;
    session
        .get_mut()
        .flush()
        .await
        .map_err(|error| read_imap_error(error.into()))?;
    let mut ids = HashSet::new();
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|error| read_imap_error(error.into()))?
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

/// STATUS for the statistics tool: require all requested fields and matching completion.
pub(crate) async fn status_counts<S>(
    session: &mut async_imap::Session<S>,
    mailbox: &str,
) -> Result<async_imap::types::Mailbox>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Debug,
{
    use imap_proto::StatusAttribute;
    let encoded = mailbox_name(mailbox, InputLimits::Read)?;
    let quoted = encoded.replace('\\', "\\\\").replace('"', "\\\"");
    let tag = session
        .run_command(format!(
            "STATUS \"{quoted}\" (MESSAGES UNSEEN RECENT UIDNEXT UIDVALIDITY)"
        ))
        .await
        .map_err(read_imap_error)?;
    let mut value = async_imap::types::Mailbox::default();
    let mut present = 0_u8;
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|error| read_imap_error(error.into()))?
            .ok_or_else(MailError::backend)?;
        match response.parsed() {
            Response::MailboxData(MailboxDatum::Status {
                mailbox: got,
                status,
            }) if got == &encoded
                || (encoded.eq_ignore_ascii_case("INBOX") && got.eq_ignore_ascii_case("INBOX")) =>
            {
                // Each matching STATUS is a snapshot; do not combine partial snapshots.
                present = 0;
                value = async_imap::types::Mailbox::default();
                for attribute in status {
                    match attribute {
                        StatusAttribute::Messages(n) => {
                            value.exists = *n;
                            present |= 1;
                        }
                        StatusAttribute::Unseen(n) => {
                            value.unseen = Some(*n);
                            present |= 2;
                        }
                        StatusAttribute::Recent(n) => {
                            value.recent = *n;
                            present |= 4;
                        }
                        StatusAttribute::UidNext(n) => {
                            value.uid_next = Some(*n);
                            present |= 8;
                        }
                        StatusAttribute::UidValidity(n) => {
                            value.uid_validity = Some(*n);
                            present |= 16;
                        }
                        _ => {}
                    }
                }
            }
            Response::Done {
                tag: got, status, ..
            } => {
                return if got == &tag
                    && *status == Status::Ok
                    && present == 31
                    && value.uid_next.is_some_and(|n| n > 0)
                    && value.uid_validity.is_some_and(|n| n > 0)
                {
                    Ok(value)
                } else {
                    Err(MailError::backend())
                };
            }
            Response::Continue(_)
            | Response::Data {
                status: Status::Bye,
                ..
            } => return Err(MailError::backend()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn statistics_status_requires_complete_matching_snapshot_and_tag() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let cases = [
            (
                "* STATUS inbox (MESSAGES 0 UNSEEN 0 RECENT 0 UIDNEXT 1 UIDVALIDITY 2)\r\n{tag} OK done\r\n",
                true,
            ),
            (
                "* STATUS INBOX (MESSAGES 1 UNSEEN 0 RECENT 0 UIDNEXT 2 UIDVALIDITY 2)\r\n",
                false,
            ),
            ("{tag} OK done\r\n", false),
            (
                "* STATUS Other (MESSAGES 0 UNSEEN 0 RECENT 0 UIDNEXT 1 UIDVALIDITY 2)\r\n{tag} OK done\r\n",
                false,
            ),
            (
                "* STATUS INBOX (UNSEEN 0 RECENT 0 UIDNEXT 1 UIDVALIDITY 2)\r\n{tag} OK done\r\n",
                false,
            ),
            (
                "* STATUS INBOX (MESSAGES 0 UNSEEN 0 RECENT 0 UIDNEXT 1 UIDVALIDITY 2)\r\nwrong OK done\r\n",
                false,
            ),
            ("{tag} NO unavailable\r\n", false),
            ("{tag} BAD invalid\r\n", false),
        ];
        for (reply, success) in cases {
            let (client, server) = tokio::io::duplex(2048);
            let backend = tokio::spawn(async move {
                let mut server = BufReader::new(server);
                let mut command = String::new();
                server.read_line(&mut command).await.unwrap();
                let tag = command.split_whitespace().next().unwrap();
                server
                    .get_mut()
                    .write_all(format!("{tag} OK login\r\n").as_bytes())
                    .await
                    .unwrap();
                command.clear();
                server.read_line(&mut command).await.unwrap();
                assert!(command.contains("STATUS \"INBOX\""));
                let tag = command.split_whitespace().next().unwrap();
                server
                    .get_mut()
                    .write_all(reply.replace("{tag}", tag).as_bytes())
                    .await
                    .unwrap();
            });
            let mut session = async_imap::Client::new(client)
                .login("synthetic", "test-only")
                .await
                .unwrap();
            let result = super::status_counts(&mut session, "INBOX").await;
            assert_eq!(result.is_ok(), success, "{reply}");
            backend.await.unwrap();
        }
    }

    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    #[tokio::test]
    async fn read_search_and_status_preserve_typed_structure_limits() {
        for (text, after_continuation, statistics) in [
            ("ascii", false, false),
            ("中文", false, false),
            ("中文", true, false),
            ("", false, true),
        ] {
            let (client, server) = tokio::io::duplex(2048);
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
                if after_continuation {
                    server.write_all(b"+ ready\r\n").await.unwrap();
                    let mut literal = vec![0; text.len() + 2];
                    server.read_exact(&mut literal).await.unwrap();
                    assert_eq!(&literal[..text.len()], text.as_bytes());
                }
                server
                    .write_all(
                        b"* 1 FETCH (BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"name\" {4294967295}\r\n",
                    )
                    .await
                    .unwrap();
            });
            let mut session = async_imap::Client::new(client)
                .login("synthetic", "test-only")
                .await
                .unwrap();
            let error = if statistics {
                status_counts(&mut session, "INBOX").await.err().unwrap()
            } else {
                uid_field_search(&mut session, 1, 500, "TEXT", text)
                    .await
                    .unwrap_err()
            };
            assert_eq!(error.code, "LIMIT_EXCEEDED");
            assert_eq!(
                serde_json::to_value(error).unwrap()["details"]["limit_kind"],
                "mime_structure"
            );
            backend.await.unwrap();
        }
    }

    #[test]
    fn read_mailbox_unicode_and_wire_budgets_keep_legacy_writes_frozen() {
        for name in [
            "x".repeat(512),
            "中".repeat(512),
            "😀".repeat(512),
            "e\u{301}".repeat(256),
        ] {
            let wire = mailbox_name(&name, InputLimits::Read).unwrap();
            assert!(wire.len() <= ReadInputLimits::MAILBOX_WIRE_BYTES);
            assert_eq!(display_name(&wire).unwrap(), name);
            assert_eq!(
                mailbox_name(&name, InputLimits::LegacyWrite).is_ok(),
                name.len() <= 512
            );
        }
        for name in [
            "x".repeat(513),
            "中".repeat(513),
            "😀".repeat(513),
            "e\u{301}".repeat(256) + "e",
        ] {
            assert!(mailbox_name(&name, InputLimits::Read).is_err());
        }
        // The wire ceiling is separate; currently the character ceiling is tighter.
        assert!(validate_mailbox_wire_bytes(8192).is_ok());
        assert!(validate_mailbox_wire_bytes(8193).is_err());
        assert!(display_name(&"x".repeat(8193)).is_err());
    }

    #[tokio::test]
    async fn read_search_transmits_full_unicode_literal_at_character_boundary() {
        for text in [
            "x".repeat(512),
            "中".repeat(512),
            "😀".repeat(512),
            "e\u{301}".repeat(256),
        ] {
            let (client, server) = tokio::io::duplex(16384);
            let expected = text.clone();
            let backend = tokio::spawn(async move {
                let mut server = BufReader::new(server);
                let mut line = String::new();
                server.read_line(&mut line).await.unwrap();
                let tag = line.split_whitespace().next().unwrap().to_owned();
                server
                    .write_all(format!("{tag} OK login\r\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
                server.read_line(&mut line).await.unwrap();
                let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
                let tag = tag.to_owned();
                if expected.is_ascii() {
                    assert_eq!(command, format!("UID SEARCH UID 1:500 TEXT \"{expected}\""));
                } else {
                    assert_eq!(
                        command,
                        format!(
                            "UID SEARCH CHARSET UTF-8 UID 1:500 TEXT {{{}}}",
                            expected.len()
                        )
                    );
                    server.write_all(b"+ ready\r\n").await.unwrap();
                    let mut bytes = vec![0; expected.len() + 2];
                    server.read_exact(&mut bytes).await.unwrap();
                    assert_eq!(bytes, format!("{expected}\r\n").as_bytes());
                }
                server
                    .write_all(format!("* SEARCH 7\r\n{tag} OK done\r\n").as_bytes())
                    .await
                    .unwrap();
            });
            let mut session = async_imap::Client::new(client)
                .login("synthetic", "test-only")
                .await
                .unwrap();
            assert_eq!(
                uid_text_search(&mut session, 1, 500, &text).await.unwrap(),
                HashSet::from([7])
            );
            // Rejection happens without another protocol command.
            assert_eq!(
                uid_text_search(&mut session, 1, 500, &(text + "x"))
                    .await
                    .unwrap_err()
                    .code,
                "INVALID_INPUT"
            );
            backend.await.unwrap();
        }
    }

    #[test]
    fn rev1_names_round_trip_and_reject_ambiguous_wire_names() {
        for (name, wire) in [
            ("INBOX", "INBOX"),
            ("Projects & Reports", "Projects &- Reports"),
            ("项目", "&mHl27g-"),
            ("😀", "&2D3eAA-"),
            ("~peter/mail/台北/日本語", "~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
        ] {
            assert_eq!(mailbox_name(name, InputLimits::Read).unwrap(), wire);
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
        assert!(mailbox_name("bad\r\nname", InputLimits::Read).is_err());
        assert!(mailbox_name(&"x".repeat(513), InputLimits::Read).is_err());
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
