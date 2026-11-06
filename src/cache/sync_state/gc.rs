//! Bounded selection of versioned derived copies for pressure reclamation.
use super::*;
use crate::cache::{ArtifactKey, BlobId, BlobInfo, Ref};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const RECLAIM_PAGE: usize = 16;
const RECLAIM_SCAN: u32 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncReclaimPosition {
    scope: [u8; 32],
    uid_validity: u32,
    seq: i64,
}

#[derive(Debug)]
pub struct SyncReclaimCandidate {
    pub uid: u32,
    pub uid_validity: u32,
    pub kind: ArtifactKey,
    pub materialized: BlobInfo,
    pub raw: BlobInfo,
    pub position: SyncReclaimPosition,
}

#[derive(Debug, Default)]
pub struct SyncReclaimPage {
    pub candidates: Vec<SyncReclaimCandidate>,
    pub next: Option<SyncReclaimPosition>,
}

impl Cache {
    pub fn evict_sync_candidate(
        self: &Arc<Self>,
        reference: Ref<'_>,
        candidate: &SyncReclaimCandidate,
    ) -> Result<bool, CacheError> {
        let namespace = self.scope(reference.account, reference.mailbox)?;
        if candidate.uid != reference.uid
            || candidate.uid_validity != reference.uid_validity
            || candidate.position.scope != reclaim_scope(namespace, reference.mailbox)
        {
            return Err(CacheError::OutOfScope);
        }
        let changed = self.evict_derived_inner(
            reference,
            candidate.kind,
            candidate.materialized.id,
            candidate.raw.id,
            Some(candidate.position.seq),
        )?;
        if !changed {
            return Ok(false);
        }
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let (_, released_bytes) =
            self.collect_sync_orphan(namespace, database, candidate.materialized.id)?;
        Ok(released_bytes > 0)
    }

    /// The caller verifies same-version derivation before attempting eviction.
    /// Position is scoped to the account, mailbox and UIDVALIDITY.
    pub fn sync_reclaim_candidates(
        &self,
        account: &AccountKey,
        mailbox: &str,
        after: Option<SyncReclaimPosition>,
        limit: usize,
    ) -> Result<SyncReclaimPage, SyncError> {
        if !self.full || !(1..=RECLAIM_PAGE).contains(&limit) {
            return Err(SyncError::Invalid);
        }
        let namespace = self.scope(account, mailbox)?;
        let scope = reclaim_scope(namespace, mailbox);
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let uid_validity: Option<u32> = db
            .query_row(
                "SELECT uid_validity FROM mailboxes WHERE mailbox=?1",
                [mailbox],
                |row| row.get(0),
            )
            .optional()?;
        let Some(uid_validity) = uid_validity else {
            return Ok(SyncReclaimPage::default());
        };
        let after = match after {
            Some(position) if position.scope == scope && position.uid_validity == uid_validity => {
                position.seq
            }
            Some(_) => return Err(SyncError::Stale),
            None => 0,
        };
        // Find the end of the recency window before applying eligibility
        // checks. A page may have no reclaimable copy and still advance.
        let through: Option<i64> = db
            .query_row(
                "SELECT seq FROM blob_recency WHERE mailbox=?1 AND uid_validity=?2 AND seq>?3
                 ORDER BY seq LIMIT 1 OFFSET ?4",
                params![mailbox, uid_validity, after, RECLAIM_SCAN - 1],
                |row| row.get(0),
            )
            .optional()?;
        let mut query = db.prepare_cached(
            "SELECT c.seq,m.uid,c.kind,c.part,b.id,b.bytes,b.sha256,rb.id,rb.bytes,rb.sha256
             FROM blob_recency c
             CROSS JOIN messages m ON m.id=c.message_id
             CROSS JOIN blobs b ON b.id=c.blob_id
             CROSS JOIN artifacts r ON r.message_id=m.id AND r.kind=0 AND r.state=1
             CROSS JOIN blobs rb ON rb.id=r.blob_id
             WHERE c.mailbox=?1 AND c.uid_validity=?2 AND c.seq>?3 AND c.seq<=?4
               AND NOT m.tombstone AND c.mime_version=1 AND b.bytes>0
               AND ((c.kind=1 AND c.html_version=1 AND EXISTS(
                    SELECT 1 FROM body_leaf_metadata d WHERE d.blob_id=c.blob_id))
                   OR (c.kind=2 AND c.html_version=0))
             ORDER BY c.seq LIMIT ?5",
        )?;
        let mut rows = query.query(params![
            mailbox,
            uid_validity,
            after,
            through.unwrap_or(i64::MAX),
            limit as u32 + 1
        ])?;
        let mut candidates = Vec::with_capacity(limit);
        while let Some(row) = rows.next()? {
            if candidates.len() == limit {
                return Ok(SyncReclaimPage {
                    next: candidates
                        .last()
                        .map(|item: &SyncReclaimCandidate| item.position),
                    candidates,
                });
            }
            let uid = row.get(1)?;
            let kind = row.get(2)?;
            let part = row.get(3)?;
            let position = SyncReclaimPosition {
                scope,
                uid_validity,
                seq: row.get(0)?,
            };
            let kind = match kind {
                1 => ArtifactKey::FullBody {
                    mime_version: 1,
                    html_version: 1,
                },
                2 => ArtifactKey::Attachment {
                    original_index: part,
                    mime_version: 1,
                },
                _ => return Err(CacheError::Corrupt.into()),
            };
            let info = |column| -> rusqlite::Result<BlobInfo> {
                Ok(BlobInfo {
                    id: BlobId::from_bytes(row.get(column)?),
                    bytes: u64::from(row.get::<_, u32>(column + 1)?),
                    sha256: row.get(column + 2)?,
                })
            };
            candidates.push(SyncReclaimCandidate {
                uid,
                uid_validity,
                kind,
                materialized: info(4)?,
                raw: info(7)?,
                position,
            });
        }
        Ok(SyncReclaimPage {
            candidates,
            next: through.map(|seq| SyncReclaimPosition {
                scope,
                uid_validity,
                seq,
            }),
        })
    }
}

fn reclaim_scope(namespace: Namespace, mailbox: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(namespace);
    digest.update(mailbox.as_bytes());
    digest.finalize().into()
}
