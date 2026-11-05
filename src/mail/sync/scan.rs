//! Frozen UID boundaries and one bounded discovery window.
use super::super::{InputLimits, mailbox_name, map_read_imap_error};
use crate::{
    accounts::AccountKey,
    cache::{
        Cache,
        sync_state::{ContentWork, MailboxObservation, ScanStamp, ScanState, SyncError},
    },
    error::{MailError, Result},
};
use async_imap::{
    error::search_limit,
    imap_proto::bounded::{MAX_SEARCH_UIDS, SearchLimitKind},
    types::Mailbox,
};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Clone, Debug)]
pub(super) struct Selected {
    namespace: [u8; 32],
    pub(super) mailbox: Box<str>,
    pub(super) uid_validity: u32,
    pub(super) uid_next: u32,
}

impl Selected {
    pub(super) fn from_mailbox(
        account: &AccountKey,
        mailbox: &str,
        state: &Mailbox,
    ) -> Result<Self> {
        let valid = |value: Option<u32>| {
            value
                .filter(|value| *value != 0)
                .ok_or_else(MailError::backend)
        };
        Ok(Self {
            namespace: account.reference_namespace(),
            mailbox: mailbox.into(),
            uid_validity: valid(state.uid_validity)?,
            uid_next: valid(state.uid_next)?,
        })
    }

    pub(super) fn high_uid(&self) -> u32 {
        self.uid_next - 1
    }

    pub(super) fn observation(&self, checked_at_ms: i64) -> MailboxObservation {
        MailboxObservation {
            uid_validity: self.uid_validity,
            uid_next: self.uid_next,
            checked_at_ms,
        }
    }

    pub(super) fn matches_account(&self, account: &AccountKey) -> bool {
        self.namespace == account.reference_namespace()
    }

    pub(super) fn matches_stamp(&self, stamp: &ScanStamp) -> bool {
        stamp.matches_scope(&self.namespace, &self.mailbox, self.uid_validity)
            && stamp.high_uid <= self.high_uid()
    }

    pub(super) fn matches_work(&self, work: &ContentWork) -> bool {
        work.matches_scope(&self.namespace, &self.mailbox, self.uid_validity)
            && (1..self.uid_next).contains(&work.uid)
    }

    pub(super) async fn examine<S>(
        session: &mut async_imap::Session<S>,
        account: &AccountKey,
        mailbox: &str,
    ) -> Result<(Self, Mailbox)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let wire = mailbox_name(mailbox, InputLimits::Read)?;
        let state = session.examine(wire).await.map_err(map_read_imap_error)?;
        Ok((Self::from_mailbox(account, mailbox, &state)?, state))
    }

    pub(super) fn require(&self, uid_validity: u32, frozen_high: u32) -> Result<()> {
        super::super::check_validity(
            &serde_json::json!({"uid_validity":uid_validity}),
            Some(self.uid_validity),
        )?;
        if self.high_uid() < frozen_high {
            return Err(MailError::backend());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Window {
    pub(super) low: u32,
    pub(super) high: u32,
}

impl Window {
    pub(super) fn from_width(low: u32, frozen_high: u32, width: u32) -> Option<Self> {
        if low == 0 || low > frozen_high || width == 0 {
            return None;
        }
        let high = (u64::from(low) + u64::from(width) - 1).min(u64::from(frozen_high));
        Some(Self {
            low,
            high: high as u32,
        })
    }

    pub(super) fn narrow(self) -> Option<Self> {
        let smaller = Self::from_width(self.low, self.high, MAX_SEARCH_UIDS as u32)?;
        (smaller != self).then_some(smaller)
    }
}

#[derive(Debug)]
pub(super) enum SearchError {
    TooManyUids,
    Failed(MailError),
}

pub(super) async fn search<S>(
    session: &mut async_imap::Session<S>,
    window: Window,
) -> std::result::Result<Vec<u32>, SearchError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    if window.low == 0 || window.high < window.low {
        return Err(SearchError::Failed(MailError::backend()));
    }
    let query = format!("UID {}:{}", window.low, window.high);
    let mut uids = match session.uid_search_bounded(query, MAX_SEARCH_UIDS).await {
        Ok(uids) => uids,
        Err(error)
            if search_limit(&error)
                .is_some_and(|limit| limit.kind == SearchLimitKind::UidCount)
                && window.narrow().is_some() =>
        {
            return Err(SearchError::TooManyUids);
        }
        Err(error) => return Err(SearchError::Failed(map_read_imap_error(error))),
    };
    if uids
        .iter()
        .any(|uid| !(window.low..=window.high).contains(uid))
    {
        return Err(SearchError::Failed(MailError::backend()));
    }
    uids.sort_unstable();
    Ok(uids)
}

#[derive(Debug)]
pub(super) struct SearchReceipt {
    selected: Selected,
    frozen_high: u32,
    pub(super) window: Window,
    pub(super) uids: Vec<u32>,
}

impl SearchReceipt {
    pub(super) fn commit(
        self,
        cache: &Cache,
        stamp: &ScanStamp,
        cancelled: &dyn Fn() -> bool,
    ) -> std::result::Result<ScanState, SyncError> {
        if stamp.high_uid != self.frozen_high || !self.selected.matches_stamp(stamp) {
            return Err(SyncError::Invalid);
        }
        cache.record_sync_search(
            stamp,
            self.window.low,
            self.window.high,
            &self.uids,
            cancelled,
        )
    }
}

/// Own the first connection so a failed limited command can never be reused
/// for the one permitted numeric-window reduction.
pub(super) async fn discover<S, F>(
    mut session: async_imap::Session<S>,
    selected: Selected,
    uid_validity: u32,
    frozen_high: u32,
    window: Window,
    reconnect: impl FnOnce() -> F,
) -> Result<SearchReceipt>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    F: Future<Output = Result<(async_imap::Session<S>, Selected)>>,
{
    selected.require(uid_validity, frozen_high)?;
    if window.high > frozen_high {
        return Err(MailError::backend());
    }
    match search(&mut session, window).await {
        Ok(uids) => Ok(SearchReceipt {
            selected,
            frozen_high,
            window,
            uids,
        }),
        Err(SearchError::Failed(error)) => Err(error),
        Err(SearchError::TooManyUids) => {
            drop(session);
            let window = window.narrow().ok_or_else(MailError::backend)?;
            let (mut fresh, checked) = reconnect().await?;
            if checked.mailbox != selected.mailbox || checked.namespace != selected.namespace {
                return Err(MailError::backend());
            }
            checked.require(uid_validity, frozen_high)?;
            let uids = search(&mut fresh, window)
                .await
                .map_err(|error| match error {
                    SearchError::Failed(error) => error,
                    SearchError::TooManyUids => MailError::backend(),
                })?;
            Ok(SearchReceipt {
                selected: checked,
                frozen_high,
                window,
                uids,
            })
        }
    }
}
