//! Durable, scope-bound modification sequence baselines and page receipts.
use super::*;
use crate::cache::{Database, dirty::scope_dirty};
use std::collections::BTreeMap;

const FLAGS_BUDGET: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DeltaLane {
    Flags,
    FlagsAndExpunges,
}

#[derive(Clone, Copy, Debug)]
pub struct DeltaObservation {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub highest_modseq: u64,
    pub checked_at_ms: i64,
}

impl DeltaObservation {
    fn validate(self) -> Result<i64, SyncError> {
        if self.uid_validity == 0 || self.uid_next == 0 || self.checked_at_ms < 0 {
            return Err(SyncError::Invalid);
        }
        positive(self.highest_modseq)
    }
}

#[derive(Debug)]
pub enum DeltaStart {
    NeedsBaseline,
    Current,
    Round(DeltaRound),
    Rollback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeltaRound {
    pub lane: DeltaLane,
    pub uid_validity: u32,
    pub high_uid: u32,
    pub base_modseq: u64,
    pub target_modseq: u64,
    namespace: Namespace,
    mailbox: String,
    restore_epoch: u64,
    dirty_revision: i64,
    fact_revision: i64,
    id: Uuid,
    revision: i64,
    known_after: u32,
}

impl DeltaRound {
    pub fn matches_scope(&self, namespace: &[u8; 32], mailbox: &str, uid_validity: u32) -> bool {
        &self.namespace == namespace && self.mailbox == mailbox && self.uid_validity == uid_validity
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct DeltaPage {
    pub uids: Vec<u32>,
    pub complete: bool,
    limit: usize,
    round_id: Uuid,
}

pub struct DeltaFlags<'a> {
    pub uid: u32,
    pub modseq: u64,
    pub flags: &'a [String],
}

#[derive(Debug, serde::Serialize)]
pub struct DeltaStatus {
    pub uid_validity: u32,
    pub high_uid: u32,
    pub committed_modseq: String,
    pub target_modseq: Option<String>,
    pub known_after: Option<u32>,
    pub checked_at_ms: i64,
}

struct LaneState {
    uid_validity: u32,
    restore_epoch: u64,
    dirty_revision: i64,
    high_uid: u32,
    committed: i64,
    checked_at_ms: i64,
    fact_revision: i64,
    round: Option<RoundState>,
}

struct RoundState {
    id: Uuid,
    target: i64,
    revision: i64,
    proof_revision: i64,
    known_after: u32,
}

impl LaneState {
    fn round(&self, namespace: Namespace, mailbox: &str, lane: DeltaLane) -> Option<DeltaRound> {
        let round = self
            .round
            .as_ref()
            .filter(|round| round.proof_revision == self.fact_revision)?;
        Some(DeltaRound {
            lane,
            uid_validity: self.uid_validity,
            high_uid: self.high_uid,
            base_modseq: self.committed as u64,
            target_modseq: round.target as u64,
            namespace,
            mailbox: mailbox.into(),
            restore_epoch: self.restore_epoch,
            dirty_revision: self.dirty_revision,
            fact_revision: self.fact_revision,
            id: round.id,
            revision: round.revision,
            known_after: round.known_after,
        })
    }
}

impl Cache {
    /// Pending rev1 floor, for the pre-selection rollback fence.
    pub fn pending_delta_floor(
        &self,
        account: &AccountKey,
        mailbox: &str,
    ) -> Result<Option<(u32, u64)>, SyncError> {
        if !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        Ok(db.query_row(
            "SELECT s.uid_validity,s.floor_modseq FROM sync_delta_seed s
             JOIN mailboxes m USING(mailbox,uid_validity)
             JOIN sync_checkpoints r ON r.mailbox=s.mailbox AND r.kind=2 AND r.scan_id=s.reconcile_id
             LEFT JOIN scope_fences f ON f.mailbox=s.mailbox
             WHERE s.mailbox=?1 AND s.restore_epoch=?2 AND s.floor_modseq IS NOT NULL
               AND s.dirty_revision=coalesce(f.revision,0)
               AND r.uid_validity=s.uid_validity AND r.restore_epoch=s.restore_epoch
               AND r.dirty_revision=s.dirty_revision AND r.high_uid=s.high_uid
               AND r.completed_at_ms IS NULL AND r.presence_revision=r.proof_presence_revision
               AND NOT EXISTS(SELECT 1 FROM mutation_scopes WHERE mailbox=s.mailbox)",
            params![mailbox,self.disk.restore_epoch() as i64],
            |row| Ok((row.get(0)?,row.get::<_,i64>(1)? as u64)),
        ).optional()?)
    }

    /// Register a candidate from the read-only selection preceding the first
    /// independent rev1 FLAGS page. A lower selected watermark invalidates
    /// this scan's candidate even when some pages have already committed.
    pub fn prepare_delta_seed(
        &self,
        stamp: &ScanStamp,
        observation: DeltaObservation,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        let floor = observation.validate()?;
        check_cancel(cancelled)?;
        if stamp.kind != ScanKind::Reconcile || observation.uid_validity != stamp.uid_validity {
            return Err(SyncError::Invalid);
        }
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?;
        let state = checked_scan(self, &database.connection, stamp)?;
        if let Some(prior) = read_seed_floor(&database.connection, stamp)? {
            if prior.is_some_and(|prior| floor < prior) {
                return self.revoke_seed(database, stamp, cancelled);
            }
            return Ok(());
        }
        if state.complete
            || state.known_after != 0
            || observation.checked_at_ms != state.checked_at_ms
            || observation.uid_next - 1 != stamp.high_uid
        {
            return Ok(());
        }
        let covered: bool = database.connection.query_row(
            "SELECT count(*)=2 FROM sync_delta WHERE mailbox=?1 AND uid_validity=?2
                    AND restore_epoch=?3 AND dirty_revision=?4 AND high_uid>=?5",
            params![
                stamp.mailbox,
                stamp.uid_validity,
                stamp.restore_epoch as i64,
                stamp.dirty_revision,
                stamp.high_uid
            ],
            |row| row.get(0),
        )?;
        if covered {
            return Ok(());
        }
        self.write_checked(
            stamp.namespace,
            database,
            |tx| {
                tx.execute(
                    "INSERT OR REPLACE INTO sync_delta_seed VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        stamp.mailbox,
                        stamp.uid_validity,
                        stamp.restore_epoch as i64,
                        stamp.dirty_revision,
                        stamp.scan_id.as_bytes(),
                        stamp.high_uid,
                        floor
                    ],
                )?;
                Ok(())
            },
            || check_cancel(cancelled),
        )
    }

    /// An ordinary rev1 page cannot carry forward an extension seed. Keep the
    /// blocked marker on this scan and preserve any earlier committed lanes.
    pub fn discard_delta_seed(
        &self,
        stamp: &ScanStamp,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        check_cancel(cancelled)?;
        if stamp.kind != ScanKind::Reconcile {
            return Err(SyncError::Invalid);
        }
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?;
        checked_scan(self, &database.connection, stamp)?;
        if read_seed_floor(&database.connection, stamp)?
            .flatten()
            .is_none()
        {
            return Ok(());
        }
        self.revoke_seed(database, stamp, cancelled)
    }

    fn revoke_seed(
        &self,
        database: &mut Database,
        stamp: &ScanStamp,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        self.write_checked(
            stamp.namespace,
            database,
            |tx| {
                tx.execute(
                "UPDATE sync_delta_seed SET floor_modseq=NULL WHERE mailbox=?1 AND reconcile_id=?2",
                params![stamp.mailbox,stamp.scan_id.as_bytes()],
            )?;
                Ok(())
            },
            || check_cancel(cancelled),
        )
    }

    /// A scope observation is captured after a pre-selection fence, so a
    /// finished local mutation cannot lend its new revision to older evidence.
    pub fn start_delta(
        &self,
        account: &AccountKey,
        mailbox: &str,
        lane: DeltaLane,
        observation: DeltaObservation,
        fence: &ReconcileFence,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<DeltaStart, SyncError> {
        let highest = observation.validate()?;
        check_cancel(cancelled)?;
        if lane != DeltaLane::Flags || !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let db = &database.connection;
        check_fence(
            self,
            db,
            namespace,
            mailbox,
            observation.uid_validity,
            fence,
        )?;
        let Some(state) = read_lane(db, mailbox, lane)?
            .filter(|state| matches_fence(state, observation.uid_validity, fence))
        else {
            return Ok(DeltaStart::NeedsBaseline);
        };
        if scope_dirty(db, mailbox)? {
            return Ok(DeltaStart::NeedsBaseline);
        }
        let highest_known: i64 = db.query_row(
            "SELECT max(committed_modseq) FROM sync_delta
             WHERE mailbox=?1 AND uid_validity=?2 AND restore_epoch=?3 AND dirty_revision=?4",
            params![
                mailbox,
                observation.uid_validity,
                fence.restore_epoch as i64,
                fence.dirty_revision
            ],
            |row| row.get(0),
        )?;
        if highest < highest_known {
            self.write_checked(
                namespace,
                database,
                |tx| clear_delta(tx, mailbox),
                || check_cancel(cancelled),
            )?;
            return Ok(DeltaStart::Rollback);
        }
        if observation.uid_next <= state.high_uid {
            return Err(SyncError::Stale);
        }
        if state
            .round
            .as_ref()
            .is_some_and(|round| round.target > highest)
        {
            self.write_checked(
                namespace,
                database,
                |tx| {
                    tx.execute(
                        "UPDATE sync_delta SET round_id=NULL,target_modseq=NULL,
                     proof_revision=NULL,known_after=0 WHERE mailbox=?1 AND lane=?2",
                        params![mailbox, lane as u8],
                    )?;
                    Ok(())
                },
                || check_cancel(cancelled),
            )?;
            return Ok(DeltaStart::NeedsBaseline);
        }
        if highest == state.committed {
            return Ok(DeltaStart::Current);
        }
        if let Some(round) = state.round(namespace, mailbox, lane) {
            return Ok(DeltaStart::Round(round));
        }
        self.write_checked(
            namespace,
            database,
            |tx| {
                tx.execute(
                "UPDATE sync_delta SET round_id=?3,target_modseq=?4,round_revision=round_revision+1,
                 proof_revision=fact_revision,known_after=0 WHERE mailbox=?1 AND lane=?2",
                params![mailbox, lane as u8, Uuid::new_v4().as_bytes(), highest],
            )?;
                Ok(DeltaStart::Round(
                    read_lane(tx, mailbox, lane)?
                        .and_then(|state| state.round(namespace, mailbox, lane))
                        .ok_or(CacheError::Corrupt)?,
                ))
            },
            || check_cancel(cancelled),
        )
    }

    pub fn delta_page(&self, round: &DeltaRound, limit: usize) -> Result<DeltaPage, SyncError> {
        if !(1..=PAGE_UIDS).contains(&limit) {
            return Err(SyncError::Invalid);
        }
        let databases = self.lock()?;
        let db = &databases
            .get(&round.namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        check_round(self, db, round)?;
        known_delta_page(db, round, limit)
    }

    /// Only explicit UID/FLAGS/MODSEQ facts can modify listing evidence.
    /// Missing rows in CHANGEDSINCE carry no existence information.
    pub fn commit_delta_page(
        &self,
        round: &DeltaRound,
        page: &DeltaPage,
        observation: DeltaObservation,
        changed: &[DeltaFlags<'_>],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<DeltaRound>, SyncError> {
        let highest = observation.validate()?;
        check_cancel(cancelled)?;
        if round.lane != DeltaLane::Flags
            || observation.uid_validity != round.uid_validity
            || observation.uid_next <= round.high_uid
            || (highest as u64) < round.target_modseq
            || changed.len() > PAGE_UIDS
        {
            return Err(SyncError::Invalid);
        }
        let mut flags = BTreeMap::new();
        let mut encoded_bytes = 0;
        for fact in changed {
            positive(fact.modseq)?;
            if fact.modseq <= round.base_modseq
                || page.uids.binary_search(&fact.uid).is_err()
                || flags.contains_key(&fact.uid)
            {
                return Err(SyncError::Invalid);
            }
            let value = encode_flags(fact.flags)?;
            encoded_bytes += value.len();
            if encoded_bytes > FLAGS_BUDGET {
                return Err(SyncError::Invalid);
            }
            flags.insert(fact.uid, value);
        }
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&round.namespace)
            .ok_or(CacheError::OutOfScope)?;
        let state = check_round(self, &database.connection, round)?;
        if observation.checked_at_ms < state.checked_at_ms
            || *page != known_delta_page(&database.connection, round, page.limit)?
        {
            return Err(SyncError::Stale);
        }
        self.write_checked(round.namespace, database, |tx| {
            for (uid, value) in flags {
                let (id, checked_at): (i64, Option<i64>) = tx.query_row(
                    "SELECT m.id,l.flags_checked_at_ms FROM messages m
                     LEFT JOIN listing_evidence l ON l.message_id=m.id
                     WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid=?3 AND NOT m.tombstone",
                    params![round.mailbox,round.uid_validity,uid],
                    |row| Ok((row.get(0)?,row.get(1)?)),
                )?;
                if checked_at.is_some_and(|current| current > observation.checked_at_ms) {
                    return Err(SyncError::Stale);
                }
                write_listing(tx,id,ListingEvidence {
                    flags:None, internal_date:None, size:None, observed_at_ms:observation.checked_at_ms,
                },Some(&value))?;
            }
            if page.complete {
                tx.execute(
                    "UPDATE sync_delta SET committed_modseq=target_modseq,round_id=NULL,target_modseq=NULL,
                     round_revision=round_revision+1,proof_revision=NULL,known_after=0,checked_at_ms=?3
                     WHERE mailbox=?1 AND lane=?2",
                    params![round.mailbox,round.lane as u8,observation.checked_at_ms],
                )?;
                Ok(None)
            } else {
                tx.execute(
                    "UPDATE sync_delta SET known_after=?3,round_revision=round_revision+1,
                     proof_revision=fact_revision,checked_at_ms=?4 WHERE mailbox=?1 AND lane=?2",
                    params![round.mailbox,round.lane as u8,page.uids.last(),observation.checked_at_ms],
                )?;
                Ok(Some(read_lane(tx,&round.mailbox,round.lane)?
                    .and_then(|state|state.round(round.namespace,&round.mailbox,round.lane)).ok_or(CacheError::Corrupt)?))
            }
        }, || check_cancel(cancelled))
    }

    /// Use only after an explicit NOMODSEQ on a successful current selection.
    pub fn invalidate_delta(
        &self,
        account: &AccountKey,
        mailbox: &str,
        uid_validity: u32,
        fence: &ReconcileFence,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        check_cancel(cancelled)?;
        if !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        check_fence(
            self,
            &database.connection,
            namespace,
            mailbox,
            uid_validity,
            fence,
        )?;
        let present: bool = database.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_delta WHERE mailbox=?1)
                 OR EXISTS(SELECT 1 FROM sync_delta_seed WHERE mailbox=?1 AND floor_modseq IS NOT NULL)",
            [mailbox],
            |row| row.get(0),
        )?;
        if !present {
            return Ok(());
        }
        self.write_checked(
            namespace,
            database,
            |tx| clear_delta(tx, mailbox),
            || check_cancel(cancelled),
        )
    }

    pub fn delta_status(
        &self,
        account: &AccountKey,
        mailbox: &str,
        lane: DeltaLane,
    ) -> Result<Option<DeltaStatus>, SyncError> {
        if !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let Some(state) = read_lane(db, mailbox, lane)? else {
            return Ok(None);
        };
        if state.restore_epoch != self.disk.restore_epoch() || scope_dirty(db, mailbox)? {
            return Ok(None);
        }
        match scope_fence(db, mailbox, state.uid_validity, ScanKind::Reconcile) {
            Ok(revision) if revision == state.dirty_revision => (),
            Ok(_) | Err(SyncError::ClaimActive | SyncError::Stale) => return Ok(None),
            Err(error) => return Err(error),
        }
        let round = state
            .round
            .filter(|round| round.proof_revision == state.fact_revision);
        Ok(Some(DeltaStatus {
            uid_validity: state.uid_validity,
            high_uid: state.high_uid,
            committed_modseq: state.committed.to_string(),
            target_modseq: round.as_ref().map(|round| round.target.to_string()),
            known_after: round.map(|round| round.known_after),
            checked_at_ms: state.checked_at_ms,
        }))
    }
}

/// Called in the same transaction as the final rev1 reconciliation receipt.
pub(super) fn finish_seed(
    tx: &rusqlite::Transaction<'_>,
    stamp: &ScanStamp,
    checked_at_ms: i64,
) -> Result<(), SyncError> {
    let seed: Option<Option<i64>> = tx
        .query_row(
            "SELECT floor_modseq FROM sync_delta_seed WHERE mailbox=?1 AND uid_validity=?2
         AND restore_epoch=?3 AND dirty_revision=?4 AND reconcile_id=?5 AND high_uid=?6",
            params![
                stamp.mailbox,
                stamp.uid_validity,
                stamp.restore_epoch as i64,
                stamp.dirty_revision,
                stamp.scan_id.as_bytes(),
                stamp.high_uid
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(floor) = seed else {
        return Ok(());
    };
    tx.execute(
        "DELETE FROM sync_delta_seed WHERE mailbox=?1",
        [&stamp.mailbox],
    )?;
    let Some(floor) = floor else {
        return Ok(());
    };
    for lane in [DeltaLane::Flags, DeltaLane::FlagsAndExpunges] {
        tx.execute(
            "INSERT INTO sync_delta
             (mailbox,lane,uid_validity,restore_epoch,dirty_revision,high_uid,committed_modseq,checked_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(mailbox,lane) DO UPDATE SET uid_validity=excluded.uid_validity,
                 restore_epoch=excluded.restore_epoch,dirty_revision=excluded.dirty_revision,
                 high_uid=excluded.high_uid,committed_modseq=excluded.committed_modseq,
                 checked_at_ms=excluded.checked_at_ms,fact_revision=fact_revision+1,
                 round_id=NULL,target_modseq=NULL,round_revision=round_revision+1,proof_revision=NULL,known_after=0
             WHERE sync_delta.uid_validity<>excluded.uid_validity
                OR sync_delta.restore_epoch<>excluded.restore_epoch
                OR sync_delta.dirty_revision<>excluded.dirty_revision
                OR sync_delta.high_uid<excluded.high_uid",
            params![stamp.mailbox,lane as u8,stamp.uid_validity,stamp.restore_epoch as i64,
                stamp.dirty_revision,stamp.high_uid,floor,checked_at_ms],
        )?;
    }
    Ok(())
}

fn positive(value: u64) -> Result<i64, SyncError> {
    i64::try_from(value)
        .ok()
        .filter(|&value| value > 0)
        .ok_or(SyncError::Invalid)
}

fn read_seed_floor(db: &Connection, stamp: &ScanStamp) -> Result<Option<Option<i64>>, SyncError> {
    Ok(db
        .query_row(
            "SELECT floor_modseq FROM sync_delta_seed WHERE mailbox=?1 AND reconcile_id=?2",
            params![stamp.mailbox, stamp.scan_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?)
}

fn read_lane(
    db: &Connection,
    mailbox: &str,
    lane: DeltaLane,
) -> Result<Option<LaneState>, SyncError> {
    Ok(db.query_row(
        "SELECT uid_validity,restore_epoch,dirty_revision,high_uid,committed_modseq,checked_at_ms,
                fact_revision,round_id,target_modseq,round_revision,proof_revision,known_after
         FROM sync_delta WHERE mailbox=?1 AND lane=?2",
        params![mailbox,lane as u8],
        |row| {
            let round = row.get::<_,Option<[u8;16]>>(7)?.map(|id| {
                Ok::<_, rusqlite::Error>(RoundState {
                    id: Uuid::from_bytes(id),
                    target: row.get(8)?,
                    revision: row.get(9)?,
                    proof_revision: row.get(10)?,
                    known_after: row.get(11)?,
                })
            }).transpose()?;
            Ok(LaneState {
                uid_validity: row.get(0)?, restore_epoch: row.get::<_,i64>(1)? as u64,
                dirty_revision: row.get(2)?, high_uid: row.get(3)?, committed: row.get(4)?,
                checked_at_ms: row.get(5)?,fact_revision: row.get(6)?,round,
            })
        },
    ).optional()?)
}

fn check_fence(
    cache: &Cache,
    db: &Connection,
    namespace: Namespace,
    mailbox: &str,
    uid_validity: u32,
    fence: &ReconcileFence,
) -> Result<(), SyncError> {
    if fence.namespace != namespace
        || fence.mailbox != mailbox
        || fence.instance != cache.instance
        || fence.restore_epoch != cache.disk.restore_epoch()
        || scope_fence(db, mailbox, uid_validity, ScanKind::Reconcile)? != fence.dirty_revision
    {
        return Err(SyncError::Stale);
    }
    Ok(())
}

fn matches_fence(state: &LaneState, uid_validity: u32, fence: &ReconcileFence) -> bool {
    state.uid_validity == uid_validity
        && state.restore_epoch == fence.restore_epoch
        && state.dirty_revision == fence.dirty_revision
}

fn check_round(cache: &Cache, db: &Connection, round: &DeltaRound) -> Result<LaneState, SyncError> {
    if !cache.namespace_has_folder(&round.namespace, &round.mailbox) {
        return Err(CacheError::OutOfScope.into());
    }
    if !cache.full
        || cache.disk.restore_epoch() != round.restore_epoch
        || scope_fence(db, &round.mailbox, round.uid_validity, ScanKind::Reconcile)?
            != round.dirty_revision
        || scope_dirty(db, &round.mailbox)?
    {
        return Err(SyncError::Stale);
    }
    let state = read_lane(db, &round.mailbox, round.lane)?.ok_or(SyncError::Stale)?;
    if state
        .round(round.namespace, &round.mailbox, round.lane)
        .as_ref()
        != Some(round)
    {
        return Err(SyncError::Stale);
    }
    Ok(state)
}

fn known_delta_page(
    db: &Connection,
    round: &DeltaRound,
    limit: usize,
) -> Result<DeltaPage, SyncError> {
    let mut query = db.prepare_cached(
        "SELECT uid FROM messages WHERE mailbox=?1 AND uid_validity=?2
         AND uid>?3 AND uid<=?4 AND NOT tombstone ORDER BY uid LIMIT ?5",
    )?;
    let mut uids = query
        .query_map(
            params![
                round.mailbox,
                round.uid_validity,
                round.known_after,
                round.high_uid,
                limit as u32 + 1
            ],
            |row| row.get(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let complete = uids.len() <= limit;
    uids.truncate(limit);
    Ok(DeltaPage {
        uids,
        complete,
        limit,
        round_id: round.id,
    })
}

fn clear_delta(tx: &rusqlite::Transaction<'_>, mailbox: &str) -> Result<(), SyncError> {
    tx.execute(
        "UPDATE sync_delta_seed SET floor_modseq=NULL WHERE mailbox=?1",
        [mailbox],
    )?;
    tx.execute("DELETE FROM sync_delta WHERE mailbox=?1", [mailbox])?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;
