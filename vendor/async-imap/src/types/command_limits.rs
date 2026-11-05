/// Response limits retained until this command's matching tagged completion.
/// Default command methods retain their existing response budgets.
#[derive(Clone, Copy, Debug)]
pub struct CommandReadLimits(pub(crate) CommandLimit);

#[derive(Clone, Copy, Debug)]
pub(crate) enum CommandLimit {
    BodySectionLiteralBytes(usize),
    Search {
        response_bytes: usize,
        uids: usize,
    },
    MailboxMetadata {
        response_bytes: usize,
        header_bytes: usize,
    },
    Condstore,
}

impl CommandReadLimits {
    /// Maximum combined wire bytes for a bounded SEARCH command.
    pub const MAX_SEARCH_RESPONSE_BYTES: usize = imap_proto::bounded::MAX_SEARCH_RESPONSE_BYTES;

    /// Cap each BODY section literal before the parser requests its payload.
    /// The cap applies regardless of returned UID, section or offset.
    pub fn body_section_literal_bytes(max_bytes: usize) -> Self {
        Self(CommandLimit::BodySectionLiteralBytes(max_bytes))
    }

    /// Limit all responses through the SEARCH completion, including untagged
    /// notifications. The hard ceilings are 128 KiB and 200 SEARCH entries.
    pub fn search(max_response_bytes: usize, max_uids: usize) -> Self {
        Self(CommandLimit::Search {
            response_bytes: max_response_bytes.min(Self::MAX_SEARCH_RESPONSE_BYTES),
            uids: max_uids.min(imap_proto::bounded::MAX_SEARCH_UIDS),
        })
    }

    /// Bound a metadata-only FETCH command and its unsolicited notifications.
    /// Header literals also have an independent per-literal cap.
    pub fn mailbox_metadata(max_response_bytes: usize, max_header_bytes: usize) -> Self {
        Self(CommandLimit::MailboxMetadata {
            response_bytes: max_response_bytes.min(2 * 1024 * 1024),
            header_bytes: max_header_bytes.min(128 * 1024 + 1),
        })
    }

    /// Use the bounded CONDSTORE response grammar and a 128 KiB wire budget.
    pub fn condstore() -> Self {
        Self(CommandLimit::Condstore)
    }
}
