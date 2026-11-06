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

    use super::*;

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
}
