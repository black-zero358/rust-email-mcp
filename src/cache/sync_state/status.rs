//! Coverage of observed and frozen synchronization ranges.
use super::*;
use crate::cache::{ArtifactLayer, LayerCoverage, blob::layer_coverage_at, dirty::scope_dirty};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SyncScopeStatus {
    pub uid_validity: Option<u32>,
    pub observed_high_uid: Option<u32>,
    pub checked_at_ms: Option<i64>,
    pub dirty: bool,
    pub history: Option<SyncScanStatus>,
    pub tail: Option<SyncScanStatus>,
    pub reconciliation: Option<SyncScanStatus>,
    pub history_coverage: Option<SyncCoverage>,
    pub current_coverage: Option<SyncCoverage>,
}

#[derive(Debug, Serialize)]
pub struct SyncScanStatus {
    pub scan_id: String,
    pub high_uid: u32,
    pub revision: i64,
    pub next_low: Option<u32>,
    pub known_after: Option<u32>,
    pub checked_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    pub complete: bool,
}

impl SyncScanStatus {
    fn from_scan(scan: &ScanState, current: bool) -> Self {
        let reconcile = scan.stamp.kind == ScanKind::Reconcile;
        Self {
            scan_id: scan.stamp.scan_id.to_string(),
            high_uid: scan.stamp.high_uid,
            revision: scan.stamp.revision,
            next_low: scan.next_low,
            known_after: reconcile.then_some(scan.known_after),
            checked_at_ms: scan.checked_at_ms,
            // Discovery timestamps describe the initial selection. A verified
            // reconciliation additionally records its last successful FETCH.
            completed_at_ms: reconcile.then_some(scan.completed_at_ms).flatten(),
            complete: scan.complete && current,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncCoverage {
    pub high_uid: u32,
    pub discovery_complete: bool,
    pub existence_flags_verified: bool,
    pub known_messages: u64,
    pub complete_listing_facts: u64,
    pub complete_headers: u64,
    pub partial_headers: u64,
    pub missing_headers: u64,
    pub indexed_headers: u64,
    pub exact_thread_index: u64,
    pub verified_manifests: u64,
    pub known_flags: u64,
    /// Most recent individual FLAGS observation in this range.
    pub flags_checked_at_ms: Option<i64>,
    pub indexed_bodies: u64,
    pub listing_facts_complete: bool,
    pub headers_complete: bool,
    pub exact_thread_index_complete: bool,
    pub manifests_complete: bool,
    pub body_index_complete: bool,
    pub raw: SyncLayerStatus,
    pub full_body: SyncLayerStatus,
    pub attachments: SyncLayerStatus,
    pub work: SyncProgress,
    pub gaps: Vec<SyncGapCount>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncLayerStatus {
    #[serde(flatten)]
    pub counts: LayerCoverage,
    pub available_complete: bool,
    pub materialization_complete: bool,
}

impl SyncLayerStatus {
    fn new(counts: LayerCoverage, scope_complete: bool) -> Self {
        let available_complete =
            scope_complete && counts.missing == 0 && counts.omitted == 0 && counts.unsupported == 0;
        Self {
            materialization_complete: available_complete && counts.derivable_from_raw == 0,
            available_complete,
            counts,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncGapCount {
    pub layer: &'static str,
    pub status: &'static str,
    pub reason: &'static str,
    pub count: u64,
}

impl Cache {
    pub fn sync_scope_status(
        &self,
        account: &AccountKey,
        mailbox: &str,
    ) -> Result<SyncScopeStatus, SyncError> {
        if !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let observed: Option<(u32, Option<u32>, i64)> = db
            .query_row(
                "SELECT uid_validity,high_uid,checked_at_ms FROM mailboxes WHERE mailbox=?1",
                [mailbox],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let dirty = scope_dirty(db, mailbox)?;
        let Some((uid_validity, observed_high_uid, checked_at_ms)) = observed else {
            return Ok(SyncScopeStatus {
                uid_validity: None,
                observed_high_uid: None,
                checked_at_ms: None,
                dirty: dirty || self.startup_unclean,
                history: None,
                tail: None,
                reconciliation: None,
                history_coverage: None,
                current_coverage: None,
            });
        };
        let read = |kind| -> Result<Option<ScanState>, SyncError> {
            Ok(read_scan(db, namespace, mailbox, kind)?.filter(|scan| {
                scan.stamp.uid_validity == uid_validity
                    && scan.stamp.restore_epoch == self.disk.restore_epoch()
            }))
        };
        let history = read(ScanKind::History)?;
        let tail = read(ScanKind::Tail)?;
        let reconcile = read(ScanKind::Reconcile)?;
        let reconcile_current = if let Some(scan) = &reconcile {
            match scope_fence(db, mailbox, uid_validity, ScanKind::Reconcile) {
                Ok(revision) => {
                    !dirty
                        && scan.stamp.dirty_revision == revision
                        && scan.presence_revision == scan.proof_presence_revision
                }
                Err(SyncError::ClaimActive) => false,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        let through = |scan: &ScanState| scan.next_low.map_or(scan.stamp.high_uid, |uid| uid - 1);
        let covered = history.as_ref().map(through).map(|high| {
            if history.as_ref().is_some_and(|scan| scan.complete) {
                tail.as_ref().map(through).unwrap_or(high).max(high)
            } else {
                high
            }
        });
        let verified = reconcile
            .as_ref()
            .filter(|scan| scan.complete && reconcile_current)
            .map(|scan| scan.stamp.high_uid);
        let coverage = |high| {
            coverage_at(
                db,
                mailbox,
                uid_validity,
                high,
                covered.is_some_and(|end| end >= high),
                verified.is_some_and(|end| end >= high),
            )
        };
        let history_coverage = history
            .as_ref()
            .map(|scan| coverage(scan.stamp.high_uid))
            .transpose()?;
        let current_coverage = observed_high_uid
            .map(|high| match &history_coverage {
                Some(history) if history.high_uid == high => Ok(history.clone()),
                _ => coverage(high),
            })
            .transpose()?;
        Ok(SyncScopeStatus {
            uid_validity: Some(uid_validity),
            observed_high_uid,
            checked_at_ms: Some(checked_at_ms),
            dirty,
            history: history
                .as_ref()
                .map(|scan| SyncScanStatus::from_scan(scan, true)),
            tail: tail
                .as_ref()
                .map(|scan| SyncScanStatus::from_scan(scan, true)),
            reconciliation: reconcile
                .as_ref()
                .map(|scan| SyncScanStatus::from_scan(scan, reconcile_current)),
            history_coverage,
            current_coverage,
        })
    }
}

fn coverage_at(
    db: &Connection,
    mailbox: &str,
    uid_validity: u32,
    high_uid: u32,
    discovery_complete: bool,
    existence_flags_verified: bool,
) -> Result<SyncCoverage, SyncError> {
    let (known_messages, complete_headers, partial_headers, indexed_headers, exact_thread_index,
        verified_manifests, known_flags, flags_checked_at_ms, indexed_bodies, complete_listing_facts) = db.query_row(
        "SELECT count(*),count(*) FILTER(WHERE m.complete_header),
            count(*) FILTER(WHERE m.header IS NOT NULL AND NOT m.complete_header),
            count(*) FILTER(WHERE m.index_text IS NOT NULL),count(t.message_id),
            count(*) FILTER(WHERE m.parts IS NOT NULL),count(l.flags),max(l.flags_checked_at_ms),count(b.id),
            count(*) FILTER(WHERE l.flags IS NOT NULL AND l.internal_date IS NOT NULL AND l.size IS NOT NULL)
         FROM messages m LEFT JOIN thread_ready t ON t.message_id=m.id AND t.version=1 AND m.complete_header
         LEFT JOIN listing_evidence l ON l.message_id=m.id
         LEFT JOIN artifacts a ON a.message_id=m.id AND a.kind=1 AND a.part=0
             AND a.mime_version=1 AND a.html_version=1 AND a.state=1
         LEFT JOIN body_leaf_metadata d ON d.blob_id=a.blob_id
         LEFT JOIN body_indexes b ON b.message_id=m.id AND b.mime_version=1 AND b.html_version=?4
             AND b.blob_id=d.blob_id AND b.complete
         WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid<=?3 AND NOT m.tombstone",
        params![mailbox,uid_validity,high_uid,super::super::body_index::index_html_version(1)],
        |row| { let count = |index| row.get::<_,i64>(index).map(|v| v as u64);
            Ok((count(0)?,count(1)?,count(2)?,count(3)?,count(4)?,count(5)?,count(6)?,row.get(7)?,count(8)?,count(9)?)) },
    )?;
    let raw = layer_coverage_at(
        db,
        mailbox,
        uid_validity,
        high_uid,
        ArtifactLayer::Raw,
        false,
    )?;
    let body = layer_coverage_at(
        db,
        mailbox,
        uid_validity,
        high_uid,
        ArtifactLayer::FullBody {
            mime_version: 1,
            html_version: 1,
        },
        true,
    )?;
    let attachments = layer_coverage_at(
        db,
        mailbox,
        uid_validity,
        high_uid,
        ArtifactLayer::Attachments { mime_version: 1 },
        false,
    )?;
    let (work, gaps, pending_layers) = work_at(db, mailbox, uid_validity, high_uid)?;
    let complete =
        |layer: usize| discovery_complete && existence_flags_verified && !pending_layers[layer];
    let manifests_complete = complete(1) && verified_manifests == known_messages;
    Ok(SyncCoverage {
        high_uid,
        discovery_complete,
        existence_flags_verified,
        known_messages,
        complete_listing_facts,
        complete_headers,
        partial_headers,
        missing_headers: known_messages - complete_headers - partial_headers,
        indexed_headers,
        exact_thread_index,
        verified_manifests,
        known_flags,
        flags_checked_at_ms,
        indexed_bodies,
        listing_facts_complete: complete(0) && complete_listing_facts == known_messages,
        headers_complete: complete(0) && complete_headers == known_messages,
        exact_thread_index_complete: complete(0) && exact_thread_index == known_messages,
        manifests_complete,
        body_index_complete: complete(4) && indexed_bodies == known_messages,
        raw: SyncLayerStatus::new(raw, complete(2)),
        full_body: SyncLayerStatus::new(body, complete(3)),
        attachments: SyncLayerStatus::new(attachments, manifests_complete && complete(5)),
        work,
        gaps,
    })
}

fn work_at(
    db: &Connection,
    mailbox: &str,
    uid_validity: u32,
    high_uid: u32,
) -> Result<(SyncProgress, Vec<SyncGapCount>, [bool; 6]), SyncError> {
    let mut work = SyncProgress::default();
    let mut gaps = Vec::new();
    let mut pending_layers = [false; 6];
    let mut statement = db.prepare_cached(
        "SELECT layer,state,reason,count(*),min(not_before_ms) FROM sync_work
         WHERE mailbox=?1 AND uid_validity=?2 AND uid<=?3 GROUP BY layer,state,reason ORDER BY layer,state,reason",
    )?;
    let mut rows = statement.query(params![mailbox, uid_validity, high_uid])?;
    const LAYERS: [&str; 6] = [
        "facts",
        "manifest",
        "raw",
        "full_body",
        "body_index",
        "attachments",
    ];
    const REASONS: [&str; 8] = [
        "raw_limit",
        "decoded_limit",
        "quota",
        "unsupported_encoding",
        "unsupported_content",
        "network",
        "protocol",
        "storage",
    ];
    while let Some(row) = rows.next()? {
        let layer_index = usize::from(row.get::<_, u8>(0)?);
        let layer = *LAYERS.get(layer_index).ok_or(CacheError::Corrupt)?;
        let state: u8 = row.get(1)?;
        let reason: Option<u8> = row.get(2)?;
        let count = row.get::<_, i64>(3)? as u64;
        match (state, reason) {
            (0, _) | (4, None) => work.pending += count,
            (1, None) => work.completed += count,
            (2, Some(_)) => work.terminal += count,
            (3 | 4, Some(3)) => work.quota_blocked += count,
            _ => return Err(CacheError::Corrupt.into()),
        }
        if matches!((state, reason), (0 | 3, _) | (4, None)) {
            pending_layers[layer_index] = true;
        }
        if state == 0 && reason.is_some() {
            work.retrying += count;
            let due = row.get(4)?;
            work.next_due_ms = Some(work.next_due_ms.map_or(due, |earliest| earliest.min(due)));
        }
        if let Some(code) = reason {
            let reason = code
                .checked_sub(1)
                .and_then(|index| REASONS.get(usize::from(index)))
                .ok_or(CacheError::Corrupt)?;
            let status = match state {
                0 => "retrying",
                2 => "terminal",
                3 => "quota_blocked",
                4 => "reclaimed",
                _ => return Err(CacheError::Corrupt.into()),
            };
            gaps.push(SyncGapCount {
                layer,
                status,
                reason,
                count,
            });
        }
    }
    Ok((work, gaps, pending_layers))
}
