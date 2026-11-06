//! Durable, generation-bound checkpoints for read-only mailbox synchronization.
use super::{
    Cache, CacheError, ListingEvidence, Namespace,
    evidence::{encode_flags, write_listing},
    metadata::mark_tombstone,
};
use crate::accounts::AccountKey;
use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

mod content;
mod delta;
mod gc;
mod status;
mod sweep;
pub use content::{
    ContentLayer, ContentOutcome, ContentWork, FailureReason, SyncFacts, SyncProgress,
};
pub use delta::{
    DeltaFlags, DeltaLane, DeltaObservation, DeltaPage, DeltaRound, DeltaStart, DeltaStatus,
    QresyncChanges, QresyncPreview,
};
pub use gc::{SyncReclaimCandidate, SyncReclaimPage, SyncReclaimPosition};
pub use status::{SyncCoverage, SyncGapCount, SyncLayerStatus, SyncScanStatus, SyncScopeStatus};
pub use sweep::{SyncGcPage, SyncGcPosition};

#[cfg(all(test, unix))]
mod tests;

const PAGE_UIDS: usize = 200;
const INITIAL_WIDTH: u32 = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ScanKind {
    History,
    Tail,
    Reconcile,
}

#[derive(Clone, Copy, Debug)]
pub struct MailboxObservation {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub checked_at_ms: i64,
}

/// Captured before EXAMINE, so an intervening completed mutation cannot
/// lend its newer revision to an older UIDNEXT observation.
pub struct ReconcileFence {
    namespace: Namespace,
    mailbox: String,
    instance: Uuid,
    restore_epoch: u64,
    dirty_revision: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanStamp {
    pub scan_id: Uuid,
    pub uid_validity: u32,
    pub high_uid: u32,
    pub revision: i64,
    pub kind: ScanKind,
    namespace: Namespace,
    mailbox: String,
    restore_epoch: u64,
    dirty_revision: i64,
}

impl ScanStamp {
    pub fn matches_scope(&self, namespace: &[u8; 32], mailbox: &str, uid_validity: u32) -> bool {
        &self.namespace == namespace && self.mailbox == mailbox && self.uid_validity == uid_validity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanState {
    pub stamp: ScanStamp,
    pub next_low: Option<u32>,
    pub width_hint: u32,
    pub known_after: u32,
    pub complete: bool,
    pub checked_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    presence_revision: i64,
    proof_presence_revision: i64,
}

impl ScanState {
    pub fn search_window(&self) -> Option<(u32, u32)> {
        let low = self.next_low?;
        Some((
            low,
            low.saturating_add(self.width_hint - 1)
                .min(self.stamp.high_uid),
        ))
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct KnownPage {
    pub uids: Vec<u32>,
    pub complete: bool,
    limit: usize,
}

pub struct ReconciledFlags<'a> {
    pub uid: u32,
    pub flags: &'a [String],
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error("Invalid synchronization evidence.")]
    Invalid,
    #[error("The synchronization checkpoint has changed.")]
    Stale,
    #[error("A mailbox mutation is active.")]
    ClaimActive,
    #[error("Discovery must reach the frozen boundary first.")]
    NotReady,
    #[error("Synchronization was cancelled.")]
    Cancelled,
    #[error("A complete mailbox reconciliation is required.")]
    ReconcileRequired,
}

impl From<rusqlite::Error> for SyncError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Cache(error.into())
    }
}

impl Cache {
    /// Begin or resume one pass after a matching tagged EXAMINE completed.
    pub fn start_sync(
        &self,
        account: &AccountKey,
        mailbox: &str,
        kind: ScanKind,
        observation: MailboxObservation,
    ) -> Result<ScanState, SyncError> {
        if kind == ScanKind::Reconcile {
            return Err(SyncError::Invalid);
        }
        self.start_sync_inner(account, mailbox, kind, observation, None)
    }

    pub fn sync_reconcile_fence(
        &self,
        account: &AccountKey,
        mailbox: &str,
    ) -> Result<ReconcileFence, SyncError> {
        if !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let (dirty_revision, claim): (i64, bool) = db.query_row(
            "SELECT coalesce((SELECT revision FROM scope_fences WHERE mailbox=?1),0),
                    EXISTS(SELECT 1 FROM mutation_scopes WHERE mailbox=?1)",
            [mailbox],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if claim {
            return Err(SyncError::ClaimActive);
        }
        Ok(ReconcileFence {
            namespace,
            mailbox: mailbox.into(),
            instance: self.instance,
            restore_epoch: self.disk.restore_epoch(),
            dirty_revision,
        })
    }

    pub fn start_sync_reconcile(
        &self,
        account: &AccountKey,
        mailbox: &str,
        observation: MailboxObservation,
        fence: &ReconcileFence,
    ) -> Result<ScanState, SyncError> {
        self.start_sync_inner(
            account,
            mailbox,
            ScanKind::Reconcile,
            observation,
            Some(fence),
        )
    }

    fn start_sync_inner(
        &self,
        account: &AccountKey,
        mailbox: &str,
        kind: ScanKind,
        observation: MailboxObservation,
        fence: Option<&ReconcileFence>,
    ) -> Result<ScanState, SyncError> {
        if !self.full
            || observation.uid_validity == 0
            || observation.uid_next == 0
            || observation.checked_at_ms < 0
        {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        if let Some(fence) = fence
            && (fence.namespace != namespace
                || fence.mailbox != mailbox
                || fence.instance != self.instance
                || fence.restore_epoch != self.disk.restore_epoch())
        {
            return Err(SyncError::Stale);
        }
        let observed_high = observation.uid_next - 1;
        self.observe_folder(
            account,
            mailbox,
            observation.uid_validity,
            Some(observed_high),
            observation.checked_at_ms,
        )?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let epoch = self.disk.restore_epoch();
        let dirty_revision = scope_fence(
            &database.connection,
            mailbox,
            observation.uid_validity,
            kind,
        )?;
        if let Some(fence) = fence
            && (fence.namespace != namespace
                || fence.mailbox != mailbox
                || fence.instance != self.instance
                || fence.restore_epoch != epoch
                || fence.dirty_revision != dirty_revision)
        {
            return Err(SyncError::Stale);
        }
        let prior = read_scan(&database.connection, namespace, mailbox, kind)?.filter(|scan| {
            scan.stamp.uid_validity == observation.uid_validity && scan.stamp.restore_epoch == epoch
        });
        if prior
            .as_ref()
            .is_some_and(|scan| scan.stamp.high_uid > observed_high)
        {
            return Err(SyncError::Stale);
        }
        let resume = prior.as_ref().filter(|scan| match kind {
            ScanKind::History => true,
            ScanKind::Tail => !scan.complete || scan.stamp.high_uid == observed_high,
            ScanKind::Reconcile => {
                !scan.complete
                    && scan.stamp.dirty_revision == dirty_revision
                    && scan.presence_revision == scan.proof_presence_revision
            }
        });
        if let Some(scan) = resume {
            if scan.stamp.dirty_revision == dirty_revision {
                return Ok(scan.clone());
            }
            return self.write(namespace, database, |tx| {
                tx.execute("UPDATE sync_checkpoints SET dirty_revision=?3,revision=revision+1 WHERE mailbox=?1 AND kind=?2",
                    params![mailbox, kind as u8, dirty_revision])?;
                read_scan(tx, namespace, mailbox, kind)?.ok_or(SyncError::Stale)
            });
        }
        let high = prior
            .as_ref()
            .filter(|scan| {
                kind == ScanKind::Reconcile
                    && !scan.complete
                    && scan.stamp.dirty_revision == dirty_revision
            })
            .map_or(observed_high, |scan| scan.stamp.high_uid);
        let first = match kind {
            ScanKind::History => Some(1),
            ScanKind::Tail => {
                let previous_high = if let Some(scan) = &prior {
                    scan.stamp.high_uid
                } else {
                    read_scan(&database.connection, namespace, mailbox, ScanKind::History)?
                        .filter(|scan| {
                            scan.stamp.uid_validity == observation.uid_validity
                                && scan.stamp.restore_epoch == epoch
                        })
                        .ok_or(SyncError::NotReady)?
                        .stamp
                        .high_uid
                };
                Some(previous_high + 1)
            }
            ScanKind::Reconcile => None,
        }
        .filter(|&uid| uid <= high);
        let complete = kind != ScanKind::Reconcile && first.is_none();
        self.write(namespace, database, |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO sync_checkpoints VALUES(?1,?2,?3,?4,?5,1,?6,?7,0,0,?8,?9,0,?10,?11)",
                params![mailbox, kind as u8, Uuid::new_v4().as_bytes(), observation.uid_validity,
                    high, epoch as i64, dirty_revision, first, INITIAL_WIDTH, observation.checked_at_ms,
                    complete.then_some(observation.checked_at_ms)],
            )?;
            read_scan(tx, namespace, mailbox, kind)?.ok_or(SyncError::Stale)
        })
    }

    pub fn sync_status(
        &self,
        account: &AccountKey,
        mailbox: &str,
    ) -> Result<Vec<ScanState>, SyncError> {
        if !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let validity: Option<u32> = db
            .query_row(
                "SELECT uid_validity FROM mailboxes WHERE mailbox=?1",
                [mailbox],
                |row| row.get(0),
            )
            .optional()?;
        let mut states = Vec::with_capacity(3);
        for kind in [ScanKind::History, ScanKind::Tail, ScanKind::Reconcile] {
            if let Some(scan) = read_scan(db, namespace, mailbox, kind)?
                && Some(scan.stamp.uid_validity) == validity
                && scan.stamp.restore_epoch == self.disk.restore_epoch()
            {
                states.push(scan);
            }
        }
        Ok(states)
    }

    /// Apply a successful bounded UID SEARCH and enqueue facts for each hit.
    pub fn record_sync_search(
        &self,
        stamp: &ScanStamp,
        low: u32,
        high: u32,
        remote: &[u32],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ScanState, SyncError> {
        if stamp.kind == ScanKind::Reconcile
            || low == 0
            || low > high
            || high > stamp.high_uid
            || remote.len() > PAGE_UIDS
            || remote.iter().any(|uid| !(low..=high).contains(uid))
        {
            return Err(SyncError::Invalid);
        }
        let mut ids = remote.to_vec();
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(SyncError::Invalid);
        }
        check_cancel(cancelled)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?;
        let state = checked_scan(self, &database.connection, stamp)?;
        if state.next_low != Some(low) {
            return Err(SyncError::Stale);
        }
        let (_, expected_high) = state.search_window().ok_or(SyncError::Stale)?;
        let narrow_high = expected_high.min(low.saturating_add(INITIAL_WIDTH - 1));
        if high != expected_high && high != narrow_high {
            return Err(SyncError::Invalid);
        }
        let next = (high < stamp.high_uid).then_some(high + 1);
        let basis = state.width_hint.min((high - low + 1).max(INITIAL_WIDTH));
        let width = if high == expected_high && remote.len() <= PAGE_UIDS / 4 {
            basis.saturating_mul(2)
        } else {
            basis
        };
        self.write_checked(stamp.namespace, database, |tx| {
            for &uid in &ids {
                let id: Option<i64> = tx.query_row(
                    "SELECT id FROM messages WHERE mailbox=?1 AND uid_validity=?2 AND uid=?3 AND tombstone",
                    params![stamp.mailbox, stamp.uid_validity, uid], |row| row.get(0),
                ).optional()?;
                if id.is_some() { continue; }
                tx.execute("INSERT OR IGNORE INTO messages(mailbox,uid_validity,uid) VALUES(?1,?2,?3)",
                    params![stamp.mailbox, stamp.uid_validity, uid])?;
                tx.execute("INSERT OR IGNORE INTO sync_work(mailbox,uid_validity,uid,layer,part,state) VALUES(?1,?2,?3,0,0,0)",
                    params![stamp.mailbox, stamp.uid_validity, uid])?;
            }
            tx.execute("UPDATE sync_checkpoints SET next_low=?3,width_hint=?4,revision=revision+1,
                        completed_at_ms=CASE WHEN ?3 IS NULL THEN checked_at_ms END WHERE mailbox=?1 AND kind=?2",
                params![stamp.mailbox, stamp.kind as u8, next, width])?;
            read_scan(tx, stamp.namespace, &stamp.mailbox, stamp.kind)?.ok_or(SyncError::Stale)
        }, || check_cancel(cancelled))
    }

    pub fn resize_sync_search(
        &self,
        stamp: &ScanStamp,
        width_hint: u32,
    ) -> Result<ScanState, SyncError> {
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?;
        let state = checked_scan(self, &database.connection, stamp)?;
        if state.next_low.is_none() || width_hint == 0 || width_hint >= state.width_hint {
            return Err(SyncError::Invalid);
        }
        self.write(stamp.namespace, database, |tx| {
            tx.execute("UPDATE sync_checkpoints SET width_hint=?3,revision=revision+1 WHERE mailbox=?1 AND kind=?2",
                params![stamp.mailbox, stamp.kind as u8, width_hint])?;
            read_scan(tx, stamp.namespace, &stamp.mailbox, stamp.kind)?.ok_or(SyncError::Stale)
        })
    }

    pub fn sync_known_page(&self, stamp: &ScanStamp, limit: usize) -> Result<KnownPage, SyncError> {
        if !(1..=PAGE_UIDS).contains(&limit) {
            return Err(SyncError::Invalid);
        }
        let databases = self.lock()?;
        let db = &databases
            .get(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let state = checked_scan(self, db, stamp)?;
        known_page(db, &state, limit)
    }

    /// Apply one strict exact-UID FLAGS command after its matching tagged OK.
    pub fn commit_sync_reconcile(
        &self,
        stamp: &ScanStamp,
        page: &KnownPage,
        present: &[ReconciledFlags<'_>],
        checked_at_ms: i64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ScanState, SyncError> {
        check_cancel(cancelled)?;
        if present.len() > PAGE_UIDS {
            return Err(SyncError::Invalid);
        }
        let mut facts = std::collections::BTreeMap::new();
        for fact in present {
            if page.uids.binary_search(&fact.uid).is_err()
                || facts.insert(fact.uid, encode_flags(fact.flags)?).is_some()
            {
                return Err(SyncError::Invalid);
            }
        }
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?;
        let state = checked_scan(self, &database.connection, stamp)?;
        if checked_at_ms < state.checked_at_ms
            || *page != known_page(&database.connection, &state, page.limit)?
        {
            return Err(SyncError::Stale);
        }
        self.write_checked(stamp.namespace, database, |tx| {
            for &uid in &page.uids {
                let id: i64 = tx.query_row("SELECT id FROM messages WHERE mailbox=?1 AND uid_validity=?2 AND uid=?3 AND NOT tombstone",
                    params![stamp.mailbox, stamp.uid_validity, uid], |row| row.get(0))?;
                if let Some(flags) = facts.get(&uid) {
                    write_listing(tx, id, ListingEvidence { flags: None, internal_date: None, size: None, observed_at_ms: checked_at_ms }, Some(flags))?;
                } else {
                    mark_tombstone(tx, id, true)?;
                }
            }
            if page.complete {
                tx.execute("UPDATE scope_fences SET dirty=0 WHERE mailbox=?1", [&stamp.mailbox])?;
                tx.execute("UPDATE mailboxes SET dirty=0 WHERE mailbox=?1 AND uid_validity=?2", params![stamp.mailbox, stamp.uid_validity])?;
                delta::finish_seed(tx, stamp, checked_at_ms)?;
            }
            tx.execute("UPDATE sync_checkpoints SET known_after=?2,completed_at_ms=?3,revision=revision+1,
                        proof_presence_revision=presence_revision WHERE mailbox=?1 AND kind=2",
                params![stamp.mailbox, page.uids.last().copied().unwrap_or(state.known_after), page.complete.then_some(checked_at_ms)])?;
            read_scan(tx, stamp.namespace, &stamp.mailbox, stamp.kind)?.ok_or(SyncError::Stale)
        }, || check_cancel(cancelled))
    }
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<(), SyncError> {
    if cancelled() {
        Err(SyncError::Cancelled)
    } else {
        Ok(())
    }
}

fn read_scan(
    db: &Connection,
    namespace: Namespace,
    mailbox: &str,
    kind: ScanKind,
) -> Result<Option<ScanState>, SyncError> {
    let scan = db.query_row(
        "SELECT scan_id,uid_validity,high_uid,revision,restore_epoch,dirty_revision,
                next_low,width_hint,known_after,checked_at_ms,completed_at_ms,presence_revision,proof_presence_revision
         FROM sync_checkpoints WHERE mailbox=?1 AND kind=?2",
        params![mailbox, kind as u8],
        |row: &Row<'_>| {
            let completed_at_ms: Option<i64> = row.get(10)?;
            Ok(ScanState {
                stamp: ScanStamp { scan_id: Uuid::from_bytes(row.get(0)?), uid_validity: row.get(1)?,
                    high_uid: row.get(2)?, revision: row.get(3)?, kind, namespace, mailbox: mailbox.into(),
                    restore_epoch: row.get::<_, i64>(4)? as u64, dirty_revision: row.get(5)? },
                next_low: row.get(6)?, width_hint: row.get(7)?, known_after: row.get(8)?,
                checked_at_ms: row.get(9)?, complete: completed_at_ms.is_some(), completed_at_ms,
                presence_revision: row.get(11)?, proof_presence_revision: row.get(12)?,
            })
        },
    ).optional()?;
    Ok(scan)
}

fn scope_fence(
    db: &Connection,
    mailbox: &str,
    uid_validity: u32,
    kind: ScanKind,
) -> Result<i64, SyncError> {
    let (revision, claim): (i64, bool) = db.query_row(
        "SELECT coalesce(s.revision,0),EXISTS(SELECT 1 FROM mutation_scopes WHERE mailbox=?1)
         FROM mailboxes m LEFT JOIN scope_fences s USING(mailbox) WHERE m.mailbox=?1 AND m.uid_validity=?2",
        params![mailbox, uid_validity], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?.ok_or(SyncError::Stale)?;
    if claim && kind == ScanKind::Reconcile {
        return Err(SyncError::ClaimActive);
    }
    Ok(revision)
}

fn checked_scan(cache: &Cache, db: &Connection, stamp: &ScanStamp) -> Result<ScanState, SyncError> {
    if !cache.namespace_has_folder(&stamp.namespace, &stamp.mailbox) {
        return Err(CacheError::OutOfScope.into());
    }
    if !cache.full
        || cache.disk.restore_epoch() != stamp.restore_epoch
        || scope_fence(db, &stamp.mailbox, stamp.uid_validity, stamp.kind)? != stamp.dirty_revision
    {
        return Err(SyncError::Stale);
    }
    let scan =
        read_scan(db, stamp.namespace, &stamp.mailbox, stamp.kind)?.ok_or(SyncError::Stale)?;
    if scan.stamp != *stamp
        || (stamp.kind == ScanKind::Reconcile
            && scan.presence_revision != scan.proof_presence_revision)
    {
        return Err(SyncError::Stale);
    }
    Ok(scan)
}

fn known_page(db: &Connection, state: &ScanState, limit: usize) -> Result<KnownPage, SyncError> {
    let stamp = &state.stamp;
    if stamp.kind != ScanKind::Reconcile || state.complete {
        return Err(SyncError::Stale);
    }
    let mut covered = 0;
    for kind in [ScanKind::History, ScanKind::Tail] {
        let Some(scan) = read_scan(db, stamp.namespace, &stamp.mailbox, kind)?.filter(|scan| {
            scan.stamp.uid_validity == stamp.uid_validity
                && scan.stamp.restore_epoch == stamp.restore_epoch
        }) else {
            break;
        };
        covered = scan.next_low.map_or(scan.stamp.high_uid, |uid| uid - 1);
        if covered >= stamp.high_uid || !scan.complete {
            break;
        }
    }
    if covered < stamp.high_uid {
        return Err(SyncError::NotReady);
    }
    let mut statement = db.prepare_cached(
        "SELECT uid FROM messages WHERE mailbox=?1 AND uid_validity=?2 AND NOT tombstone
             AND uid>?3 AND uid<=?4 ORDER BY uid LIMIT ?5",
    )?;
    let mut uids = statement
        .query_map(
            params![
                stamp.mailbox,
                stamp.uid_validity,
                state.known_after,
                stamp.high_uid,
                (limit + 1) as u32
            ],
            |row| row.get(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let complete = uids.len() <= limit;
    uids.truncate(limit);
    Ok(KnownPage {
        uids,
        complete,
        limit,
    })
}
