//! Stable attachment references and bounded display metadata; IDs grant no access.
use super::*;
use crate::error::InputViolation;

pub(super) struct AttachmentReference<'a> {
    pub alias: &'a str,
    pub account: &'a AccountKey,
    pub mailbox: &'a str,
    pub uid: u32,
    pub validity: u32,
}
impl AttachmentReference<'_> {
    pub fn id(&self, index: usize) -> String {
        let mut hash = Sha256::new();
        hash.update(b"email-mcp-attachment-reference-v1\0");
        hash.update(self.account.reference_namespace());
        hash.update((self.mailbox.len() as u64).to_be_bytes());
        hash.update(self.mailbox.as_bytes());
        hash.update(self.validity.to_be_bytes());
        hash.update(self.uid.to_be_bytes());
        hash.update((index as u32).to_be_bytes());
        hash.update(MIME_REFERENCE_VERSION.to_be_bytes());
        format!("att_v1_{:x}", hash.finalize())
    }
}

pub(super) fn ensure_account(args: &Value, alias: &str) -> Result<()> {
    if args.get("account_id").is_some_and(|value| value != alias) {
        return Err(MailError::account_unavailable());
    }
    Ok(())
}

pub(super) fn select_part(
    parts: &[Part],
    args: &Value,
    reference: &AttachmentReference<'_>,
) -> Result<usize> {
    ensure_account(args, reference.alias)?;
    let id = match args.get("attachment_id") {
        None => None,
        Some(Value::String(id))
            if id.strip_prefix("att_v1_").is_some_and(|suffix| {
                suffix.len() == 64
                    && suffix
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) =>
        {
            Some(id.as_str())
        }
        _ => {
            return Err(MailError::input_field(InputViolation::new(
                "/attachment_id",
                "invalid_attachment_id",
                "relist_attachments",
            )));
        }
    };
    let index = args
        .get("attachment_index")
        .map(|_| number(args, "attachment_index", 0, 0, MAX_ATTACHMENT_INDEX as u64))
        .transpose()?
        .map(|index| index as usize);
    if index.is_none() && id.is_none() {
        return Err(MailError::input_field(InputViolation::new(
            "",
            "attachment_reference_required",
            "provide_attachment_reference",
        )));
    }
    if let Some(index) = index {
        if let Some(id) = id
            && reference.id(index) != id
        {
            return Err(MailError::input_field(InputViolation::new(
                "/attachment_id",
                "reference_mismatch",
                "use_matching_reference",
            )));
        }
        return parts
            .get(index)
            .map(|_| index)
            .ok_or_else(attachment_missing);
    }
    parts
        .iter()
        .find(|part| Some(reference.id(part.index).as_str()) == id)
        .map(|part| part.index)
        .ok_or_else(attachment_missing)
}

pub(super) fn safe_filename(filename: Option<&str>, index: usize) -> String {
    let mut value = String::new();
    for ch in filename.unwrap_or_default().chars() {
        let ch = if ch.is_control()
            || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            '_'
        } else {
            ch
        };
        if value.len() + ch.len_utf8() > 179 {
            break;
        }
        value.push(ch);
    }
    let value = value.trim_matches([' ', '.']);
    if value.is_empty() {
        return format!("attachment-{index}.bin");
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str())
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|digit| {
                ["1", "2", "3", "4", "5", "6", "7", "8", "9", "¹", "²", "³"].contains(&digit)
            });
    if reserved {
        format!("_{value}")
    } else {
        value.to_owned()
    }
}

pub(super) fn descriptor(
    part: &Part,
    reference: &AttachmentReference<'_>,
    config: &Config,
    expires_at: Option<u64>,
) -> Value {
    let unavailable = if !part.encoding_supported {
        Some("unsupported_transfer_encoding")
    } else if part.wire_size >= config.max_message.saturating_mul(4) {
        Some("connection_bytes")
    } else if part.size_accuracy == "exact"
        && part
            .decoded_size
            .is_some_and(|size| size > config.max_message)
    {
        Some("decoded_attachment_bytes")
    } else {
        None
    };
    let mut modes = Vec::new();
    if unavailable.is_none() {
        if part
            .raw_message_size
            .is_none_or(|size| size <= config.max_message)
            && part
                .decoded_size
                .is_some_and(|size| size <= config.max_output / 4)
            && part.wire_size <= mime::attachment_input_limit(config.max_output / 4)
        {
            modes.push("inline_base64");
        }
        modes.push("authenticated_http");
    }
    json!({
        "account_id":reference.alias, "mailbox":reference.mailbox, "uid":reference.uid, "uid_validity":reference.validity,
        "attachment_id":reference.id(part.index), "attachment_index":part.index,
        "filename":part.filename, "safe_filename":safe_filename(part.filename.as_deref(), part.index),
        "content_type":part.content_type, "declared_content_type":part.content_type, "detected_content_type":null,
        "encoded_size":part.wire_size, "decoded_size":part.decoded_size, "size_accuracy":part.size_accuracy,
        "disposition":part.disposition, "content_id":part.content_id,
        "downloadable":unavailable.is_none(), "unavailable_reason":unavailable,
        "delivery":{
            "available_modes":modes, "authentication":"same_subject_bearer",
            "expires_in_seconds":HANDLE_TTL.as_secs(), "expires_at":expires_at,
            "client_delivery_required":true,
            "next_action":if unavailable.is_some() {"use_supported_attachment"} else if expires_at.is_some() {"authenticated_get_then_import_file"} else {"prepare_attachment_download"}
        },
        "checksum":null
    })
}
