//! Exact, versioned header relationships over a frozen local scope.
use super::{Cache, CacheError, Namespace, Ref};
use crate::accounts::AccountKey;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::ops::RangeInclusive;

pub const THREAD_INDEX_VERSION: u16 = 1;
const MAX_IDS: usize = 1024;
const MAX_INDEX_BYTES: usize = 256 * 1024;
const MAX_HEADER_BYTES: usize = 128 * 1024;
const MAX_WINDOW: u32 = 200;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum ThreadRole {
    MessageId = 0,
    References = 1,
    InReplyTo = 2,
}

pub struct ThreadId<'a> {
    pub role: ThreadRole,
    /// The shared ThreadFacts parser supplies this canonical exact value.
    pub id: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeSnapshot {
    pub account_namespace: [u8; 32],
    pub mailbox: String,
    pub uid_validity: u32,
    pub high_uid: u32,
    pub checked_at_ms: i64,
    pub index_revision: u64,
    pub dirty_revision: u64,
    pub restore_epoch: u64,
    pub complete_headers: u64,
    pub indexed_threads: u64,
    pub known_messages: u64,
}

impl Cache {
    pub fn freeze_scope(
        &self,
        account: &AccountKey,
        mailbox: &str,
    ) -> Result<ScopeSnapshot, CacheError> {
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        current_scope(self, db, namespace, mailbox)
    }

    pub fn verify_scope(
        &self,
        account: &AccountKey,
        expected: &ScopeSnapshot,
    ) -> Result<(), CacheError> {
        let namespace = self.scope(account, &expected.mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        verify_scope(self, db, namespace, expected)
    }

    pub fn thread_uids(
        &self,
        account: &AccountKey,
        expected: &ScopeSnapshot,
        role: ThreadRole,
        id: &str,
        window: RangeInclusive<u32>,
    ) -> Result<Vec<u32>, CacheError> {
        let (&start, &end) = (window.start(), window.end());
        if !valid_id(id)
            || start == 0
            || end < start
            || end - start >= MAX_WINDOW
            || end > expected.high_uid
        {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(account, &expected.mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        verify_scope(self, db, namespace, expected)?;
        let mut query = db.prepare_cached(
            "SELECT uid FROM thread_ids WHERE mailbox=?1 AND uid_validity=?2
             AND role=?3 AND normalized_id=?4 AND uid BETWEEN ?5 AND ?6 ORDER BY uid LIMIT 201",
        )?;
        let hits: Vec<u32> = query
            .query_map(
                params![
                    expected.mailbox,
                    expected.uid_validity,
                    role as u8,
                    id,
                    start,
                    end
                ],
                |row| row.get(0),
            )?
            .collect::<Result<_, _>>()?;
        if hits.len() > MAX_WINDOW as usize || hits.windows(2).any(|uids| uids[0] >= uids[1]) {
            return Err(CacheError::Corrupt);
        }
        Ok(hits)
    }

    pub fn scope_uids(
        &self,
        account: &AccountKey,
        expected: &ScopeSnapshot,
        window: RangeInclusive<u32>,
        limit: usize,
    ) -> Result<Vec<u32>, CacheError> {
        let (&start, &end) = (window.start(), window.end());
        if start == 0
            || end < start
            || end - start >= 500
            || end > expected.high_uid
            || !(1..=202).contains(&limit)
        {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(account, &expected.mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        verify_scope(self, db, namespace, expected)?;
        let mut query = db.prepare_cached(
            "SELECT uid FROM messages WHERE mailbox=?1 AND uid_validity=?2 AND NOT tombstone
             AND uid BETWEEN ?3 AND ?4 ORDER BY uid DESC LIMIT ?5",
        )?;
        Ok(query
            .query_map(
                params![
                    expected.mailbox,
                    expected.uid_validity,
                    start,
                    end,
                    limit as u32
                ],
                |row| row.get(0),
            )?
            .collect::<Result<_, _>>()?)
    }

    pub fn thread_header(
        &self,
        account: &AccountKey,
        expected: &ScopeSnapshot,
        uid: u32,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        if uid == 0 || uid > expected.high_uid {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(account, &expected.mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        verify_scope(self, db, namespace, expected)?;
        Ok(db
            .query_row(
                "SELECT header FROM messages m JOIN thread_ready r ON r.message_id=m.id
             WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid=?3 AND NOT m.tombstone
                AND m.complete_header AND length(m.header)<=131072",
                params![expected.mailbox, expected.uid_validity, uid],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn remember_thread_ids(
        &self,
        reference: Ref<'_>,
        version: u16,
        expected_header: &[u8],
        ids: &[ThreadId<'_>],
    ) -> Result<(), CacheError> {
        if !self.full || version != THREAD_INDEX_VERSION || expected_header.len() > MAX_HEADER_BYTES
        {
            return Err(CacheError::OutOfScope);
        }
        let ids = prepare_thread_ids(ids)?;
        self.update_message(reference, |tx, message| {
            let header_matches: bool = tx.query_row(
                "SELECT complete_header AND header=?2 FROM messages WHERE id=?1",
                params![message, expected_header],
                |row| row.get(0),
            )?;
            if !header_matches {
                return Err(CacheError::OutOfScope);
            }
            write_thread_ids(
                tx,
                message,
                reference.mailbox,
                reference.uid_validity,
                reference.uid,
                &ids,
            )
        })
    }
}

pub(super) fn prepare_thread_ids<'a>(
    ids: &[ThreadId<'a>],
) -> Result<Vec<(u8, &'a str)>, CacheError> {
    if ids.len() > MAX_IDS
        || ids.iter().any(|id| !valid_id(id.id))
        || ids.iter().map(|id| id.id.len()).sum::<usize>() > MAX_INDEX_BYTES
        || ids
            .iter()
            .filter(|id| id.role == ThreadRole::MessageId)
            .count()
            > 1
    {
        return Err(CacheError::OutOfScope);
    }
    let mut ids = ids
        .iter()
        .map(|id| (id.role as u8, id.id))
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

pub(super) fn write_thread_ids(
    tx: &Transaction<'_>,
    message: i64,
    mailbox: &str,
    uid_validity: u32,
    uid: u32,
    ids: &[(u8, &str)],
) -> Result<(), CacheError> {
    let indexed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_ready WHERE message_id=?1)",
        [message],
        |row| row.get(0),
    )?;
    if indexed {
        let mut query = tx.prepare_cached(
                    "SELECT role,normalized_id FROM thread_ids WHERE message_id=?1 ORDER BY role,normalized_id"
                )?;
        let mut rows = query.query([message])?;
        let mut same = true;
        for (role, id) in ids {
            let Some(row) = rows.next()? else {
                same = false;
                break;
            };
            if row.get::<_, u8>(0)? != *role
                || row.get_ref(1)?.as_str().map_err(|_| CacheError::Corrupt)? != *id
            {
                same = false;
                break;
            }
        }
        if same && rows.next()?.is_none() {
            return Ok(());
        }
    }
    tx.execute("DELETE FROM thread_ready WHERE message_id=?1", [message])?;
    tx.execute(
        "INSERT INTO thread_ready VALUES(?1,?2,?3,?4)",
        params![message, mailbox, uid_validity, THREAD_INDEX_VERSION],
    )?;
    let mut insert = tx.prepare_cached("INSERT INTO thread_ids VALUES(?1,?2,?3,?4,?5,?6)")?;
    for (role, id) in ids {
        insert.execute(params![message, role, id, mailbox, uid_validity, uid])?;
    }
    Ok(())
}

pub(super) fn current_scope(
    cache: &Cache,
    db: &Connection,
    namespace: Namespace,
    mailbox: &str,
) -> Result<ScopeSnapshot, CacheError> {
    cache.require_offline_health()?;
    let observed = db.query_row(
        "SELECT f.uid_validity,f.high_uid,f.checked_at_ms,coalesce(t.revision,0),coalesce(t.indexed_count,0),
                f.complete_count,f.message_count FROM mailboxes f
         LEFT JOIN thread_scope t USING(mailbox,uid_validity) WHERE f.mailbox=?1",
        [mailbox],
        |row| Ok((row.get(0)?,row.get::<_, Option<u32>>(1)?,row.get(2)?,row.get::<_,i64>(3)?,
            row.get::<_,i64>(4)?,row.get::<_,i64>(5)?,row.get::<_,i64>(6)?)),
    ).optional()?.ok_or(CacheError::NeedsVerification)?;
    let (uid_validity, high_uid, checked_at_ms, index_revision, indexed, complete, known) =
        observed;
    let high_uid = high_uid.ok_or(CacheError::NeedsVerification)?;
    let stamp = cache.check_offline(db, mailbox, uid_validity, namespace)?;
    Ok(ScopeSnapshot {
        account_namespace: namespace,
        mailbox: mailbox.into(),
        uid_validity,
        high_uid,
        checked_at_ms,
        index_revision: index_revision as u64,
        dirty_revision: stamp.revision(),
        restore_epoch: cache.disk.restore_epoch(),
        complete_headers: complete as u64,
        indexed_threads: indexed as u64,
        known_messages: known as u64,
    })
}

pub(super) fn verify_scope(
    cache: &Cache,
    db: &Connection,
    namespace: Namespace,
    expected: &ScopeSnapshot,
) -> Result<(), CacheError> {
    if expected.account_namespace != namespace {
        return Err(CacheError::OutOfScope);
    }
    let current = current_scope(cache, db, namespace, &expected.mailbox)?;
    if current.uid_validity != expected.uid_validity
        || current.high_uid < expected.high_uid
        || current.index_revision != expected.index_revision
        || current.dirty_revision != expected.dirty_revision
        || current.restore_epoch != expected.restore_epoch
    {
        return Err(CacheError::NeedsVerification);
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    (5..=998).contains(&id.len())
        && id.starts_with('<')
        && id.ends_with('>')
        && !id.chars().any(char::is_control)
}
