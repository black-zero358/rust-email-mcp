//! Complete targeted FETCH evidence for discovery and known-UID reconciliation.
use super::{
    super::{conversation, insights::HEADER_LIMIT, map_read_imap_error, persistence::index_text},
    scan::Selected,
};
use crate::{
    cache::{
        Cache, ThreadId,
        sync_state::{
            ContentLayer, ContentOutcome, ContentWork, KnownPage, ReconciledFlags, ScanStamp,
            ScanState, SyncError, SyncFacts,
        },
    },
    error::{MailError, Result},
    mime,
};
use async_imap::{
    imap_proto::{
        AttributeValue, MessageSection, Response, SectionPath, Status, bounded::MAX_SEARCH_UIDS,
    },
    types::{CommandReadLimits, Fetch, Flag},
};
use tokio::io::{AsyncRead, AsyncWrite};

const MAX_FACT_UIDS: usize = 8;
const MAX_FACT_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_FLAGS_RESPONSE: usize = 128 * 1024;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Mode {
    Flags,
    Facts,
}

#[derive(Debug)]
pub(super) struct Header {
    pub(super) bytes: Box<[u8]>,
    pub(super) complete: bool,
}

#[derive(Debug)]
pub(super) struct Observation {
    pub(super) uid: u32,
    pub(super) flags: Vec<String>,
    pub(super) header: Option<Header>,
    pub(super) internal_date: Option<String>,
    pub(super) size: Option<u32>,
}

#[derive(Debug)]
pub(super) struct Proof {
    selected: Selected,
    pub(super) present: Vec<Observation>,
    pub(super) absent: Vec<u32>,
}

impl Proof {
    pub(super) fn empty_reconciliation(selected: &Selected, page: &KnownPage) -> Result<Self> {
        if !page.uids.is_empty() || !page.complete {
            return Err(MailError::backend());
        }
        Ok(Self {
            selected: selected.clone(),
            present: Vec::new(),
            absent: Vec::new(),
        })
    }

    pub(super) fn commit_facts(
        &self,
        cache: &Cache,
        work: &ContentWork,
        checked_at_ms: i64,
        cancelled: &dyn Fn() -> bool,
    ) -> std::result::Result<(), SyncError> {
        if work.layer != ContentLayer::Facts || !self.selected.matches_work(work) {
            return Err(SyncError::Invalid);
        }
        if self.absent.binary_search(&work.uid).is_ok() {
            return cache.commit_sync_content(work, ContentOutcome::MissingRemote, cancelled);
        }
        let index = self
            .present
            .binary_search_by_key(&work.uid, |item| item.uid)
            .map_err(|_| SyncError::Invalid)?;
        let item = &self.present[index];
        let header = item.header.as_ref().ok_or(SyncError::Invalid)?;
        let parsed = header
            .complete
            .then(|| mime::headers(&header.bytes).ok())
            .flatten();
        let index = parsed.as_ref().and_then(|headers| index_text(headers));
        let ids = parsed
            .as_ref()
            .and_then(|headers| conversation::thread_index_ids(headers).ok());
        let thread_ids = ids.as_ref().map(|ids| {
            ids.iter()
                .map(|(role, id)| ThreadId { role: *role, id })
                .collect::<Vec<_>>()
        });
        cache.commit_sync_facts(
            work,
            SyncFacts {
                header: &header.bytes,
                complete: parsed.is_some(),
                index_text: index.as_deref(),
                thread_ids: thread_ids.as_deref(),
                flags: &item.flags,
                internal_date: item.internal_date.as_deref().ok_or(SyncError::Invalid)?,
                size: item.size.ok_or(SyncError::Invalid)?,
            },
            checked_at_ms,
            cancelled,
        )
    }

    pub(super) fn commit_reconcile(
        &self,
        cache: &Cache,
        stamp: &ScanStamp,
        page: &KnownPage,
        checked_at_ms: i64,
        cancelled: &dyn Fn() -> bool,
    ) -> std::result::Result<ScanState, SyncError> {
        if !self.selected.matches_stamp(stamp) {
            return Err(SyncError::Invalid);
        }
        let mut evidence = self
            .present
            .iter()
            .map(|item| item.uid)
            .chain(self.absent.iter().copied())
            .collect::<Vec<_>>();
        evidence.sort_unstable();
        if evidence != page.uids {
            return Err(SyncError::Invalid);
        }
        let present = self
            .present
            .iter()
            .map(|item| ReconciledFlags {
                uid: item.uid,
                flags: &item.flags,
            })
            .collect::<Vec<_>>();
        cache.commit_sync_reconcile(stamp, page, &present, checked_at_ms, cancelled)
    }
}

#[derive(Default)]
struct Item {
    seen: bool,
    flags: Option<Vec<String>>,
    header: Option<Header>,
    internal_date: Option<String>,
    size: Option<u32>,
}

pub(super) async fn collect<S>(
    session: &mut async_imap::Session<S>,
    selected: &Selected,
    expected: &[u32],
    mode: Mode,
) -> Result<Proof>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let (max_uids, bytes, header_bytes) = match mode {
        Mode::Flags => (MAX_SEARCH_UIDS, MAX_FLAGS_RESPONSE, 0),
        Mode::Facts => (MAX_FACT_UIDS, MAX_FACT_RESPONSE, HEADER_LIMIT + 1),
    };
    if expected.is_empty()
        || expected.len() > max_uids
        || expected[0] == 0
        || expected[expected.len() - 1] >= selected.uid_next
        || expected.windows(2).any(|uids| uids[0] >= uids[1])
    {
        return Err(MailError::backend());
    }
    let mut set = expected[0].to_string();
    for uid in &expected[1..] {
        use std::fmt::Write;
        write!(set, ",{uid}").map_err(|_| MailError::backend())?;
    }
    let attributes = if mode == Mode::Facts {
        format!("UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER]<0.{header_bytes}>")
    } else {
        "UID FLAGS".into()
    };
    let tag = session
        .run_command_with_limits(
            format!("UID FETCH {set} ({attributes})"),
            CommandReadLimits::mailbox_metadata(bytes, header_bytes),
        )
        .await
        .map_err(map_read_imap_error)?;
    let mut items = Vec::with_capacity(expected.len());
    items.resize_with(expected.len(), Item::default);
    let mut unidentified_update = false;
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|error| map_read_imap_error(error.into()))?
            .ok_or_else(MailError::backend)?;
        match response.parsed() {
            Response::Done {
                tag: received,
                status: Status::Ok,
                ..
            } if received == &tag => break,
            Response::Done { .. }
            | Response::Continue(_)
            | Response::Expunge(_)
            | Response::Data {
                status: Status::Bye,
                ..
            } => {
                return Err(MailError::backend());
            }
            Response::Fetch(_, attributes) => {
                let mut ids = attributes.iter().filter_map(|attribute| match attribute {
                    AttributeValue::Uid(uid) => Some(*uid),
                    _ => None,
                });
                let uid = ids.next();
                if ids.next().is_some() || uid == Some(0) {
                    return Err(MailError::backend());
                }
                if let Some(uid) = uid {
                    if let Ok(index) = expected.binary_search(&uid) {
                        items[index].merge(attributes, mode)?;
                    } else if !attributes.iter().all(|attribute| {
                        matches!(
                            attribute,
                            AttributeValue::Uid(_)
                                | AttributeValue::Flags(_)
                                | AttributeValue::ModSeq(_)
                        )
                    }) {
                        return Err(MailError::backend());
                    }
                } else if attributes.iter().all(|attribute| {
                    matches!(
                        attribute,
                        AttributeValue::Flags(_) | AttributeValue::ModSeq(_)
                    )
                }) {
                    unidentified_update = true;
                } else {
                    return Err(MailError::backend());
                }
            }
            _ => {}
        }
    }
    let mut present = Vec::with_capacity(expected.len());
    let mut absent = Vec::new();
    for (uid, item) in expected.iter().copied().zip(items) {
        if !item.seen {
            if unidentified_update {
                // A legal unsolicited FLAGS update may omit UID. Do not
                // classify it as a different message to prove a deletion.
                return Err(MailError::backend());
            }
            absent.push(uid);
            continue;
        }
        if mode == Mode::Facts
            && (item.header.is_none() || item.size.is_none() || item.internal_date.is_none())
        {
            return Err(MailError::backend());
        }
        present.push(Observation {
            uid,
            flags: item.flags.ok_or_else(MailError::backend)?,
            header: item.header,
            internal_date: item.internal_date,
            size: item.size,
        });
    }
    Ok(Proof {
        selected: selected.clone(),
        present,
        absent,
    })
}

impl Item {
    fn merge(&mut self, attributes: &[AttributeValue<'_>], mode: Mode) -> Result<()> {
        self.seen = true;
        let mut flags = None;
        for attribute in attributes {
            match attribute {
                AttributeValue::Flags(raw) => {
                    if flags.is_some() || raw.len() > 128 {
                        return Err(MailError::backend());
                    }
                    let mut total = 0;
                    let mut values = Vec::with_capacity(raw.len());
                    for value in raw {
                        if value.is_empty()
                            || value.len() > 256
                            || value.chars().any(char::is_control)
                        {
                            return Err(MailError::backend());
                        }
                        let value = format!("{:?}", Flag::from(value.as_ref()));
                        total += value.len();
                        if value.len() > 256 || total > 3072 {
                            return Err(MailError::backend());
                        }
                        values.push(value);
                    }
                    flags = Some(values);
                }
                AttributeValue::InternalDate(value) if mode == Mode::Facts => {
                    if self.internal_date.is_some() {
                        return Err(MailError::backend());
                    }
                    let date = Fetch::parse_internal_date(value).ok_or_else(MailError::backend)?;
                    self.internal_date = Some(date.to_string());
                }
                AttributeValue::Rfc822Size(size) if mode == Mode::Facts => {
                    if self.size.replace(*size).is_some() {
                        return Err(MailError::backend());
                    }
                }
                AttributeValue::BodySection {
                    section: Some(SectionPath::Full(MessageSection::Header)),
                    index,
                    data,
                } if mode == Mode::Facts => {
                    let bytes = data.as_deref().ok_or_else(MailError::backend)?;
                    if *index != Some(0) || self.header.is_some() || bytes.len() > HEADER_LIMIT + 1
                    {
                        return Err(MailError::backend());
                    }
                    self.header = Some(Header {
                        bytes: bytes[..bytes.len().min(HEADER_LIMIT)].into(),
                        complete: bytes.len() <= HEADER_LIMIT,
                    });
                }
                _ => {}
            }
        }
        if let Some(flags) = flags {
            // A later explicit FLAGS update, including FLAGS (), wins.
            self.flags = Some(flags);
        }
        Ok(())
    }
}
