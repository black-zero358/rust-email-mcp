//! Generation-aware metadata, verified MIME evidence and substring indexing.
use super::{Cache, CacheError, Ref, valid_mailbox};
use crate::accounts::AccountKey;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;

const HEADER_BYTES: usize = 128 * 1024;
const INDEX_BYTES: usize = 8 * 1024;
const PART_BYTES: usize = 256 * 1024;
const MAX_SEARCH_CANDIDATES: usize = 200;
const MAX_HITS: usize = 50;

#[derive(Debug, Serialize)]
pub struct AccountStatus {
    pub max_bytes: u64,
    pub used_bytes: u64,
    pub global_max_bytes: u64,
    pub global_used_bytes: u64,
    pub startup_unclean: bool,
    pub folders: Vec<FolderStatus>,
}

#[derive(Debug, Serialize)]
pub struct FolderStatus {
    pub mailbox: String,
    pub uid_validity: Option<u32>,
    pub high_uid: Option<u32>,
    pub checked_at_ms: Option<i64>,
    pub dirty: bool,
    pub messages: u64,
    pub complete_headers: u64,
    pub indexed_headers: u64,
    pub verified_parts: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct SearchHit {
    pub uid_validity: u32,
    pub uid: u32,
    pub matched: &'static str,
}

#[derive(Debug)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub next_before_row: Option<i64>,
    pub indexed: u64,
    pub pending: u64,
}

impl Cache {
    pub fn observe_folder(
        &self,
        account: &AccountKey,
        mailbox: &str,
        uid_validity: u32,
        high_uid: Option<u32>,
        checked_at_ms: i64,
    ) -> Result<(), CacheError> {
        if uid_validity == 0 {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(account, mailbox)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let full_schema = database.full_schema;
        self.write(namespace, database, |tx| {
            let previous: Option<u32> = tx.query_row(
                "SELECT uid_validity FROM mailboxes WHERE mailbox=?1",
                [mailbox],
                |row| row.get(0),
            ).optional()?;
            if let Some(old) = previous
                && old != uid_validity
            {
                // UIDVALIDITY increases when an older generation is
                // invalidated. Concurrent observations can arrive out of order.
                if uid_validity < old {
                    return Err(CacheError::OutOfScope);
                }
                tx.execute("INSERT INTO retired_epochs VALUES(?1,?2)", params![mailbox, old])?;
            }
            if previous != Some(uid_validity) {
                tx.execute(
                    "DELETE FROM conversation_cursors WHERE id IN (
                        SELECT cursor_id FROM cursor_scopes WHERE mailbox=?1 AND uid_validity<>?2
                     )",
                    params![mailbox, uid_validity],
                )?;
            }
            tx.execute(
                "INSERT INTO mailboxes(mailbox,uid_validity,high_uid,checked_at_ms,dirty)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(mailbox) DO UPDATE SET
                   dirty=mailboxes.dirty OR mailboxes.uid_validity<>excluded.uid_validity,
                   uid_validity=excluded.uid_validity,
                   high_uid=CASE WHEN mailboxes.uid_validity=excluded.uid_validity
                            THEN coalesce(max(mailboxes.high_uid,excluded.high_uid),mailboxes.high_uid,excluded.high_uid)
                            ELSE excluded.high_uid END,
                   message_count=CASE WHEN mailboxes.uid_validity=excluded.uid_validity THEN mailboxes.message_count ELSE 0 END,
                   complete_count=CASE WHEN mailboxes.uid_validity=excluded.uid_validity THEN mailboxes.complete_count ELSE 0 END,
                   indexed_count=CASE WHEN mailboxes.uid_validity=excluded.uid_validity THEN mailboxes.indexed_count ELSE 0 END,
                   parts_count=CASE WHEN mailboxes.uid_validity=excluded.uid_validity THEN mailboxes.parts_count ELSE 0 END,
                   checked_at_ms=CASE WHEN mailboxes.uid_validity=excluded.uid_validity
                                 THEN max(mailboxes.checked_at_ms,excluded.checked_at_ms)
                                 ELSE excluded.checked_at_ms END",
                params![
                    mailbox, uid_validity, high_uid, checked_at_ms,
                    self.startup_unclean || (full_schema && super::dirty::scope_dirty(tx, mailbox)?),
                ],
            )?;
            Ok(())
        })
    }

    pub fn remember_headers(
        &self,
        reference: Ref<'_>,
        complete: bool,
        header: &[u8],
        index_text: Option<&str>,
    ) -> Result<(), CacheError> {
        let index_text = index_text.filter(|text| !text.is_empty());
        if header.len() > HEADER_BYTES
            || index_text.is_some_and(|text| text.len() > INDEX_BYTES || text.contains('\0'))
            || (!complete && index_text.is_some())
        {
            return Err(CacheError::OutOfScope);
        }
        self.update_message(reference, |tx, id| {
            write_headers(tx, id, complete, header, index_text)
        })
    }

    pub fn remember_parts(&self, reference: Ref<'_>, parts: &[u8]) -> Result<(), CacheError> {
        if parts.is_empty() || parts.len() > PART_BYTES {
            return Err(CacheError::OutOfScope);
        }
        self.update_message(reference, |tx, id| {
            tx.execute(
                "UPDATE messages SET parts=?2 WHERE id=?1",
                params![id, parts],
            )?;
            Ok(())
        })
    }

    /// Immutable content lookup. The caller validates current online UID
    /// existence and authorization before using a persisted header or parts.
    pub fn header(&self, reference: Ref<'_>) -> Result<Option<Vec<u8>>, CacheError> {
        self.message_bytes(reference, true)
    }

    pub fn parts(&self, reference: Ref<'_>) -> Result<Option<Vec<u8>>, CacheError> {
        self.message_bytes(reference, false)
    }

    pub fn tombstone(&self, reference: Ref<'_>) -> Result<(), CacheError> {
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let full_schema = database.full_schema;
        self.write(namespace, database, |tx| {
            let id = message_id(tx, reference)?;
            mark_tombstone(tx, id, full_schema)
        })
    }

    /// Record a completed live FETCH miss using the selected session's UIDNEXT.
    /// A UID at or above that boundary can still be assigned in this generation.
    pub fn tombstone_observed(&self, reference: Ref<'_>, uid_next: u32) -> Result<(), CacheError> {
        if reference.uid == 0 || reference.uid >= uid_next {
            return Ok(());
        }
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let pending: bool = database.connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM mailboxes f LEFT JOIN messages m
                  ON m.mailbox=f.mailbox AND m.uid_validity=f.uid_validity AND m.uid=?3
                WHERE f.mailbox=?1 AND f.uid_validity=?2 AND NOT coalesce(m.tombstone,0)
             )",
            params![reference.mailbox, reference.uid_validity, reference.uid],
            |row| row.get(0),
        )?;
        if pending {
            let full_schema = database.full_schema;
            self.write(namespace, database, |tx| {
                // The selected session can be ahead of the stored watermark.
                // Keep the deletion fence without rewriting folder observations.
                let id = ensure_message_row(
                    tx,
                    reference.mailbox,
                    reference.uid_validity,
                    reference.uid,
                )?;
                mark_tombstone(tx, id, full_schema)
            })?;
        }
        Ok(())
    }

    /// Only a completed reconciliation can clear a folder's dirty marker.
    pub fn set_folder_dirty(
        &self,
        account: &AccountKey,
        mailbox: &str,
        uid_validity: u32,
        dirty: bool,
    ) -> Result<(), CacheError> {
        let namespace = self.scope(account, mailbox)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        if database.full_schema {
            return Err(CacheError::NeedsVerification);
        }
        self.write(namespace, database, |tx| {
            let updated = tx.execute(
                "UPDATE mailboxes SET dirty=?3 WHERE mailbox=?1 AND uid_validity=?2",
                params![mailbox, uid_validity, dirty],
            )?;
            if updated == 0 {
                return Err(CacheError::OutOfScope);
            }
            Ok(())
        })
    }

    pub fn status(&self, account: &AccountKey) -> Result<AccountStatus, CacheError> {
        let namespace = account.reference_namespace();
        let scope = self.scopes.get(&namespace).ok_or(CacheError::OutOfScope)?;
        let databases = self.lock()?;
        self.write_health()?;
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        let (global_used_bytes, used_bytes) = self.disk.bytes(&database.name)?;
        let mut folders = Vec::with_capacity(scope.mailboxes.len());
        for mailbox in &scope.mailboxes {
            let observed = database
                .connection
                .query_row(
                    "SELECT uid_validity,high_uid,checked_at_ms,dirty,
                        message_count,complete_count,indexed_count,parts_count
                 FROM mailboxes WHERE mailbox=?1",
                    [mailbox],
                    |row| {
                        Ok(FolderStatus {
                            mailbox: mailbox.clone(),
                            uid_validity: Some(row.get(0)?),
                            high_uid: row.get(1)?,
                            checked_at_ms: Some(row.get(2)?),
                            dirty: row.get(3)?,
                            messages: row.get::<_, i64>(4)? as u64,
                            complete_headers: row.get::<_, i64>(5)? as u64,
                            indexed_headers: row.get::<_, i64>(6)? as u64,
                            verified_parts: row.get::<_, i64>(7)? as u64,
                        })
                    },
                )
                .optional()?;
            let mut folder = observed.unwrap_or_else(|| FolderStatus {
                mailbox: mailbox.clone(),
                uid_validity: None,
                high_uid: None,
                checked_at_ms: None,
                dirty: self.startup_unclean,
                messages: 0,
                complete_headers: 0,
                indexed_headers: 0,
                verified_parts: 0,
            });
            if database.full_schema {
                folder.dirty |= super::dirty::scope_dirty(&database.connection, mailbox)?;
            }
            folders.push(folder);
        }
        Ok(AccountStatus {
            max_bytes: scope.max_bytes,
            used_bytes,
            global_max_bytes: self.max_bytes,
            global_used_bytes,
            startup_unclean: self.startup_unclean,
            folders,
        })
    }

    pub fn search_metadata(
        &self,
        account: &AccountKey,
        mailbox: &str,
        uid_validity: u32,
        query: &str,
        before_row: Option<i64>,
        limit: usize,
    ) -> Result<SearchPage, CacheError> {
        if !valid_mailbox(query)
            || !(1..=MAX_HITS).contains(&limit)
            || before_row.is_some_and(|row| row <= 0)
        {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        let db = &database.connection;
        if database.full_schema && super::dirty::scope_dirty(db, mailbox)? {
            return Err(CacheError::NeedsVerification);
        }
        let (indexed, pending) = db
            .query_row(
                "SELECT indexed_count,message_count-indexed_count
             FROM mailboxes WHERE mailbox=?1 AND uid_validity=?2 AND NOT dirty",
                params![mailbox, uid_validity],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
            )
            .optional()?
            .ok_or(CacheError::OutOfScope)?;
        let before = before_row.unwrap_or(i64::MAX);
        let mut hits = Vec::with_capacity(limit);
        let mut next_before_row = None;
        if query.chars().count() < 3 {
            let mut statement = db.prepare(
                "SELECT id,uid,index_text FROM messages
                 WHERE mailbox=?1 AND uid_validity=?2 AND id<?3 AND NOT tombstone
                 ORDER BY id DESC LIMIT ?4",
            )?;
            let mut rows = statement.query(params![
                mailbox,
                uid_validity,
                before,
                (MAX_SEARCH_CANDIDATES + 1) as u32
            ])?;
            let mut last = None;
            let mut scanned = 0;
            while let Some(row) = rows.next()? {
                if scanned == MAX_SEARCH_CANDIDATES || hits.len() == limit {
                    next_before_row = last;
                    break;
                }
                last = Some(row.get(0)?);
                scanned += 1;
                if let rusqlite::types::ValueRef::Text(text) = row.get_ref(2)?
                    && std::str::from_utf8(text)
                        .map_err(|_| CacheError::Corrupt)?
                        .contains(query)
                {
                    hits.push(SearchHit {
                        uid_validity,
                        uid: row.get(1)?,
                        matched: "metadata",
                    });
                }
            }
        } else {
            let phrase = format!("\"{}\"", query.replace('"', "\"\""));
            let mut statement = db.prepare(
                "SELECT hits.id,m.uid FROM (
                    SELECT rowid AS id FROM message_index
                    WHERE message_index MATCH ?1 AND rowid<?2
                    ORDER BY rowid DESC LIMIT ?5
                 ) AS hits
                 LEFT JOIN messages AS m ON m.id=hits.id
                     AND m.mailbox=?3 AND m.uid_validity=?4 AND NOT m.tombstone
                 ORDER BY hits.id DESC",
            )?;
            let mut rows = statement.query(params![
                phrase,
                before,
                mailbox,
                uid_validity,
                (MAX_SEARCH_CANDIDATES + 1) as u32
            ])?;
            let mut last = None;
            let mut scanned = 0;
            while let Some(row) = rows.next()? {
                if scanned == MAX_SEARCH_CANDIDATES || hits.len() == limit {
                    next_before_row = last;
                    break;
                }
                last = Some(row.get(0)?);
                scanned += 1;
                if let Some(uid) = row.get(1)? {
                    hits.push(SearchHit {
                        uid_validity,
                        uid,
                        matched: "metadata",
                    });
                }
            }
        }
        Ok(SearchPage {
            hits,
            next_before_row,
            indexed,
            pending,
        })
    }

    pub(super) fn update_message(
        &self,
        reference: Ref<'_>,
        apply: impl FnOnce(&Transaction<'_>, i64) -> Result<(), CacheError>,
    ) -> Result<(), CacheError> {
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        self.write(namespace, database, |tx| {
            let id = message_id(tx, reference)?;
            let deleted: bool =
                tx.query_row("SELECT tombstone FROM messages WHERE id=?1", [id], |row| {
                    row.get(0)
                })?;
            if deleted {
                return Err(CacheError::OutOfScope);
            }
            apply(tx, id)
        })
    }

    fn message_bytes(
        &self,
        reference: Ref<'_>,
        header: bool,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let databases = self.lock()?;
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        let sql = if header {
            "SELECT m.header FROM messages AS m JOIN mailboxes AS f USING(mailbox,uid_validity)
             WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid=?3 AND NOT m.tombstone
               AND m.complete_header AND length(m.header)<=131072"
        } else {
            "SELECT m.parts FROM messages AS m JOIN mailboxes AS f USING(mailbox,uid_validity)
             WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid=?3 AND NOT m.tombstone
               AND length(m.parts)<=262144"
        };
        Ok(database
            .connection
            .query_row(
                sql,
                params![reference.mailbox, reference.uid_validity, reference.uid],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten())
    }
}

pub(super) fn write_headers(
    tx: &Transaction<'_>,
    id: i64,
    complete: bool,
    header: &[u8],
    index_text: Option<&str>,
) -> Result<(), CacheError> {
    let updated = tx.execute(
        "UPDATE messages SET header=?2,complete_header=?3,index_text=?4
         WHERE id=?1 AND (?3 OR NOT complete_header)",
        params![id, header, complete, index_text],
    )?;
    if updated != 0 {
        tx.execute("DELETE FROM message_index WHERE rowid=?1", [id])?;
        if let Some(text) = index_text {
            tx.execute(
                "INSERT INTO message_index(rowid,text) VALUES(?1,?2)",
                params![id, text],
            )?;
        }
    }
    Ok(())
}

pub(super) fn mark_tombstone(
    tx: &Transaction<'_>,
    id: i64,
    full_schema: bool,
) -> Result<(), CacheError> {
    if full_schema {
        tx.execute("DELETE FROM artifacts WHERE message_id=?1", [id])?;
    }
    tx.execute("DELETE FROM message_index WHERE rowid=?1", [id])?;
    tx.execute(
        "UPDATE messages SET tombstone=1,header=NULL,complete_header=0,index_text=NULL,parts=NULL
         WHERE id=?1",
        [id],
    )?;
    Ok(())
}

pub(super) fn message_id(tx: &Transaction<'_>, reference: Ref<'_>) -> Result<i64, CacheError> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM mailboxes WHERE mailbox=?1 AND uid_validity=?2 AND (high_uid IS NULL OR high_uid>=?3))",
        params![reference.mailbox, reference.uid_validity, reference.uid],
        |row| row.get(0),
    )?;
    if !exists || reference.uid == 0 {
        return Err(CacheError::OutOfScope);
    }
    ensure_message_row(tx, reference.mailbox, reference.uid_validity, reference.uid)
}

pub(super) fn ensure_message_row(
    tx: &Transaction<'_>,
    mailbox: &str,
    uid_validity: u32,
    uid: u32,
) -> Result<i64, CacheError> {
    tx.execute(
        "INSERT INTO messages(mailbox,uid_validity,uid) VALUES(?1,?2,?3)
         ON CONFLICT(mailbox,uid_validity,uid) DO NOTHING",
        params![mailbox, uid_validity, uid],
    )?;
    Ok(tx.query_row(
        "SELECT id FROM messages WHERE mailbox=?1 AND uid_validity=?2 AND uid=?3",
        params![mailbox, uid_validity, uid],
        |row| row.get(0),
    )?)
}
