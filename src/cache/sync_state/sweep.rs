//! Keyset-bounded cleanup of retired or unreferenced cache records.
use super::*;
use crate::cache::{BlobId, Database};

const PAGE: u32 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncGcPosition {
    namespace: Namespace,
    restore_epoch: u64,
    phase: usize,
    after: i64,
    through: Option<i64>,
}

#[derive(Debug)]
pub struct SyncGcPage {
    pub next: Option<SyncGcPosition>,
    pub inspected: u32,
    pub removed: usize,
    pub released_bytes: u64,
}

const LIVE_BODY: &str = "EXISTS(SELECT 1 FROM artifacts a WHERE a.message_id=b.message_id
    AND a.kind=1 AND a.mime_version=b.mime_version AND a.html_version=b.html_version
    AND a.state=1 AND a.blob_id=b.blob_id)";

impl Cache {
    /// Inspect a small index page before testing eligibility. Retain `next`
    /// even when there is nothing to remove in this page.
    pub fn collect_sync_garbage(
        &self,
        account: &AccountKey,
        position: Option<SyncGcPosition>,
    ) -> Result<SyncGcPage, SyncError> {
        if !self.full {
            return Err(SyncError::Invalid);
        }
        let namespace = account.reference_namespace();
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let restore_epoch = self.disk.restore_epoch();
        let (phase, after, through) = match position {
            Some(position)
                if position.namespace == namespace && position.restore_epoch == restore_epoch =>
            {
                (position.phase, position.after, position.through)
            }
            Some(_) => return Err(SyncError::Stale),
            None => (0, 0, None),
        };
        let (table, projection, predicate) = match phase {
            0 => (
                "sync_work",
                ",mailbox,uid_validity",
                "EXISTS(SELECT 1 FROM retired_epochs e
                WHERE (e.mailbox,e.uid_validity)=(i.mailbox,i.uid_validity))",
            ),
            1 => (
                "artifacts",
                ",message_id",
                "NOT EXISTS(SELECT 1 FROM messages m JOIN mailboxes f USING(mailbox,uid_validity)
                WHERE m.id=i.message_id AND NOT m.tombstone)",
            ),
            2 => (
                "body_segments",
                ",index_id",
                "NOT EXISTS(SELECT 1 FROM body_indexes b WHERE b.id=i.index_id AND ",
            ),
            3 => (
                "body_indexes",
                "",
                "NOT EXISTS(SELECT 1 FROM body_segments s WHERE s.index_id=i.rowkey)
                AND NOT EXISTS(SELECT 1 FROM body_indexes b WHERE b.id=i.rowkey AND ",
            ),
            4 => (
                "blobs",
                ",id",
                "NOT EXISTS(SELECT 1 FROM artifacts a WHERE a.blob_id=i.id)
                AND NOT EXISTS(SELECT 1 FROM artifacts a WHERE a.raw_id=i.id)",
            ),
            5 => (
                "messages",
                ",mailbox,uid_validity,uid",
                "EXISTS(SELECT 1 FROM retired_epochs e
                WHERE (e.mailbox,e.uid_validity)=(i.mailbox,i.uid_validity))
                AND NOT EXISTS(SELECT 1 FROM artifacts a WHERE a.message_id=i.rowkey)
                AND NOT EXISTS(SELECT 1 FROM body_indexes b WHERE b.message_id=i.rowkey)
                AND NOT EXISTS(SELECT 1 FROM sync_work w
                    WHERE (w.mailbox,w.uid_validity,w.uid)=(i.mailbox,i.uid_validity,i.uid))",
            ),
            _ => return Err(SyncError::Invalid),
        };
        let body_predicate = if matches!(phase, 2 | 3) {
            format!("{predicate}{LIVE_BODY})")
        } else {
            predicate.into()
        };
        let blob_column = if phase == 4 { ",i.id" } else { ",NULL" };
        let through = match through {
            Some(through) => through,
            None => database.connection.query_row(
                &format!("SELECT coalesce(max(rowid),0) FROM {table}"),
                [],
                |row| row.get(0),
            )?,
        };
        let sql = format!(
            "WITH inspected AS MATERIALIZED (
            SELECT rowid AS rowkey{projection} FROM {table}
            WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT ?3
        ) SELECT i.rowkey,({body_predicate}){blob_column} FROM inspected i ORDER BY i.rowkey"
        );
        let (ids, inspected, last) = {
            let mut query = database.connection.prepare_cached(&sql)?;
            let mut rows = query.query(params![after, through, PAGE])?;
            let mut ids = Vec::new();
            let mut inspected = 0;
            let mut last = after;
            while let Some(row) = rows.next()? {
                inspected += 1;
                last = row.get(0)?;
                if row.get(1)? {
                    ids.push((last, row.get::<_, Option<[u8; 16]>>(2)?));
                }
            }
            (ids, inspected, last)
        };
        let mut released_bytes = 0u64;
        let removed = if phase == 4 {
            let mut removed = 0;
            for (_, id) in ids {
                let id = BlobId::from_bytes(id.ok_or(CacheError::Corrupt)?);
                let (cleared, bytes) = self.collect_sync_orphan(namespace, database, id)?;
                removed += usize::from(cleared);
                released_bytes += bytes;
            }
            removed
        } else if ids.is_empty() {
            0
        } else {
            let delete = format!("DELETE FROM {table} WHERE rowid=?1");
            match self.write(namespace, database, |tx| {
                let mut statement = tx.prepare_cached(&delete)?;
                for (id, _) in &ids {
                    if phase == 5 {
                        tx.execute("DELETE FROM message_index WHERE rowid=?1", [id])?;
                    }
                    statement.execute([id])?;
                }
                Ok::<_, SyncError>(ids.len())
            }) {
                Ok(removed) => removed,
                // Continue toward physical orphans even when retired metadata
                // currently has no journal headroom. Retry it on the next sweep.
                Err(SyncError::Cache(CacheError::Quota)) => 0,
                Err(error) => return Err(error),
            }
        };
        let next = if inspected == PAGE && last < through {
            Some(SyncGcPosition {
                namespace,
                restore_epoch,
                phase,
                after: last,
                through: Some(through),
            })
        } else if phase < 5 {
            Some(SyncGcPosition {
                namespace,
                restore_epoch,
                phase: phase + 1,
                after: 0,
                through: None,
            })
        } else {
            None
        };
        Ok(SyncGcPage {
            next,
            inspected,
            removed,
            released_bytes,
        })
    }

    pub(super) fn collect_sync_orphan(
        &self,
        namespace: Namespace,
        database: &mut Database,
        id: BlobId,
    ) -> Result<(bool, u64), CacheError> {
        let id = id.bytes();
        let orphan: bool = database.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM blobs b WHERE b.id=?1
              AND NOT EXISTS(SELECT 1 FROM artifacts a WHERE a.blob_id=b.id)
              AND NOT EXISTS(SELECT 1 FROM artifacts a WHERE a.raw_id=b.id))",
            [id],
            |row| row.get(0),
        )?;
        if !orphan
            || self
                .blob_leases
                .lock()
                .map_err(|_| CacheError::Storage)?
                .contains_key(&(namespace, BlobId::from_bytes(id)))
        {
            return Ok((false, 0));
        }
        let path = self
            .disk
            .blob_path(&database.name, Uuid::from_bytes(id), false)?;
        let bytes = self.disk.remove_blob(&path)?;
        self.disk.forget_blob(&database.name, bytes)?;
        match self.write(namespace, database, |tx| {
            tx.execute("DELETE FROM blobs WHERE id=?1", [id])?;
            Ok(())
        }) {
            // The physical space is available already. A later sweep can
            // remove this now-orphaned row once its journal has room.
            Ok(()) => Ok((true, bytes)),
            Err(CacheError::Quota) => Ok((bytes > 0, bytes)),
            Err(error) => Err(error),
        }
    }
}
