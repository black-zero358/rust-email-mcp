//! Verified, generation-bound file artifacts and their transfer leases.
use super::{
    Cache, CacheError, Database, DirtyStamp, Namespace, PAGE_BYTES, Ref, disk, metadata::message_id,
};
use crate::accounts::AccountKey;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::PathBuf,
    sync::Arc,
};
use uuid::Uuid;

const MAX_TEMP_BYTES: u64 = 32 * 1024 * 1024;
const MAX_BODY_METADATA_BYTES: usize = 16 * 1024;
const MAX_GC_ROWS: u32 = 200;
const IO_BLOCK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BlobId(Uuid);

impl BlobId {
    pub(super) fn bytes(self) -> [u8; 16] {
        *self.0.as_bytes()
    }

    pub(super) fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobInfo {
    pub id: BlobId,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactKey {
    Raw,
    FullBody {
        mime_version: u16,
        html_version: u16,
    },
    Attachment {
        original_index: u32,
        mime_version: u16,
    },
}

impl ArtifactKey {
    pub(super) fn sql(self) -> Result<(u8, u32, u16, u16), CacheError> {
        match self {
            Self::Raw => Ok((0, 0, 0, 0)),
            Self::FullBody {
                mime_version,
                html_version,
            } if mime_version > 0 && html_version > 0 => Ok((1, 0, mime_version, html_version)),
            Self::Attachment {
                original_index,
                mime_version,
            } if mime_version > 0 => Ok((2, original_index, mime_version, 0)),
            _ => Err(CacheError::OutOfScope),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Omission {
    RawLimit = 1,
    DecodedLimit = 2,
    Quota = 3,
    UnsupportedEncoding = 4,
    UnsupportedContent = 5,
}

impl Omission {
    fn decode(value: Option<u8>) -> Result<Self, CacheError> {
        match value {
            Some(1) => Ok(Self::RawLimit),
            Some(2) => Ok(Self::DecodedLimit),
            Some(3) => Ok(Self::Quota),
            Some(4) => Ok(Self::UnsupportedEncoding),
            Some(5) => Ok(Self::UnsupportedContent),
            _ => Err(CacheError::Corrupt),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactState {
    Missing,
    Materialized(BlobInfo),
    Derivable { raw: BlobId },
    Omitted(Omission),
    Unsupported(Omission),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactLayer {
    Raw,
    FullBody {
        mime_version: u16,
        html_version: u16,
    },
    Attachments {
        mime_version: u16,
    },
}

/// Coverage of locally observed messages and attachment indices. A future
/// synchronized scan supplies the separate scope-wide completion evidence.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct LayerCoverage {
    pub materialized: u64,
    pub materialized_bytes: u64,
    pub derivable_from_raw: u64,
    pub missing: u64,
    pub omitted: u64,
    pub unsupported: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Access {
    LiveVerified,
    Offline,
}

struct MessageRef {
    account: AccountKey,
    mailbox: String,
    uid_validity: u32,
    uid: u32,
}

impl From<Ref<'_>> for MessageRef {
    fn from(reference: Ref<'_>) -> Self {
        Self {
            account: reference.account.clone(),
            mailbox: reference.mailbox.into(),
            uid_validity: reference.uid_validity,
            uid: reference.uid,
        }
    }
}

impl MessageRef {
    fn as_ref(&self) -> Ref<'_> {
        Ref {
            account: &self.account,
            mailbox: &self.mailbox,
            uid_validity: self.uid_validity,
            uid: self.uid,
        }
    }
}

/// Owned synchronous staging. I/O and cancellation cleanup belong on a
/// blocking worker, and the reservation follows the stage until deletion.
pub struct BlobStage {
    cache: Arc<Cache>,
    reference: MessageRef,
    kind: ArtifactKey,
    name: String,
    id: BlobId,
    path: PathBuf,
    file: Option<File>,
    sha: Sha256,
    bytes: u64,
    reserved: u64,
    max_len: u64,
    expected_len: Option<u64>,
    restore_epoch: u64,
    failed: bool,
    committed: bool,
}

/// An opened and fully checksum-verified file. The cache keeps it pinned
/// until the reader is dropped. Callers still own the current account ACL.
pub struct BlobLease {
    cache: Arc<Cache>,
    reference: MessageRef,
    kind: ArtifactKey,
    info: BlobInfo,
    body_metadata: Option<Vec<u8>>,
    offline_stamp: Option<DirtyStamp>,
    restore_epoch: u64,
    file: Option<File>,
    remaining: u64,
    invalidated: bool,
}

impl Cache {
    pub fn begin_blob(
        self: &Arc<Self>,
        reference: Ref<'_>,
        kind: ArtifactKey,
        expected_len: Option<u64>,
        max_len: u64,
    ) -> Result<BlobStage, CacheError> {
        if !self.full || reference.uid == 0 || reference.uid_validity == 0 {
            return Err(CacheError::OutOfScope);
        }
        kind.sql()?;
        if max_len > MAX_TEMP_BYTES || expected_len.is_some_and(|bytes| bytes > max_len) {
            return Err(CacheError::Quota);
        }
        self.write_health()?;
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let quota = self.scopes[&namespace].max_bytes;
        let databases = self.lock()?;
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        let name = &database.name;
        self.disk.prepare_blobs(name, quota, self.max_bytes)?;
        let id = BlobId(Uuid::new_v4());
        let path = self.disk.blob_path(name, id.0, true)?;
        let max_len = expected_len.unwrap_or(max_len);
        let reserved = max_len.max(1).next_multiple_of(IO_BLOCK as u64) + PAGE_BYTES;
        self.disk.reserve_blob(name, reserved)?;
        let file = self
            .disk
            .reserve_write(&database.connection, name, quota, self.max_bytes)
            .and_then(|()| self.disk.create_blob(&path));
        let file = match file {
            Ok(file) => file,
            Err(error) => {
                self.disk.settle_blob(name, reserved, 0)?;
                return Err(error);
            }
        };
        Ok(BlobStage {
            cache: Arc::clone(self),
            reference: reference.into(),
            kind,
            name: name.clone(),
            id,
            path,
            file: Some(file),
            sha: Sha256::new(),
            bytes: 0,
            reserved,
            max_len,
            expected_len,
            restore_epoch: self.disk.restore_epoch(),
            failed: false,
            committed: false,
        })
    }

    pub fn open_blob(
        self: &Arc<Self>,
        reference: Ref<'_>,
        kind: ArtifactKey,
        access: Access,
        expected_blob: Option<BlobId>,
    ) -> Result<BlobLease, CacheError> {
        let namespace = self.scope(reference.account, reference.mailbox)?;
        if !self.full {
            return Err(CacheError::OutOfScope);
        }
        let mut lease = {
            let mut databases = self.lock()?;
            let database = databases
                .get_mut(&namespace)
                .ok_or(CacheError::OutOfScope)?;
            let offline_stamp = (access == Access::Offline)
                .then(|| {
                    self.check_offline(
                        &database.connection,
                        reference.mailbox,
                        reference.uid_validity,
                        namespace,
                    )
                })
                .transpose()?;
            let ArtifactState::Materialized(info) =
                artifact(&database.connection, reference, kind)?
            else {
                return Err(CacheError::OutOfScope);
            };
            if expected_blob.is_some_and(|id| info.id != id) {
                return Err(CacheError::OutOfScope);
            }
            let body_metadata = if matches!(kind, ArtifactKey::FullBody { .. }) {
                let record: Option<(Vec<u8>, [u8; 32])> = database.connection.query_row(
                    "SELECT payload,digest FROM body_leaf_metadata WHERE blob_id=?1 AND length(payload)<=16384",
                    [info.id.0.as_bytes()], |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?;
                match record {
                    Some((payload, digest)) if body_metadata_digest(info, &payload) == digest => {
                        Some(payload)
                    }
                    Some(_) => {
                        self.quarantine_blob(namespace, database, info.id)?;
                        return Err(CacheError::Corrupt);
                    }
                    None => None,
                }
            } else {
                None
            };
            let path = self.disk.blob_path(&database.name, info.id.0, false)?;
            let file = match self.disk.open_blob_file(&path) {
                Err(CacheError::Corrupt) => {
                    self.quarantine_blob(namespace, database, info.id)?;
                    return Err(CacheError::Corrupt);
                }
                result => result?,
            };
            let mut leases = self.blob_leases.lock().map_err(|_| CacheError::Storage)?;
            let count = leases.entry((namespace, info.id)).or_default();
            *count = count.checked_add(1).ok_or(CacheError::Quota)?;
            BlobLease {
                cache: Arc::clone(self),
                reference: reference.into(),
                kind,
                info,
                body_metadata,
                offline_stamp,
                restore_epoch: self.disk.restore_epoch(),
                file: Some(file),
                remaining: info.bytes,
                invalidated: false,
            }
        };
        if let Err(error) = lease.verify_contents() {
            if matches!(error, CacheError::Corrupt) {
                let mut databases = self.lock()?;
                let database = databases
                    .get_mut(&namespace)
                    .ok_or(CacheError::OutOfScope)?;
                self.quarantine_blob(namespace, database, lease.info.id)?;
            }
            return Err(error);
        }
        lease.revalidate()?;
        Ok(lease)
    }

    pub fn artifact_state(
        &self,
        reference: Ref<'_>,
        kind: ArtifactKey,
    ) -> Result<ArtifactState, CacheError> {
        let namespace = self.scope(reference.account, reference.mailbox)?;
        if !self.full {
            return Err(CacheError::OutOfScope);
        }
        let databases = self.lock()?;
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        artifact(&database.connection, reference, kind)
    }

    pub fn record_artifact_state(
        &self,
        reference: Ref<'_>,
        kind: ArtifactKey,
        state: ArtifactState,
    ) -> Result<(), CacheError> {
        self.record_artifact_states(reference, &[(kind, state)])
    }

    pub fn record_artifact_states(
        &self,
        reference: Ref<'_>,
        states: &[(ArtifactKey, ArtifactState)],
    ) -> Result<(), CacheError> {
        if !self.full || states.len() > 1025 {
            return Err(CacheError::OutOfScope);
        }
        if states.is_empty() {
            return Ok(());
        }
        self.update_message(reference, |tx, message| {
            apply_artifact_states(tx, message, states)
        })
    }

    pub fn remember_full_manifest(
        &self,
        reference: Ref<'_>,
        parts: &[u8],
        expected: &[(ArtifactKey, ArtifactState)],
    ) -> Result<(), CacheError> {
        if !self.full
            || parts.is_empty()
            || parts.len() > 256 * 1024
            || expected.len() > 1024
            || expected
                .iter()
                .any(|(kind, _)| !matches!(kind, ArtifactKey::Attachment { .. }))
        {
            return Err(CacheError::OutOfScope);
        }
        self.update_message(reference, |tx, message| {
            apply_artifact_states(tx, message, expected)?;
            tx.execute(
                "UPDATE messages SET parts=?2 WHERE id=?1",
                params![message, parts],
            )?;
            Ok(())
        })
    }

    /// Reclaim a reconstructible copy while retaining its verified raw
    /// dependency. An active reader keeps its materialization until it exits.
    /// The caller supplies successful derivation evidence for this version.
    pub fn evict_derived(
        self: &Arc<Self>,
        reference: Ref<'_>,
        kind: ArtifactKey,
        expected: BlobId,
        raw: BlobId,
    ) -> Result<bool, CacheError> {
        self.evict_derived_inner(reference, kind, expected, raw, None)
    }

    pub(super) fn evict_derived_inner(
        self: &Arc<Self>,
        reference: Ref<'_>,
        kind: ArtifactKey,
        expected: BlobId,
        raw: BlobId,
        expected_recency: Option<i64>,
    ) -> Result<bool, CacheError> {
        let (kind_code, part, mime, html) = kind.sql()?;
        if !self.full || kind_code == 0 {
            return Err(CacheError::OutOfScope);
        }
        let sync_layer = match kind {
            ArtifactKey::FullBody {
                mime_version: 1,
                html_version: 1,
            } => Some(3),
            ArtifactKey::Attachment {
                mime_version: 1, ..
            } => Some(5),
            _ => None,
        };
        if expected_recency.is_some() && sync_layer.is_none() {
            return Err(CacheError::OutOfScope);
        }
        let _raw = self.open_blob(reference, ArtifactKey::Raw, Access::LiveVerified, Some(raw))?;
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        if self
            .blob_leases
            .lock()
            .map_err(|_| CacheError::Storage)?
            .contains_key(&(namespace, expected))
        {
            return Ok(false);
        }
        self.write(namespace, database, |tx| {
            if !matches!(artifact(tx, reference, ArtifactKey::Raw)?, ArtifactState::Materialized(info) if info.id == raw)
                || !matches!(artifact(tx, reference, kind)?, ArtifactState::Materialized(info) if info.id == expected)
            {
                return Ok(false);
            }
            if let Some(seq) = expected_recency {
                let unchanged: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM blob_recency WHERE blob_id=?1 AND seq=?2)",
                    params![expected.bytes(),seq],
                    |row| row.get(0),
                )?;
                if !unchanged {
                    return Ok(false);
                }
            }
            let message = present_message(tx, reference)?;
            tx.execute(
                "UPDATE artifacts SET state=2,blob_id=NULL,raw_id=?6
                 WHERE message_id=?1 AND kind=?2 AND part=?3 AND mime_version=?4 AND html_version=?5",
                params![message, kind_code, part, mime, html, raw.0.as_bytes()],
            )?;
            if let Some(layer) = sync_layer.filter(|_| expected_recency.is_some()) {
                tx.execute(
                    "UPDATE sync_work SET state=4,reason=?6,not_before_ms=0,revision=revision+1
                     WHERE mailbox=?1 AND uid_validity=?2 AND uid=?3 AND layer=?4 AND part=?5 AND state=0",
                    params![reference.mailbox, reference.uid_validity, reference.uid, layer, part, Omission::Quota as u8],
                )?;
            }
            Ok(true)
        })
    }

    pub fn layer_coverage(
        &self,
        account: &AccountKey,
        mailbox: &str,
        uid_validity: u32,
        layer: ArtifactLayer,
    ) -> Result<LayerCoverage, CacheError> {
        if !self.full {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        layer_coverage_at(
            &database.connection,
            mailbox,
            uid_validity,
            u32::MAX,
            layer,
            false,
        )
    }

    pub(super) fn recover_blobs(
        &self,
        namespace: Namespace,
        database: &mut Database,
    ) -> Result<(), CacheError> {
        let path = self.disk.blob_root(&database.name)?;
        if path.try_exists()? {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let (id, staged) =
                    disk::parse_blob_name(&entry.file_name()).ok_or(CacheError::Corrupt)?;
                let known: bool = !staged
                    && database.connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM blobs WHERE id=?1)",
                        [id.as_bytes()],
                        |row| row.get(0),
                    )?;
                if !known {
                    let bytes = self.disk.remove_blob(&entry.path())?;
                    self.disk.forget_blob(&database.name, bytes)?;
                }
            }
        }
        let mut after = None::<[u8; 16]>;
        loop {
            let batch = {
                let mut query = database.connection.prepare(
                    "SELECT id,bytes,
                       EXISTS(SELECT 1 FROM artifacts WHERE blob_id=blobs.id)
                       OR EXISTS(SELECT 1 FROM artifacts WHERE raw_id=blobs.id)
                     FROM blobs WHERE id>coalesce(?1,x'') ORDER BY id LIMIT ?2",
                )?;
                query
                    .query_map(params![after, MAX_GC_ROWS], |row| {
                        Ok((
                            row.get::<_, [u8; 16]>(0)?,
                            row.get::<_, u32>(1)?,
                            row.get::<_, bool>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
            };
            if batch.is_empty() {
                break;
            }
            for (id, expected, referenced) in batch {
                after = Some(id);
                let path = self
                    .disk
                    .blob_path(&database.name, Uuid::from_bytes(id), false)?;
                let present = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata.len() == u64::from(expected),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error.into()),
                };
                if !present || !referenced {
                    self.write(namespace, database, |tx| {
                        tx.execute(
                            "UPDATE artifacts SET state=0,blob_id=NULL,raw_id=NULL,reason=NULL
                             WHERE blob_id=?1 OR raw_id=?1",
                            [id],
                        )?;
                        tx.execute("DELETE FROM blobs WHERE id=?1", [id])?;
                        Ok::<_, CacheError>(())
                    })?;
                    let bytes = self.disk.remove_blob(&path)?;
                    if bytes != 0 {
                        self.disk.forget_blob(&database.name, bytes)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn quarantine_blob(
        &self,
        namespace: Namespace,
        database: &mut Database,
        blob: BlobId,
    ) -> Result<(), CacheError> {
        let result = self.write(namespace, database, |tx| {
            tx.execute(
                "UPDATE artifacts SET state=0,blob_id=NULL,raw_id=NULL,reason=NULL
                 WHERE blob_id=?1 OR raw_id=?1",
                [blob.0.as_bytes()],
            )?;
            Ok::<_, CacheError>(())
        });
        if result.is_err() {
            self.disable_offline();
        }
        result
    }

    /// Bounded sweep. Retired generations cannot acquire leases; already-open
    /// file readers hold their physical charge until the last lease is gone.
    pub fn collect_blobs(&self, account: &AccountKey) -> Result<usize, CacheError> {
        if !self.full {
            return Err(CacheError::OutOfScope);
        }
        let namespace = account.reference_namespace();
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let retired = self.write(namespace, database, |tx| {
            tx.execute(
                "DELETE FROM artifacts WHERE rowid IN (
                    SELECT a.rowid FROM artifacts AS a JOIN messages AS m ON m.id=a.message_id
                    WHERE m.tombstone OR NOT EXISTS(
                        SELECT 1 FROM mailboxes AS f WHERE f.mailbox=m.mailbox AND f.uid_validity=m.uid_validity)
                    LIMIT ?1
                 )",
                [MAX_GC_ROWS],
            )?;
            tx.execute(
                "DELETE FROM body_segments WHERE id IN (
                    SELECT s.id FROM body_segments s JOIN body_indexes b ON b.id=s.index_id
                    WHERE NOT EXISTS(
                        SELECT 1 FROM artifacts a WHERE a.message_id=b.message_id AND a.kind=1
                          AND a.mime_version=b.mime_version AND a.html_version=b.html_version
                          AND a.state=1 AND a.blob_id=b.blob_id)
                    LIMIT 16
                 )",
                [],
            )?;
            tx.execute(
                "DELETE FROM body_indexes WHERE id IN (
                    SELECT b.id FROM body_indexes b
                    WHERE NOT EXISTS(SELECT 1 FROM body_segments s WHERE s.index_id=b.id)
                    AND NOT EXISTS(
                        SELECT 1 FROM artifacts a WHERE a.message_id=b.message_id AND a.kind=1
                          AND a.mime_version=b.mime_version AND a.html_version=b.html_version
                          AND a.state=1 AND a.blob_id=b.blob_id)
                    LIMIT ?1
                 )",
                [MAX_GC_ROWS],
            )?;
            Ok::<_, CacheError>(())
        });
        // Already-unreferenced files can free physical space even when a
        // different partition left too little global room for this journal.
        // Keep their DB rows until after unlink, so recovery can finish it.
        let lacked_journal_room = match retired {
            Ok(()) => false,
            Err(CacheError::Quota) => true,
            Err(error) => return Err(error),
        };
        let candidates = {
            let mut query = database.connection.prepare(
                "SELECT id FROM blobs
                 WHERE NOT EXISTS(SELECT 1 FROM artifacts WHERE blob_id=id)
                   AND NOT EXISTS(SELECT 1 FROM artifacts WHERE raw_id=id)
                 LIMIT ?1",
            )?;
            query
                .query_map([MAX_GC_ROWS], |row| row.get::<_, [u8; 16]>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut removed = Vec::new();
        for id in candidates {
            let blob = BlobId(Uuid::from_bytes(id));
            if self
                .blob_leases
                .lock()
                .map_err(|_| CacheError::Storage)?
                .contains_key(&(namespace, blob))
            {
                continue;
            }
            let path = self.disk.blob_path(&database.name, blob.0, false)?;
            let bytes = self.disk.remove_blob(&path)?;
            self.disk.forget_blob(&database.name, bytes)?;
            removed.push(id);
        }
        if removed.is_empty() {
            return if lacked_journal_room {
                Err(CacheError::Quota)
            } else {
                Ok(0)
            };
        }
        self.write(namespace, database, |tx| {
            for id in &removed {
                tx.execute("DELETE FROM blobs WHERE id=?1", [id])?;
            }
            Ok(removed.len())
        })
    }
}

fn apply_artifact_states(
    tx: &rusqlite::Transaction<'_>,
    message: i64,
    states: &[(ArtifactKey, ArtifactState)],
) -> Result<(), CacheError> {
    let raw: Option<[u8; 16]> = tx
        .query_row(
            "SELECT blob_id FROM artifacts WHERE message_id=?1 AND kind=0 AND state=1",
            [message],
            |row| row.get(0),
        )
        .optional()?;
    let mut keys = std::collections::BTreeSet::new();
    let mut insert = tx.prepare_cached(
        "INSERT INTO artifacts VALUES(?1,?2,?3,?4,?5,?6,NULL,?7,?8)
         ON CONFLICT(message_id,kind,part,mime_version,html_version)
         DO UPDATE SET state=excluded.state,raw_id=excluded.raw_id,reason=excluded.reason
         WHERE excluded.state<>0 AND artifacts.state<>1
           AND (artifacts.state NOT IN (2,4) OR excluded.state=2)",
    )?;
    for (kind, state) in states {
        let key = kind.sql()?;
        if !keys.insert(key) {
            return Err(CacheError::OutOfScope);
        }
        let (kind, part, mime, html) = key;
        let (state, raw_id, reason) = match *state {
            ArtifactState::Missing => (0, None, None),
            ArtifactState::Derivable { raw: id } if kind != 0 && Some(id.bytes()) == raw => {
                (2, raw, None)
            }
            ArtifactState::Omitted(
                reason @ (Omission::RawLimit | Omission::DecodedLimit | Omission::Quota),
            ) => (3, None, Some(reason as u8)),
            ArtifactState::Unsupported(
                reason @ (Omission::UnsupportedEncoding | Omission::UnsupportedContent),
            ) => (4, None, Some(reason as u8)),
            _ => return Err(CacheError::OutOfScope),
        };
        insert.execute(params![
            message, kind, part, mime, html, state, raw_id, reason
        ])?;
    }
    Ok(())
}

impl BlobStage {
    /// Call only with verified, complete evidence after protocol completion.
    pub fn publish(self, cancelled: &dyn Fn() -> bool) -> Result<BlobInfo, CacheError> {
        self.publish_inner(None, cancelled)
    }

    pub fn publish_body(
        self,
        leaf_metadata: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<BlobInfo, CacheError> {
        if !matches!(self.kind, ArtifactKey::FullBody { .. })
            || leaf_metadata.is_empty()
            || leaf_metadata.len() > MAX_BODY_METADATA_BYTES
        {
            return Err(CacheError::OutOfScope);
        }
        self.publish_inner(Some(leaf_metadata), cancelled)
    }

    fn publish_inner(
        mut self,
        body_metadata: Option<&[u8]>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<BlobInfo, CacheError> {
        if self.failed || self.expected_len.is_some_and(|len| len != self.bytes) {
            return Err(CacheError::Corrupt);
        }
        if cancelled() {
            return Err(CacheError::OutOfScope);
        }
        self.file.take().ok_or(CacheError::Storage)?.sync_all()?;
        let stored = self.cache.disk.blob_file_bytes(&self.path)?;
        if stored > self.reserved.saturating_sub(PAGE_BYTES) {
            return Err(CacheError::Quota);
        }
        let final_path = self.cache.disk.blob_path(&self.name, self.id.0, false)?;
        self.cache.disk.rename_blob(&self.path, &final_path)?;
        self.path = final_path;
        self.cache.disk.sync_blob_directory(&self.path)?;
        let info = BlobInfo {
            id: self.id,
            bytes: self.bytes,
            sha256: self.sha.clone().finalize().into(),
        };
        let reference = self.reference.as_ref();
        let namespace = reference.account.reference_namespace();
        let (kind, part, mime, html) = self.kind.sql()?;
        let mut replace_legacy = None;
        for _ in 0..3 {
            let mut databases = self.cache.lock()?;
            if self.restore_epoch != self.cache.disk.restore_epoch() {
                return Err(CacheError::NeedsVerification);
            }
            let database = databases
                .get_mut(&namespace)
                .ok_or(CacheError::OutOfScope)?;
            if let ArtifactState::Materialized(existing) =
                artifact(&database.connection, reference, self.kind)?
                && replace_legacy != Some(existing.id)
            {
                if existing.bytes != info.bytes || existing.sha256 != info.sha256 {
                    return Err(CacheError::Corrupt);
                }
                drop(databases);
                match self.cache.open_blob(
                    reference,
                    self.kind,
                    Access::LiveVerified,
                    Some(existing.id),
                ) {
                    Ok(lease) if lease.restore_epoch == self.restore_epoch && !cancelled() => {
                        match (body_metadata, lease.body_metadata()) {
                            (Some(_), None) => {
                                replace_legacy = Some(existing.id);
                                continue;
                            }
                            (Some(next), Some(prior)) if next != prior => {
                                return Err(CacheError::Corrupt);
                            }
                            _ => return Ok(existing),
                        }
                    }
                    Ok(_) => return Err(CacheError::OutOfScope),
                    Err(CacheError::Corrupt | CacheError::OutOfScope) => continue,
                    Err(error) => return Err(error),
                }
            }
            self.cache.write_checked(
                namespace,
                database,
                |tx| {
                    let message = present_message(tx, reference)?;
                    if self.kind == ArtifactKey::Raw {
                        let fingerprint: Option<(u32, [u8; 32])> = tx
                            .query_row(
                                "SELECT bytes,sha256 FROM raw_fingerprints WHERE message_id=?1",
                                [message],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()?;
                        match fingerprint {
                            Some((length, sha))
                                if u64::from(length) != info.bytes || sha != info.sha256 =>
                            {
                                return Err(CacheError::Corrupt);
                            }
                            Some(_) => {}
                            None => {
                                tx.execute(
                                    "INSERT INTO raw_fingerprints VALUES(?1,?2,?3)",
                                    params![message, info.bytes as u32, info.sha256],
                                )?;
                            }
                        }
                    }
                    tx.execute(
                        "INSERT INTO blobs VALUES(?1,?2,?3)",
                        params![info.id.0.as_bytes(), info.bytes as u32, info.sha256],
                    )?;
                    if let Some(metadata) = body_metadata {
                        tx.execute(
                            "INSERT INTO body_leaf_metadata VALUES(?1,?2,?3)",
                            params![
                                info.id.0.as_bytes(),
                                metadata,
                                body_metadata_digest(info, metadata)
                            ],
                        )?;
                    }
                    tx.execute(
                        "INSERT INTO artifacts VALUES(?1,?2,?3,?4,?5,1,?6,NULL,NULL)
                     ON CONFLICT(message_id,kind,part,mime_version,html_version) DO UPDATE
                     SET state=1,blob_id=excluded.blob_id,raw_id=NULL,reason=NULL",
                        params![message, kind, part, mime, html, info.id.0.as_bytes()],
                    )?;
                    Ok(())
                },
                || {
                    if cancelled() {
                        Err(CacheError::OutOfScope)
                    } else {
                        Ok(())
                    }
                },
            )?;
            self.committed = true;
            if self
                .cache
                .disk
                .settle_blob(&self.name, self.reserved, stored)
                .is_err()
            {
                self.cache.disable_offline();
            }
            return Ok(info);
        }
        Err(CacheError::Storage)
    }
}

fn body_metadata_digest(info: BlobInfo, metadata: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"email-mcp-body-leaf-metadata-v1\0");
    hash.update(info.id.bytes());
    hash.update(info.sha256);
    hash.update(metadata);
    hash.finalize().into()
}

impl Write for BlobStage {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed || bytes.len() as u64 > self.max_len.saturating_sub(self.bytes) {
            self.failed = true;
            return Err(io::Error::other("Cache artifact limit exceeded."));
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("Cache artifact is closed."))?;
        match file.write(bytes) {
            Ok(count) => {
                self.sha.update(&bytes[..count]);
                self.bytes += count as u64;
                Ok(count)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("Cache artifact is closed."))?
            .flush()
    }
}

impl Drop for BlobStage {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.committed
            && self
                .cache
                .disk
                .blob_root(&self.name)
                .and_then(|_| self.cache.disk.remove_blob(&self.path))
                .is_ok()
            && self
                .cache
                .disk
                .settle_blob(&self.name, self.reserved, 0)
                .is_err()
        {
            self.cache.disable_offline();
        }
    }
}

impl BlobLease {
    pub fn info(&self) -> BlobInfo {
        self.info
    }

    /// Record successful foreground use of a verified derived copy. A
    /// recency write failure leaves the caller's already-verified read valid.
    pub fn touch(&self) {
        if self.kind != ArtifactKey::Raw && !self.invalidated {
            let _ = self.touch_inner();
        }
    }

    fn touch_inner(&self) -> Result<(), CacheError> {
        let namespace = self.reference.account.reference_namespace();
        let mut databases = self.cache.lock()?;
        if self.restore_epoch != self.cache.disk.restore_epoch() {
            return Ok(());
        }
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let id = self.info.id.bytes();
        self.cache.write(namespace, database, |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO blob_recency
                 (blob_id,mailbox,uid_validity,message_id,kind,part,mime_version,html_version)
                 SELECT blob_id,mailbox,uid_validity,message_id,kind,part,mime_version,html_version
                 FROM blob_recency WHERE blob_id=?1",
                [id],
            )?;
            Ok(())
        })
    }

    pub fn body_metadata(&self) -> Option<&[u8]> {
        self.body_metadata.as_deref()
    }

    /// A prepared offline handle must bind this stamp as well as the blob id.
    pub fn offline_stamp(&self) -> Option<&DirtyStamp> {
        self.offline_stamp.as_ref()
    }

    pub fn revalidate(&self) -> Result<(), CacheError> {
        let reference = self.reference.as_ref();
        let namespace = reference.account.reference_namespace();
        let databases = self.cache.lock()?;
        if self.restore_epoch != self.cache.disk.restore_epoch() {
            return Err(CacheError::NeedsVerification);
        }
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        if let Some(expected) = &self.offline_stamp {
            let current = self.cache.check_offline(
                &database.connection,
                reference.mailbox,
                reference.uid_validity,
                namespace,
            )?;
            if &current != expected {
                return Err(CacheError::NeedsVerification);
            }
        }
        if artifact(&database.connection, reference, self.kind)?
            != ArtifactState::Materialized(self.info)
        {
            return Err(CacheError::OutOfScope);
        }
        Ok(())
    }

    fn verify_contents(&mut self) -> Result<(), CacheError> {
        let file = self.file.as_mut().ok_or(CacheError::Storage)?;
        if file.metadata()?.len() != self.info.bytes {
            return Err(CacheError::Corrupt);
        }
        let mut sha = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0; IO_BLOCK];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            if bytes > self.info.bytes {
                return Err(CacheError::Corrupt);
            }
            sha.update(&buffer[..count]);
        }
        if bytes != self.info.bytes || <[u8; 32]>::from(sha.finalize()) != self.info.sha256 {
            return Err(CacheError::Corrupt);
        }
        file.rewind()?;
        Ok(())
    }
}

impl Read for BlobLease {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.check_read_fence()?;
        let limit = output.len().min(IO_BLOCK).min(self.remaining as usize);
        if limit == 0 {
            return Ok(0);
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("Cache artifact is closed."))?;
        let mut block = [0; IO_BLOCK];
        let count = file.read(&mut block[..limit])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Cache artifact is incomplete.",
            ));
        }
        self.check_read_fence()?;
        output[..count].copy_from_slice(&block[..count]);
        self.remaining -= count as u64;
        Ok(count)
    }
}

impl BlobLease {
    fn check_read_fence(&mut self) -> io::Result<()> {
        if self.invalidated {
            return Err(io::Error::other(CacheError::OutOfScope));
        }
        if let Err(error) = self.revalidate() {
            self.invalidated = true;
            return Err(io::Error::other(error));
        }
        Ok(())
    }
}

impl Drop for BlobLease {
    fn drop(&mut self) {
        drop(self.file.take());
        let namespace = self.reference.account.reference_namespace();
        if let Ok(mut leases) = self.cache.blob_leases.lock() {
            let key = (namespace, self.info.id);
            if let Some(count) = leases.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    leases.remove(&key);
                }
            }
        }
    }
}

pub(super) fn layer_coverage_at(
    db: &Connection,
    mailbox: &str,
    uid_validity: u32,
    high_uid: u32,
    layer: ArtifactLayer,
    require_body_metadata: bool,
) -> Result<LayerCoverage, CacheError> {
    let key = match layer {
        ArtifactLayer::Raw => ArtifactKey::Raw,
        ArtifactLayer::FullBody {
            mime_version,
            html_version,
        } => ArtifactKey::FullBody {
            mime_version,
            html_version,
        },
        ArtifactLayer::Attachments { mime_version } => ArtifactKey::Attachment {
            original_index: 0,
            mime_version,
        },
    };
    let (kind, _, mime, html) = key.sql()?;
    Ok(db.query_row(
        "SELECT count(*) FILTER(WHERE a.state=1),coalesce(sum(b.bytes),0),
                count(*) FILTER(WHERE a.state=2),
                count(*) FILTER(WHERE coalesce(a.state,0)=0),
                count(*) FILTER(WHERE a.state=3),count(*) FILTER(WHERE a.state=4)
         FROM messages AS m JOIN mailboxes AS f USING(mailbox,uid_validity)
         LEFT JOIN artifacts AS a ON a.message_id=m.id AND a.kind=?3
              AND a.mime_version=?4 AND a.html_version=?5
              AND (NOT ?7 OR a.kind<>1 OR a.state<>1 OR EXISTS(
                  SELECT 1 FROM body_leaf_metadata d WHERE d.blob_id=a.blob_id))
         LEFT JOIN blobs AS b ON b.id=a.blob_id
         WHERE m.mailbox=?1 AND m.uid_validity=?2 AND NOT m.tombstone AND m.uid<=?6
           AND (?3<>2 OR a.kind IS NOT NULL)",
        params![
            mailbox,
            uid_validity,
            kind,
            mime,
            html,
            high_uid,
            require_body_metadata
        ],
        |row| {
            Ok(LayerCoverage {
                materialized: row.get::<_, i64>(0)? as u64,
                materialized_bytes: row.get::<_, i64>(1)? as u64,
                derivable_from_raw: row.get::<_, i64>(2)? as u64,
                missing: row.get::<_, i64>(3)? as u64,
                omitted: row.get::<_, i64>(4)? as u64,
                unsupported: row.get::<_, i64>(5)? as u64,
            })
        },
    )?)
}

pub(super) fn present_message(
    tx: &rusqlite::Transaction<'_>,
    reference: Ref<'_>,
) -> Result<i64, CacheError> {
    let id = message_id(tx, reference)?;
    let tombstone: bool =
        tx.query_row("SELECT tombstone FROM messages WHERE id=?1", [id], |row| {
            row.get(0)
        })?;
    if tombstone {
        return Err(CacheError::OutOfScope);
    }
    Ok(id)
}

pub(super) fn artifact(
    db: &Connection,
    reference: Ref<'_>,
    kind: ArtifactKey,
) -> Result<ArtifactState, CacheError> {
    let (kind, part, mime, html) = kind.sql()?;
    let row = db
        .query_row(
            "SELECT a.state,a.reason,a.raw_id,b.id,b.bytes,b.sha256
         FROM artifacts AS a JOIN messages AS m ON m.id=a.message_id
         JOIN mailboxes AS f USING(mailbox,uid_validity)
         LEFT JOIN blobs AS b ON b.id=a.blob_id
         WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid=?3 AND NOT m.tombstone
           AND a.kind=?4 AND a.part=?5 AND a.mime_version=?6 AND a.html_version=?7",
            params![
                reference.mailbox,
                reference.uid_validity,
                reference.uid,
                kind,
                part,
                mime,
                html
            ],
            |row| {
                Ok((
                    row.get::<_, u8>(0)?,
                    row.get(1)?,
                    row.get::<_, Option<[u8; 16]>>(2)?,
                    row.get::<_, Option<[u8; 16]>>(3)?,
                    row.get::<_, Option<u32>>(4)?,
                    row.get::<_, Option<[u8; 32]>>(5)?,
                ))
            },
        )
        .optional()?;
    match row {
        None | Some((0, None, None, None, None, None)) => Ok(ArtifactState::Missing),
        Some((1, None, None, Some(id), Some(bytes), Some(sha256)))
            if u64::from(bytes) <= MAX_TEMP_BYTES =>
        {
            Ok(ArtifactState::Materialized(BlobInfo {
                id: BlobId(Uuid::from_bytes(id)),
                bytes: bytes.into(),
                sha256,
            }))
        }
        Some((2, None, Some(raw), None, None, None)) => Ok(ArtifactState::Derivable {
            raw: BlobId(Uuid::from_bytes(raw)),
        }),
        Some((3, reason, None, None, None, None)) => {
            Ok(ArtifactState::Omitted(Omission::decode(reason)?))
        }
        Some((4, reason, None, None, None, None)) => {
            Ok(ArtifactState::Unsupported(Omission::decode(reason)?))
        }
        _ => Err(CacheError::Corrupt),
    }
}
