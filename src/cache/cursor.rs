//! Transactional, bounded streaming storage for cross-folder conversation state.
use super::{Cache, CacheError, valid_mailbox};
use rusqlite::{Row, blob::ZeroBlob, params};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{self, Read, Write},
    sync::Arc,
};

const MAX_CURSORS: usize = 64;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_FROZEN: usize = 20;
const TTL_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CursorBinding {
    pub namespace: [u8; 32],
    pub principal: [u8; 32],
}

#[derive(Clone, Copy)]
pub struct CursorExpected {
    pub token: [u8; 16],
    pub revision: u64,
}

#[derive(Clone, Copy)]
pub struct CursorScope<'a> {
    pub mailbox: &'a str,
    pub uid_validity: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorRow {
    pub active: Option<[u8; 16]>,
    pub replay: Option<[u8; 16]>,
    pub revision: u64,
    pub codec_version: u16,
    pub owned_bytes: usize,
    pub encoded_len: usize,
    pub expires_ms: i64,
    pub last_progress_ms: i64,
}

pub struct CursorWrite<'a> {
    pub binding: CursorBinding,
    pub expected: Option<CursorExpected>,
    pub active: Option<[u8; 16]>,
    pub replay: Option<[u8; 16]>,
    pub codec_version: u16,
    pub owned_bytes: usize,
    pub encoded_len: usize,
    pub expires_ms: i64,
    pub last_progress_ms: i64,
    pub now_ms: i64,
    pub frozen: &'a [CursorScope<'a>],
    pub cancelled: &'a dyn Fn() -> bool,
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum CursorError {
    #[error("Cursor is unavailable.")]
    Unavailable,
    #[error("Cursor revision changed.")]
    Conflict,
    #[error("Cursor quota is exhausted.")]
    Quota,
    #[error("Cursor content is invalid.")]
    Corrupt,
    #[error("Cursor storage is unavailable.")]
    Storage,
}

impl From<CacheError> for CursorError {
    fn from(error: CacheError) -> Self {
        match error {
            CacheError::OutOfScope => Self::Unavailable,
            CacheError::NeedsVerification => Self::Unavailable,
            CacheError::Quota => Self::Quota,
            CacheError::Corrupt => Self::Corrupt,
            CacheError::Storage => Self::Storage,
        }
    }
}

impl From<rusqlite::Error> for CursorError {
    fn from(error: rusqlite::Error) -> Self {
        CacheError::from(error).into()
    }
}

#[derive(Clone)]
pub struct CursorBackend(pub(super) Arc<Cache>);

const ROW: &str = "id,principal,active,replay,revision,codec_version,owned_bytes,encoded_len,
    expires_ms,last_progress_ms,digest,length(payload)";
const SELECT_TOKEN: &str = " FROM conversation_cursors
    WHERE principal=?1 AND (active=?2 OR replay=?2) AND expires_ms>?3";

struct StoredRow {
    id: i64,
    principal: [u8; 32],
    row: CursorRow,
    digest: [u8; 32],
    blob_len: usize,
}

impl CursorBackend {
    pub fn reservations(
        &self,
        now_ms: i64,
    ) -> Result<Vec<(CursorBinding, CursorRow)>, CursorError> {
        let databases = self.0.lock()?;
        let mut reservations = Vec::new();
        let mut owned = 0usize;
        let mut encoded = 0usize;
        for (namespace, database) in databases.iter() {
            let sql = format!(
                "SELECT {ROW} FROM conversation_cursors WHERE expires_ms>?1 ORDER BY id LIMIT ?2"
            );
            let mut query = database.connection.prepare_cached(&sql)?;
            let mut rows = query.query(params![now_ms, (MAX_CURSORS + 1) as u32])?;
            while let Some(value) = rows.next()? {
                let stored = read_row(value)?;
                verify_row(&stored, now_ms)?;
                owned = owned.saturating_add(stored.row.owned_bytes);
                encoded = encoded.saturating_add(stored.row.encoded_len);
                if reservations.len() == MAX_CURSORS || owned > MAX_BYTES || encoded > MAX_BYTES {
                    return Err(CursorError::Quota);
                }
                reservations.push((
                    CursorBinding {
                        namespace: *namespace,
                        principal: stored.principal,
                    },
                    stored.row,
                ));
            }
        }
        Ok(reservations)
    }

    pub fn validate(
        &self,
        binding: CursorBinding,
        token: [u8; 16],
        now_ms: i64,
    ) -> Result<CursorRow, CursorError> {
        let databases = self.0.lock()?;
        let database = databases
            .get(&binding.namespace)
            .ok_or(CursorError::Unavailable)?;
        Ok(lookup(&database.connection, binding, token, now_ms)?.row)
    }

    pub fn load<T>(
        &self,
        binding: CursorBinding,
        token: [u8; 16],
        now_ms: i64,
        decode: impl FnOnce(CursorRow, &mut dyn Read) -> Result<T, CursorError>,
    ) -> Result<T, CursorError> {
        let databases = self.0.lock()?;
        let database = databases
            .get(&binding.namespace)
            .ok_or(CursorError::Unavailable)?;
        let connection = &database.connection;
        let stored = lookup(connection, binding, token, now_ms)?;
        let mut hash = digest_prefix(binding, stored.row);
        let mut statement = connection.prepare_cached(
            "SELECT mailbox,uid_validity FROM cursor_scopes WHERE cursor_id=?1 ORDER BY mailbox LIMIT ?2"
        )?;
        let mut rows = statement.query(params![stored.id, (MAX_FROZEN + 1) as u32])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            let mailbox = row.get_ref(0)?.as_str().map_err(|_| CursorError::Corrupt)?;
            let uid_validity: u32 = row.get(1)?;
            if count > MAX_FROZEN || !valid_mailbox(mailbox) || uid_validity == 0 {
                return Err(CursorError::Corrupt);
            }
            hash_scope(&mut hash, mailbox, uid_validity);
        }
        if count == 0 {
            return Err(CursorError::Corrupt);
        }
        hash.update([0]);
        let blob =
            connection.blob_open("main", "conversation_cursors", "payload", stored.id, true)?;
        if blob.len() != stored.row.encoded_len {
            return Err(CursorError::Corrupt);
        }
        let mut stream = HashStream {
            inner: blob,
            hash,
            remaining: stored.row.encoded_len,
        };
        let result = decode(stored.row, &mut stream)?;
        let digest: [u8; 32] = stream.hash.finalize().into();
        if stream.remaining != 0 || digest != stored.digest {
            return Err(CursorError::Corrupt);
        }
        Ok(result)
    }

    pub fn save(
        &self,
        write: CursorWrite<'_>,
        encode: impl FnOnce(&mut dyn Write) -> Result<(), CursorError>,
    ) -> Result<(), CursorError> {
        self.save_inner(write, None, encode)
    }

    pub fn save_offline(
        &self,
        write: CursorWrite<'_>,
        expected: &[super::ScopeSnapshot],
        encode: impl FnOnce(&mut dyn Write) -> Result<(), CursorError>,
    ) -> Result<(), CursorError> {
        if expected.len() != write.frozen.len()
            || !write.frozen.iter().all(|folder| {
                expected.iter().any(|scope| {
                    scope.mailbox == folder.mailbox && scope.uid_validity == folder.uid_validity
                })
            })
        {
            return Err(CursorError::Unavailable);
        }
        self.save_inner(write, Some(expected), encode)
    }

    fn save_inner(
        &self,
        write: CursorWrite<'_>,
        offline: Option<&[super::ScopeSnapshot]>,
        encode: impl FnOnce(&mut dyn Write) -> Result<(), CursorError>,
    ) -> Result<(), CursorError> {
        let row = checked_write(&write)?;
        let namespace = write.binding.namespace;
        let mut databases = self.0.lock()?;
        let target = databases.get(&namespace).ok_or(CursorError::Unavailable)?;
        let old = match write.expected {
            Some(expected) => {
                let stored = lookup(
                    &target.connection,
                    write.binding,
                    expected.token,
                    write.now_ms,
                )
                .map_err(|error| match error {
                    CursorError::Unavailable => CursorError::Conflict,
                    other => other,
                })?;
                if stored.row.revision != expected.revision
                    || stored.row.active != Some(expected.token)
                {
                    return Err(CursorError::Conflict);
                }
                Some(stored.id)
            }
            None => None,
        };
        let mut count = 1usize;
        let mut owned = row.owned_bytes;
        let mut encoded = row.encoded_len;
        for (key, db) in databases.iter() {
            let skip = if *key == namespace { old } else { None };
            let mut statement = db.connection.prepare_cached(
                "SELECT active,replay,owned_bytes,encoded_len
                 FROM conversation_cursors
                 WHERE expires_ms>?1 AND (?2 IS NULL OR id<>?2) LIMIT ?3",
            )?;
            let mut rows =
                statement.query(params![write.now_ms, skip, (MAX_CURSORS + 1) as u32])?;
            while let Some(value) = rows.next()? {
                let active: Option<[u8; 16]> = value.get(0)?;
                let replay: Option<[u8; 16]> = value.get(1)?;
                if [row.active, row.replay]
                    .into_iter()
                    .flatten()
                    .any(|token| active == Some(token) || replay == Some(token))
                {
                    return Err(CursorError::Conflict);
                }
                count += 1;
                owned = owned.saturating_add(value.get::<_, u32>(2)? as usize);
                encoded = encoded.saturating_add(value.get::<_, u32>(3)? as usize);
                if count > MAX_CURSORS || owned > MAX_BYTES || encoded > MAX_BYTES {
                    return Err(CursorError::Quota);
                }
            }
        }

        let database = databases
            .get_mut(&namespace)
            .ok_or(CursorError::Unavailable)?;
        self.0.write_checked(namespace, database, |tx| {
            if let Some(expected) = offline {
                let configured = self.0.scopes.get(&namespace).ok_or(CursorError::Unavailable)?;
                for scope in expected {
                    if !configured.mailboxes.contains(&scope.mailbox) {
                        return Err(CursorError::Unavailable);
                    }
                    super::thread_index::verify_scope(&self.0, tx, namespace, scope)?;
                }
            }
            let mut frozen = write.frozen.to_vec();
            frozen.sort_unstable_by(|a, b| a.mailbox.cmp(b.mailbox));
            let mut hash = digest_prefix(write.binding, row);
            for scope in &frozen {
                // A live observer may have retired this UIDVALIDITY while a
                // conversation request was preparing its candidate checkpoint.
                let stale: bool = tx.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM retired_epochs WHERE mailbox=?1 AND uid_validity=?2
                        UNION ALL SELECT 1 FROM mailboxes WHERE mailbox=?1 AND uid_validity<>?2
                     )",
                    params![scope.mailbox, scope.uid_validity],
                    |row| row.get(0),
                )?;
                if stale {
                    return Err(CursorError::Unavailable);
                }
                hash_scope(&mut hash, scope.mailbox, scope.uid_validity);
            }
            hash.update([0]);
            tx.execute("DELETE FROM conversation_cursors WHERE expires_ms<=?1", [write.now_ms])?;
            if let Some(id) = old {
                tx.execute("DELETE FROM conversation_cursors WHERE id=?1", [id])?;
            }
            tx.execute(
                "INSERT INTO conversation_cursors(principal,active,replay,revision,codec_version,owned_bytes,
                    encoded_len,expires_ms,last_progress_ms,digest,payload)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,zeroblob(32),?10)",
                params![
                    &write.binding.principal[..], row.active.as_ref().map(|token| &token[..]),
                    row.replay.as_ref().map(|token| &token[..]), row.revision as i64, row.codec_version,
                    row.owned_bytes as u32, row.encoded_len as u32, row.expires_ms, row.last_progress_ms,
                    ZeroBlob(row.encoded_len as u64),
                ],
            )?;
            let id = tx.last_insert_rowid();
            for scope in frozen {
                tx.execute(
                    "INSERT INTO cursor_scopes VALUES(?1,?2,?3)",
                    params![id, scope.mailbox, scope.uid_validity],
                )?;
            }
            {
                let blob = tx.blob_open("main", "conversation_cursors", "payload", id, false)?;
                let mut stream = HashStream { inner: blob, hash, remaining: row.encoded_len };
                encode(&mut stream)?;
                if stream.remaining != 0 {
                    return Err(CursorError::Corrupt);
                }
                let digest = stream.hash.finalize();
                drop(stream.inner);
                tx.execute("UPDATE conversation_cursors SET digest=?2 WHERE id=?1", params![id, &digest[..]])?;
            }
            Ok(())
        }, || {
            if offline.is_some() {
                self.0.require_offline_health()?;
            }
            if (write.cancelled)() {
                return Err(CursorError::Unavailable);
            }
            Ok(())
        })
    }

    pub fn invalidate(
        &self,
        binding: CursorBinding,
        token: [u8; 16],
        expected_revision: u64,
    ) -> Result<(), CursorError> {
        let revision = i64::try_from(expected_revision).map_err(|_| CursorError::Conflict)?;
        let mut databases = self.0.lock()?;
        let database = databases
            .get_mut(&binding.namespace)
            .ok_or(CursorError::Unavailable)?;
        self.0.write(binding.namespace, database, |tx| {
            let count = tx.execute(
                "DELETE FROM conversation_cursors
                 WHERE principal=?1 AND (active=?2 OR replay=?2) AND revision=?3",
                params![&binding.principal[..], &token[..], revision],
            )?;
            if count == 0 {
                return Err(CursorError::Conflict);
            }
            Ok(())
        })
    }
}

fn checked_write(write: &CursorWrite<'_>) -> Result<CursorRow, CursorError> {
    if (write.cancelled)() {
        return Err(CursorError::Unavailable);
    }
    if write.encoded_len == 0
        || write.encoded_len > MAX_BYTES
        || write.owned_bytes == 0
        || write.owned_bytes > MAX_BYTES
    {
        return Err(CursorError::Quota);
    }
    let mut seen = HashSet::new();
    if write.frozen.is_empty()
        || write.frozen.len() > MAX_FROZEN
        || write.frozen.iter().any(|scope| {
            scope.uid_validity == 0 || !valid_mailbox(scope.mailbox) || !seen.insert(scope.mailbox)
        })
    {
        return Err(CursorError::Corrupt);
    }
    let revision = match write.expected {
        Some(expected) => expected
            .revision
            .checked_add(1)
            .ok_or(CursorError::Corrupt)?,
        None => 1,
    };
    let row = CursorRow {
        active: write.active,
        replay: write.replay,
        revision,
        codec_version: write.codec_version,
        owned_bytes: write.owned_bytes,
        encoded_len: write.encoded_len,
        expires_ms: write.expires_ms,
        last_progress_ms: write.last_progress_ms,
    };
    if !row_valid(row, write.now_ms) {
        return Err(CursorError::Corrupt);
    }
    Ok(row)
}

fn lookup(
    connection: &rusqlite::Connection,
    binding: CursorBinding,
    token: [u8; 16],
    now_ms: i64,
) -> Result<StoredRow, CursorError> {
    let sql = format!("SELECT {ROW}{SELECT_TOKEN}");
    let mut query = connection.prepare_cached(&sql)?;
    let mut rows = query.query(params![&binding.principal[..], &token[..], now_ms])?;
    let stored = read_row(rows.next()?.ok_or(CursorError::Unavailable)?)?;
    if rows.next()?.is_some() || stored.principal != binding.principal {
        return Err(CursorError::Corrupt);
    }
    verify_row(&stored, now_ms)?;
    Ok(stored)
}

fn read_row(row: &Row<'_>) -> Result<StoredRow, CursorError> {
    Ok(StoredRow {
        id: row.get(0)?,
        principal: row.get(1)?,
        row: CursorRow {
            active: row.get(2)?,
            replay: row.get(3)?,
            revision: u64::try_from(row.get::<_, i64>(4)?).map_err(|_| CursorError::Corrupt)?,
            codec_version: row.get(5)?,
            owned_bytes: row.get::<_, u32>(6)? as usize,
            encoded_len: row.get::<_, u32>(7)? as usize,
            expires_ms: row.get(8)?,
            last_progress_ms: row.get(9)?,
        },
        digest: row.get(10)?,
        blob_len: row.get::<_, u32>(11)? as usize,
    })
}

fn row_valid(row: CursorRow, now_ms: i64) -> bool {
    row.active != row.replay
        && (1..=i64::MAX as u64).contains(&row.revision)
        && row.codec_version != 0
        && (1..=MAX_BYTES).contains(&row.owned_bytes)
        && (1..=MAX_BYTES).contains(&row.encoded_len)
        && row.last_progress_ms <= now_ms
        && row.expires_ms > now_ms
        && row.expires_ms > row.last_progress_ms
        && row.expires_ms <= row.last_progress_ms.saturating_add(TTL_MS)
}

fn verify_row(stored: &StoredRow, now_ms: i64) -> Result<(), CursorError> {
    if !row_valid(stored.row, now_ms) || stored.blob_len != stored.row.encoded_len {
        return Err(CursorError::Corrupt);
    }
    Ok(())
}

fn digest_prefix(binding: CursorBinding, row: CursorRow) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(b"email-mcp-cache-cursor-v1\0");
    hash.update(binding.namespace);
    hash.update(binding.principal);
    for token in [row.active, row.replay] {
        hash.update([u8::from(token.is_some())]);
        if let Some(token) = token {
            hash.update(token);
        }
    }
    hash.update(row.revision.to_be_bytes());
    hash.update(row.codec_version.to_be_bytes());
    hash.update((row.owned_bytes as u64).to_be_bytes());
    hash.update((row.encoded_len as u64).to_be_bytes());
    hash.update(row.expires_ms.to_be_bytes());
    hash.update(row.last_progress_ms.to_be_bytes());
    hash
}

fn hash_scope(hash: &mut Sha256, mailbox: &str, uid_validity: u32) {
    hash.update([1]);
    hash.update((mailbox.len() as u32).to_be_bytes());
    hash.update(mailbox.as_bytes());
    hash.update(uid_validity.to_be_bytes());
}

struct HashStream<T> {
    inner: T,
    hash: Sha256,
    remaining: usize,
}

impl<T: Read> Read for HashStream<T> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let limit = self.remaining.min(output.len());
        let count = self.inner.read(&mut output[..limit])?;
        self.hash.update(&output[..count]);
        self.remaining -= count;
        Ok(count)
    }
}

impl<T: Write> Write for HashStream<T> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.len() > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Cursor stream limit exceeded.",
            ));
        }
        let count = self.inner.write(input)?;
        self.hash.update(&input[..count]);
        self.remaining -= count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
