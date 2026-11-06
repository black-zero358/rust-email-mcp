//! Bounded, restartable work for each immutable content layer.
use super::*;
use crate::cache::{
    ArtifactKey, BlobId, Omission, ThreadId,
    metadata::write_headers,
    thread_index::{prepare_thread_ids, write_thread_ids},
};
use rusqlite::Transaction;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentLayer {
    Facts,
    Manifest,
    Raw,
    FullBody,
    BodyIndex,
    Attachment(u32),
}

impl ContentLayer {
    fn sql(self) -> (u8, u32) {
        match self {
            Self::Facts => (0, 0),
            Self::Manifest => (1, 0),
            Self::Raw => (2, 0),
            Self::FullBody => (3, 0),
            Self::BodyIndex => (4, 0),
            Self::Attachment(index) => (5, index),
        }
    }

    fn from_sql(layer: u8, part: u32) -> Result<Self, SyncError> {
        match (layer, part) {
            (0, 0) => Ok(Self::Facts),
            (1, 0) => Ok(Self::Manifest),
            (2, 0) => Ok(Self::Raw),
            (3, 0) => Ok(Self::FullBody),
            (4, 0) => Ok(Self::BodyIndex),
            (5, index) if index < 1024 => Ok(Self::Attachment(index)),
            _ => Err(CacheError::Corrupt.into()),
        }
    }

    fn artifact(self) -> Option<ArtifactKey> {
        match self {
            Self::Raw => Some(ArtifactKey::Raw),
            Self::FullBody | Self::BodyIndex => Some(ArtifactKey::FullBody {
                mime_version: 1,
                html_version: 1,
            }),
            Self::Attachment(original_index) => Some(ArtifactKey::Attachment {
                original_index,
                mime_version: 1,
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ContentWork {
    pub uid: u32,
    pub layer: ContentLayer,
    stamp: ScanStamp,
    revision: i64,
}

impl ContentWork {
    pub fn matches_scope(&self, namespace: &[u8; 32], mailbox: &str, uid_validity: u32) -> bool {
        self.stamp.matches_scope(namespace, mailbox, uid_validity)
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum FailureReason {
    Network = 6,
    Protocol = 7,
    Storage = 8,
}

#[derive(Clone, Copy, Debug)]
pub enum ContentOutcome {
    Present,
    PresentBlob(BlobId),
    Terminal(Omission),
    RetryAt {
        reason: FailureReason,
        not_before_ms: i64,
    },
    QuotaBlocked,
    NeedsRaw,
    MissingRemote,
}

pub struct SyncFacts<'a> {
    pub header: &'a [u8],
    pub complete: bool,
    pub index_text: Option<&'a str>,
    pub thread_ids: Option<&'a [ThreadId<'a>]>,
    pub flags: &'a [String],
    pub internal_date: &'a str,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct SyncProgress {
    pub pending: u64,
    pub completed: u64,
    pub terminal: u64,
    pub quota_blocked: u64,
    pub retrying: u64,
    pub next_due_ms: Option<i64>,
}

impl Cache {
    pub fn sync_content_page(
        &self,
        stamp: &ScanStamp,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<ContentWork>, SyncError> {
        if !(1..=PAGE_UIDS).contains(&limit) || now_ms < 0 || stamp.kind == ScanKind::Reconcile {
            return Err(SyncError::Invalid);
        }
        let databases = self.lock()?;
        let db = &databases
            .get(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        checked_scan(self, db, stamp)?;
        let mut statement = db.prepare_cached(
            "SELECT uid,layer,part,revision FROM sync_work WHERE mailbox=?1 AND uid_validity=?2
             AND uid<=?3 AND state=0 AND not_before_ms<=?4
             ORDER BY not_before_ms,uid,layer,part LIMIT ?5",
        )?;
        let mut rows = statement.query(params![
            stamp.mailbox,
            stamp.uid_validity,
            stamp.high_uid,
            now_ms,
            limit as u32
        ])?;
        let mut jobs = Vec::with_capacity(limit);
        while let Some(row) = rows.next()? {
            jobs.push(ContentWork {
                uid: row.get(0)?,
                layer: ContentLayer::from_sql(row.get(1)?, row.get(2)?)?,
                revision: row.get(3)?,
                stamp: stamp.clone(),
            });
        }
        Ok(jobs)
    }

    /// Persist metadata and its checkpoint in the same transaction.
    pub fn commit_sync_facts(
        &self,
        work: &ContentWork,
        facts: SyncFacts<'_>,
        checked_at_ms: i64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        if work.layer != ContentLayer::Facts
            || checked_at_ms < 0
            || facts.header.len() > 128 * 1024
            || facts.internal_date.is_empty()
            || facts.internal_date.len() > 64
            || facts.internal_date.chars().any(char::is_control)
            || (facts.thread_ids.is_some() && !facts.complete)
            || facts
                .index_text
                .is_some_and(|text| !facts.complete || text.len() > 8192 || text.contains('\0'))
        {
            return Err(SyncError::Invalid);
        }
        let flags = encode_flags(facts.flags)?;
        let ids = facts.thread_ids.map(prepare_thread_ids).transpose()?;
        self.apply_sync_work(work, cancelled, |tx, message| {
            write_headers(
                tx,
                message,
                facts.complete,
                facts.header,
                facts.index_text.filter(|text| !text.is_empty()),
            )?;
            if let Some(ids) = &ids {
                // The same transaction has just written the complete header.
                write_thread_ids(
                    tx,
                    message,
                    &work.stamp.mailbox,
                    work.stamp.uid_validity,
                    work.uid,
                    ids,
                )?;
            }
            write_listing(
                tx,
                message,
                ListingEvidence {
                    flags: None,
                    internal_date: Some(facts.internal_date),
                    size: Some(facts.size),
                    observed_at_ms: checked_at_ms,
                },
                Some(&flags),
            )?;
            if facts.complete {
                finish_work(tx, work, message, ContentOutcome::Present)
            } else {
                finish_work(
                    tx,
                    work,
                    message,
                    ContentOutcome::Terminal(Omission::UnsupportedContent),
                )
            }
        })
    }

    pub fn commit_sync_content(
        &self,
        work: &ContentWork,
        outcome: ContentOutcome,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        self.apply_sync_work(work, cancelled, |tx, message| {
            if matches!(outcome, ContentOutcome::MissingRemote) {
                // The UID was assigned inside the discovered scan. A strict
                // tagged miss therefore retires all of its content work.
                return Ok(mark_tombstone(tx, message, true)?);
            }
            verify_receipt(tx, work, message, outcome)?;
            finish_work(tx, work, message, outcome)
        })
    }

    fn apply_sync_work(
        &self,
        work: &ContentWork,
        cancelled: &dyn Fn() -> bool,
        apply: impl FnOnce(&Transaction<'_>, i64) -> Result<(), SyncError>,
    ) -> Result<(), SyncError> {
        let stamp = &work.stamp;
        check_cancel(cancelled)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?;
        checked_scan(self, &database.connection, stamp)?;
        let (layer, part) = work.layer.sql();
        self.write_checked(stamp.namespace, database, |tx| {
            let message: i64 = tx.query_row(
                "SELECT m.id FROM messages m JOIN sync_work w USING(mailbox,uid_validity,uid)
                 WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid=?3 AND NOT m.tombstone
                   AND w.layer=?4 AND w.part=?5 AND w.state=0 AND w.revision=?6",
                params![stamp.mailbox,stamp.uid_validity,work.uid,layer,part,work.revision], |row| row.get(0),
            ).optional()?.ok_or(SyncError::Stale)?;
            apply(tx, message)
        }, || check_cancel(cancelled))
    }

    pub fn resume_sync_quota(&self, stamp: &ScanStamp, limit: usize) -> Result<usize, SyncError> {
        self.resume_sync_quota_inner(stamp, limit, true)
    }

    /// Retry capacity failures after freeing physical bytes, without
    /// rematerializing the copies deliberately reclaimed to free them.
    pub fn resume_sync_quota_after_reclaim(
        &self,
        stamp: &ScanStamp,
        limit: usize,
    ) -> Result<usize, SyncError> {
        self.resume_sync_quota_inner(stamp, limit, false)
    }

    fn resume_sync_quota_inner(
        &self,
        stamp: &ScanStamp,
        limit: usize,
        include_reclaimed: bool,
    ) -> Result<usize, SyncError> {
        if !(1..=PAGE_UIDS).contains(&limit) || stamp.kind == ScanKind::Reconcile {
            return Err(SyncError::Invalid);
        }
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?;
        checked_scan(self, &database.connection, stamp)?;
        let ready: bool = database.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_work WHERE mailbox=?1 AND uid_validity=?2 AND uid<=?3
              AND (state=3 OR (?4 AND state=4 AND reason=3)))",
            params![
                stamp.mailbox,
                stamp.uid_validity,
                stamp.high_uid,
                include_reclaimed
            ],
            |row| row.get(0),
        )?;
        if !ready {
            return Ok(0);
        }
        self.write(stamp.namespace, database, |tx| {
            Ok(tx.execute(
                "UPDATE sync_work SET state=0,reason=NULL,revision=revision+1 WHERE rowid IN (
                SELECT rowid FROM sync_work WHERE mailbox=?1 AND uid_validity=?2 AND uid<=?3
                  AND (state=3 OR (?5 AND state=4 AND reason=3))
                ORDER BY state,uid,layer,part LIMIT ?4)",
                params![
                    stamp.mailbox,
                    stamp.uid_validity,
                    stamp.high_uid,
                    limit as u32,
                    include_reclaimed
                ],
            )?)
        })
    }

    /// Sweep only bookkeeping for retired UIDVALIDITY values. Blob files
    /// and their leases remain owned by the separate artifact collector.
    pub fn collect_retired_sync_work(
        &self,
        account: &AccountKey,
        limit: usize,
    ) -> Result<usize, SyncError> {
        if !self.full || !(1..=PAGE_UIDS).contains(&limit) {
            return Err(SyncError::Invalid);
        }
        let namespace = account.reference_namespace();
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let ready: bool = database.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM retired_epochs e JOIN sync_work w USING(mailbox,uid_validity))", [], |row| row.get(0),
        )?;
        if !ready {
            return Ok(0);
        }
        self.write(namespace, database, |tx| {
            Ok(tx.execute("DELETE FROM sync_work WHERE rowid IN (
                SELECT w.rowid FROM retired_epochs e JOIN sync_work w USING(mailbox,uid_validity) LIMIT ?1
            )",[limit as u32])?)
        })
    }

    pub fn sync_progress(&self, stamp: &ScanStamp) -> Result<SyncProgress, SyncError> {
        let databases = self.lock()?;
        let db = &databases
            .get(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        checked_scan(self, db, stamp)?;
        Ok(db.query_row(
            "SELECT count(*) FILTER(WHERE state=0 OR (state=4 AND reason IS NULL)),
                    count(*) FILTER(WHERE state=1),count(*) FILTER(WHERE state=2),
                    count(*) FILTER(WHERE state=3 OR (state=4 AND reason=3)),
                    count(*) FILTER(WHERE state=0 AND reason IS NOT NULL),
                    min(not_before_ms) FILTER(WHERE state=0 AND reason IS NOT NULL)
             FROM sync_work WHERE mailbox=?1 AND uid_validity=?2 AND uid<=?3",
            params![stamp.mailbox, stamp.uid_validity, stamp.high_uid],
            |row| {
                Ok(SyncProgress {
                    pending: row.get::<_, i64>(0)? as u64,
                    completed: row.get::<_, i64>(1)? as u64,
                    terminal: row.get::<_, i64>(2)? as u64,
                    quota_blocked: row.get::<_, i64>(3)? as u64,
                    retrying: row.get::<_, i64>(4)? as u64,
                    next_due_ms: row.get(5)?,
                })
            },
        )?)
    }
}

fn verify_receipt(
    tx: &Transaction<'_>,
    work: &ContentWork,
    message: i64,
    outcome: ContentOutcome,
) -> Result<(), SyncError> {
    let layer = work.layer;
    match outcome {
        ContentOutcome::Present => {
            let sql = match layer {
                ContentLayer::Manifest => "SELECT parts IS NOT NULL FROM messages WHERE id=?1",
                ContentLayer::Facts => "SELECT EXISTS(SELECT 1 FROM messages m JOIN listing_evidence l ON l.message_id=m.id
                    WHERE m.id=?1 AND m.complete_header AND l.flags IS NOT NULL AND l.internal_date IS NOT NULL AND l.size IS NOT NULL)",
                _ => return Err(SyncError::Invalid),
            };
            if !tx.query_row(sql, [message], |row| row.get::<_, bool>(0))? {
                return Err(SyncError::Invalid);
            }
        }
        ContentOutcome::PresentBlob(blob) => {
            let key = layer.artifact().ok_or(SyncError::Invalid)?;
            let (kind, part, mime, html) = key.sql()?;
            let valid: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM artifacts WHERE message_id=?1 AND kind=?2 AND part=?3
                AND mime_version=?4 AND html_version=?5 AND state=1 AND blob_id=?6)",
                params![message, kind, part, mime, html, blob.bytes()],
                |row| row.get(0),
            )?;
            if !valid {
                return Err(SyncError::Invalid);
            }
            if matches!(layer, ContentLayer::FullBody | ContentLayer::BodyIndex)
                && !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM body_leaf_metadata WHERE blob_id=?1)",
                    [blob.bytes()],
                    |row| row.get::<_, bool>(0),
                )?
            {
                return Err(SyncError::Invalid);
            }
            if layer == ContentLayer::BodyIndex && !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM body_indexes WHERE message_id=?1 AND mime_version=?2 AND html_version=?3
                    AND blob_id=?4 AND complete)", params![message,mime,html,blob.bytes()],|row| row.get::<_,bool>(0))?
            { return Err(SyncError::Invalid); }
        }
        ContentOutcome::Terminal(reason) => {
            if reason == Omission::Quota {
                return Err(SyncError::Invalid);
            }
            if let Some(key) = layer
                .artifact()
                .filter(|_| layer != ContentLayer::BodyIndex)
            {
                let (kind, part, mime, html) = key.sql()?;
                let state = match reason {
                    Omission::UnsupportedEncoding | Omission::UnsupportedContent => 4,
                    _ => 3,
                };
                if tx.execute("INSERT INTO artifacts(message_id,kind,part,mime_version,html_version,state,reason)
                    VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(message_id,kind,part,mime_version,html_version)
                    DO UPDATE SET state=excluded.state,reason=excluded.reason,raw_id=NULL WHERE artifacts.state<>1",
                    params![message,kind,part,mime,html,state,reason as u8])? != 1 { return Err(SyncError::Stale); }
            }
        }
        ContentOutcome::RetryAt { not_before_ms, .. } if not_before_ms < 0 => {
            return Err(SyncError::Invalid);
        }
        ContentOutcome::NeedsRaw if layer != ContentLayer::Manifest => {
            return Err(SyncError::Invalid);
        }
        ContentOutcome::NeedsRaw => {
            let stamp = &work.stamp;
            let settled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_work WHERE mailbox=?1 AND uid_validity=?2
                    AND uid=?3 AND layer=2 AND part=0 AND state IN(1,2))",
                params![stamp.mailbox, stamp.uid_validity, work.uid],
                |row| row.get(0),
            )?;
            if settled {
                return Err(SyncError::Stale);
            }
        }
        _ => {}
    }
    Ok(())
}

fn finish_work(
    tx: &Transaction<'_>,
    work: &ContentWork,
    message: i64,
    outcome: ContentOutcome,
) -> Result<(), SyncError> {
    let (state, reason, due) = match outcome {
        ContentOutcome::Present | ContentOutcome::PresentBlob(_) => (1, None, 0),
        ContentOutcome::Terminal(reason) => (2, Some(reason as u8), 0),
        ContentOutcome::RetryAt {
            reason,
            not_before_ms,
        } => (0, Some(reason as u8), not_before_ms),
        ContentOutcome::QuotaBlocked => (3, Some(Omission::Quota as u8), 0),
        ContentOutcome::NeedsRaw => (4, None, 0),
        ContentOutcome::MissingRemote => return Err(SyncError::Invalid),
    };
    let stamp = &work.stamp;
    let (layer, part) = work.layer.sql();
    tx.execute(
        "UPDATE sync_work SET state=?6,reason=?7,not_before_ms=?8,revision=revision+1
                WHERE mailbox=?1 AND uid_validity=?2 AND uid=?3 AND layer=?4 AND part=?5",
        params![
            stamp.mailbox,
            stamp.uid_validity,
            work.uid,
            layer,
            part,
            state,
            reason,
            due
        ],
    )?;
    let next = match (work.layer, state) {
        (ContentLayer::Manifest, 4) => ContentLayer::Raw,
        (_, 0 | 3 | 4) => return Ok(()),
        (ContentLayer::Facts, _) => ContentLayer::Manifest,
        (ContentLayer::Manifest, _) => {
            let raw_done: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_work WHERE mailbox=?1 AND uid_validity=?2
                    AND uid=?3 AND layer=2 AND part=0 AND state IN(1,2))",
                params![stamp.mailbox, stamp.uid_validity, work.uid],
                |row| row.get(0),
            )?;
            if raw_done {
                ContentLayer::FullBody
            } else {
                ContentLayer::Raw
            }
        }
        (ContentLayer::Raw, _) => {
            if tx.execute("UPDATE sync_work SET state=0,revision=revision+1 WHERE mailbox=?1 AND uid_validity=?2
                AND uid=?3 AND layer=1 AND part=0 AND state=4",params![stamp.mailbox,stamp.uid_validity,work.uid])? > 0 { return Ok(()); }
            ContentLayer::FullBody
        }
        (ContentLayer::FullBody, 1) => ContentLayer::BodyIndex,
        (ContentLayer::FullBody | ContentLayer::BodyIndex, _) => {
            tx.execute("INSERT OR IGNORE INTO sync_work(mailbox,uid_validity,uid,layer,part,state)
                SELECT ?1,?2,?3,5,part,0 FROM artifacts WHERE message_id=?4 AND kind=2 AND mime_version=1",
                params![stamp.mailbox,stamp.uid_validity,work.uid,message])?;
            return Ok(());
        }
        (ContentLayer::Attachment(_), _) => return Ok(()),
    };
    let (layer, part) = next.sql();
    tx.execute(
        "INSERT INTO sync_work(mailbox,uid_validity,uid,layer,part,state) VALUES(?1,?2,?3,?4,?5,0)
        ON CONFLICT(mailbox,uid_validity,uid,layer,part) DO UPDATE SET state=0,revision=revision+1
        WHERE excluded.layer=4 AND sync_work.state=4",
        params![stamp.mailbox, stamp.uid_validity, work.uid, layer, part],
    )?;
    Ok(())
}
