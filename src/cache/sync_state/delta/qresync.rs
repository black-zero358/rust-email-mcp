//! QRESYNC's independent deletion lane and bounded selection proof.
use super::*;
use std::ops::RangeInclusive;

#[derive(Debug, Eq, PartialEq)]
pub struct QresyncPreview {
    pub uid_validity: u32,
    pub high_uid: u32,
    pub base_modseq: u64,
    pub known: Vec<u32>,
    pub complete: bool,
    namespace: Namespace,
    mailbox: String,
    instance: Uuid,
    restore_epoch: u64,
    dirty_revision: i64,
    fact_revision: i64,
    round: Option<DeltaRound>,
}

impl QresyncPreview {
    pub fn examine_uids(&self) -> Option<&[u32]> {
        (self.complete
            && !self.known.is_empty()
            && self
                .round
                .as_ref()
                .is_none_or(|round| round.known_after == 0))
        .then_some(self.known.as_slice())
    }
}

#[derive(Default)]
pub struct QresyncChanges<'a> {
    pub flags: &'a [DeltaFlags<'a>],
    pub earlier: &'a [RangeInclusive<u32>],
    pub live: &'a [RangeInclusive<u32>],
}

impl QresyncChanges<'_> {
    fn prepare(
        self,
        known: &[u32],
        base: u64,
        uid_next: u32,
    ) -> Result<PreparedChanges, SyncError> {
        if self.earlier.len().saturating_add(self.live.len()) > PAGE_UIDS * 2 {
            return Err(SyncError::ReconcileRequired);
        }
        let flags = prepare_flags(base, known, self.flags)?;
        let mut vanished = BTreeSet::new();
        for (earlier, ranges) in [(true, self.earlier), (false, self.live)] {
            for range in ranges {
                let (first, last) = (*range.start(), *range.end());
                if first == 0 || first > last {
                    return Err(SyncError::Invalid);
                }
                if last >= uid_next || last - first >= PAGE_UIDS as u32 {
                    return Err(SyncError::ReconcileRequired);
                }
                // Only bounded cardinalities reach iteration. EARLIER cannot
                // introduce UIDs outside the actual requested set.
                for uid in range.clone() {
                    if (earlier && known.binary_search(&uid).is_err()) || flags.contains_key(&uid) {
                        return Err(SyncError::Invalid);
                    }
                    if !vanished.contains(&uid) {
                        if vanished.len() == PAGE_UIDS {
                            return Err(SyncError::ReconcileRequired);
                        }
                        vanished.insert(uid);
                    }
                }
            }
        }
        Ok(PreparedChanges { flags, vanished })
    }
}

impl Cache {
    /// Classify the whole frozen scope using one extra UID as lookahead.
    /// Only a complete, nonempty first page can be sent in EXAMINE QRESYNC.
    pub fn preview_qresync(
        &self,
        account: &AccountKey,
        mailbox: &str,
        fence: &ReconcileFence,
    ) -> Result<Option<QresyncPreview>, SyncError> {
        if !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let Some(state) = read_lane(db, mailbox, DeltaLane::FlagsAndExpunges)? else {
            return Ok(None);
        };
        check_fence(self, db, namespace, mailbox, state.uid_validity, fence)?;
        if !matches_fence(&state, state.uid_validity, fence) || scope_dirty(db, mailbox)? {
            return Ok(None);
        }
        Ok(Some(make_preview(self, db, namespace, mailbox, &state)?))
    }

    /// Validate the exact same cache proof used to construct EXAMINE QRESYNC.
    pub fn start_qresync(
        &self,
        preview: &QresyncPreview,
        observation: DeltaObservation,
        fence: &ReconcileFence,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<DeltaStart, SyncError> {
        self.start_delta_inner(
            DeltaSource {
                namespace: preview.namespace,
                mailbox: &preview.mailbox,
                lane: DeltaLane::FlagsAndExpunges,
                preview: Some(preview),
            },
            observation,
            fence,
            cancelled,
        )
    }

    /// Apply a complete UID FETCH VANISHED or exact EXAMINE QRESYNC page.
    /// A transport receipt must resolve a later real-time removal before
    /// handing in FLAGS; an unresolved positive/deletion conflict is rejected.
    pub fn commit_qresync_page(
        &self,
        round: &DeltaRound,
        page: &DeltaPage,
        observation: DeltaObservation,
        changes: QresyncChanges<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<DeltaRound>, SyncError> {
        check_cancel(cancelled)?;
        observation.validate()?;
        if round.lane != DeltaLane::FlagsAndExpunges
            || observation.uid_validity != round.uid_validity
        {
            return Err(SyncError::Invalid);
        }
        let changes = changes.prepare(&page.uids, round.base_modseq, observation.uid_next)?;
        self.commit_prepared_delta_page(round, page, observation, changes, cancelled)
    }

    /// Explicit events can arrive even when the selected watermark still
    /// equals the committed value. Apply them without advancing that value.
    pub fn commit_qresync_current(
        &self,
        preview: &QresyncPreview,
        observation: DeltaObservation,
        changes: QresyncChanges<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        check_cancel(cancelled)?;
        observation.validate()?;
        if observation.uid_validity != preview.uid_validity
            || observation.uid_next <= preview.high_uid
            || observation.highest_modseq != preview.base_modseq
            || (preview.examine_uids().is_none()
                && (!changes.flags.is_empty() || !changes.earlier.is_empty()))
        {
            return Err(SyncError::Invalid);
        }
        let changes = changes.prepare(&preview.known, preview.base_modseq, observation.uid_next)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&preview.namespace)
            .ok_or(CacheError::OutOfScope)?;
        let db = &database.connection;
        let state = read_lane(db, &preview.mailbox, DeltaLane::FlagsAndExpunges)?
            .ok_or(SyncError::Stale)?;
        check_preview(self, db, preview, &state, observation.uid_validity)?;
        if state.round.is_some() || observation.checked_at_ms < state.checked_at_ms {
            return Err(SyncError::Stale);
        }
        if changes.flags.is_empty() && changes.vanished.is_empty() {
            return Ok(());
        }
        self.write_checked(
            preview.namespace,
            database,
            |tx| apply_changes(tx, &preview.mailbox, observation, changes),
            || check_cancel(cancelled),
        )
    }

    /// Fence out a previous partial audit when explicit deletion evidence
    /// cannot fit a single safe transaction. A fresh rev1 pass clears dirty.
    pub fn require_qresync_reconcile(
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
        self.write_checked(
            namespace,
            database,
            |tx| {
                tx.execute(
                    "INSERT INTO scope_fences VALUES(?1,1,1)
                 ON CONFLICT(mailbox) DO UPDATE SET revision=revision+1,dirty=1",
                    [mailbox],
                )?;
                tx.execute(
                    "UPDATE mailboxes SET dirty=1 WHERE mailbox=?1 AND uid_validity=?2",
                    params![mailbox, uid_validity],
                )?;
                Ok(())
            },
            || check_cancel(cancelled),
        )
    }
}

pub(super) fn check_preview(
    cache: &Cache,
    db: &Connection,
    preview: &QresyncPreview,
    state: &LaneState,
    uid_validity: u32,
) -> Result<(), SyncError> {
    if !cache.namespace_has_folder(&preview.namespace, &preview.mailbox) {
        return Err(CacheError::OutOfScope.into());
    }
    if !cache.full
        || uid_validity != preview.uid_validity
        || uid_validity != state.uid_validity
        || state.restore_epoch != cache.disk.restore_epoch()
        || scope_fence(db, &preview.mailbox, uid_validity, ScanKind::Reconcile)?
            != preview.dirty_revision
        || scope_dirty(db, &preview.mailbox)?
        || &make_preview(cache, db, preview.namespace, &preview.mailbox, state)? != preview
    {
        return Err(SyncError::Stale);
    }
    Ok(())
}

fn make_preview(
    cache: &Cache,
    db: &Connection,
    namespace: Namespace,
    mailbox: &str,
    state: &LaneState,
) -> Result<QresyncPreview, SyncError> {
    let mut known = known_uids(
        db,
        mailbox,
        state.uid_validity,
        0,
        state.high_uid,
        PAGE_UIDS + 1,
    )?;
    let complete = known.len() <= PAGE_UIDS;
    known.truncate(PAGE_UIDS);
    Ok(QresyncPreview {
        uid_validity: state.uid_validity,
        high_uid: state.high_uid,
        base_modseq: state.committed as u64,
        known,
        complete,
        namespace,
        mailbox: mailbox.into(),
        instance: cache.instance,
        restore_epoch: state.restore_epoch,
        dirty_revision: state.dirty_revision,
        fact_revision: state.fact_revision,
        round: state.round(namespace, mailbox, DeltaLane::FlagsAndExpunges),
    })
}
