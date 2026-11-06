//! Explicit source selection for read-only tools.
use crate::error::{MailError, Result};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Live,
    Cache,
}

pub(crate) fn source_field(tool: &str) -> Option<(&'static str, &'static str)> {
    match tool {
        "list_mailboxes"
        | "list_emails"
        | "get_email"
        | "get_emails"
        | "list_attachments"
        | "download_attachment"
        | "prepare_attachment_download"
        | "read_conversation" => Some(("read_source", "live")),
        "search_emails" | "locate_message" => Some(("search_source", "server")),
        _ => None,
    }
}

impl Source {
    pub(crate) fn parse(tool: &str, args: &Value) -> Result<Self> {
        let Some((field, default)) = source_field(tool) else {
            return Ok(Self::Live);
        };
        match args.get(field) {
            None => Ok(Self::Live),
            Some(value) => match value.as_str() {
                Some("cache") => Ok(Self::Cache),
                Some(source) if source == default => Ok(Self::Live),
                _ => Err(MailError::input()),
            },
        }
    }
}
