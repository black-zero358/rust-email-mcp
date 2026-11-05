//! Versioned private database schema and SQLite connection policy.
use super::{CacheError, Namespace, PAGE_BYTES};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::path::Path;

const APPLICATION_ID: u32 = 0x454d4343;
const VERSION: u32 = 1;

pub(super) fn open(path: &Path) -> Result<Connection, CacheError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.execute_batch(
        "PRAGMA page_size=4096;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_spill=OFF;
         PRAGMA mmap_size=0;
         PRAGMA foreign_keys=ON;
         PRAGMA synchronous=FULL;",
    )?;
    let mode: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let page_size: u32 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("delete") || u64::from(page_size) != PAGE_BYTES {
        return Err(CacheError::Corrupt);
    }
    Ok(connection)
}

pub(super) fn initialize(
    connection: &mut Connection,
    namespace: &Namespace,
    unclean: bool,
    restore_epoch: u64,
) -> Result<(), CacheError> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let app: u32 = connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    match (version, app) {
        (0, 0) => {
            let empty: bool = connection.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM sqlite_schema)",
                [],
                |row| row.get(0),
            )?;
            if !empty {
                return Err(CacheError::Corrupt);
            }
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(V1)?;
            tx.execute(
                "INSERT INTO cache_owner VALUES(1,?1,?2)",
                params![&namespace[..], restore_epoch as i64],
            )?;
            tx.pragma_update(None, "user_version", VERSION)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.commit()?;
        }
        (VERSION, APPLICATION_ID) => {}
        _ => return Err(CacheError::Corrupt),
    }
    let saved_epoch: i64 = connection
        .query_row(
            "SELECT restore_epoch FROM cache_owner WHERE id=1 AND namespace=?1",
            [&namespace[..]],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(CacheError::Corrupt)?;
    let clean: bool = connection.query_row("PRAGMA quick_check(1)", [], |row| {
        Ok(row.get_ref(0)?.as_str()? == "ok")
    })?;
    if !clean || saved_epoch < 0 || saved_epoch as u64 > restore_epoch {
        return Err(CacheError::Corrupt);
    }
    if (saved_epoch as u64) < restore_epoch {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        restore(&tx, restore_epoch)?;
        tx.commit()?;
    } else if unclean {
        connection.execute("UPDATE mailboxes SET dirty=1 WHERE NOT dirty", [])?;
    }
    Ok(())
}

pub(super) fn restore(tx: &Transaction<'_>, restore_epoch: u64) -> Result<(), CacheError> {
    tx.execute("DELETE FROM conversation_cursors", [])?;
    tx.execute("UPDATE mailboxes SET dirty=1 WHERE NOT dirty", [])?;
    tx.execute(
        "UPDATE cache_owner SET restore_epoch=?1 WHERE id=1",
        [restore_epoch as i64],
    )?;
    Ok(())
}

const V1: &str = "
CREATE TABLE cache_owner (
    id INTEGER PRIMARY KEY CHECK(id=1),
    namespace BLOB NOT NULL CHECK(length(namespace)=32),
    restore_epoch INTEGER NOT NULL CHECK(restore_epoch>=0)
) STRICT;
CREATE TABLE mailboxes (
    mailbox TEXT PRIMARY KEY,
    uid_validity INTEGER NOT NULL CHECK(uid_validity BETWEEN 1 AND 4294967295),
    high_uid INTEGER CHECK(high_uid BETWEEN 0 AND 4294967295),
    checked_at_ms INTEGER NOT NULL,
    dirty INTEGER NOT NULL DEFAULT 0 CHECK(dirty IN (0,1)),
    message_count INTEGER NOT NULL DEFAULT 0 CHECK(message_count>=0),
    complete_count INTEGER NOT NULL DEFAULT 0 CHECK(complete_count BETWEEN 0 AND message_count),
    indexed_count INTEGER NOT NULL DEFAULT 0 CHECK(indexed_count BETWEEN 0 AND complete_count),
    parts_count INTEGER NOT NULL DEFAULT 0 CHECK(parts_count BETWEEN 0 AND message_count)
) STRICT;
CREATE TABLE retired_epochs (
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    PRIMARY KEY(mailbox,uid_validity)
) STRICT;
CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    uid INTEGER NOT NULL CHECK(uid BETWEEN 1 AND 4294967295),
    tombstone INTEGER NOT NULL DEFAULT 0 CHECK(tombstone IN (0,1)),
    header BLOB CHECK(length(header)<=131072),
    complete_header INTEGER NOT NULL DEFAULT 0 CHECK(complete_header IN (0,1)),
    index_text TEXT CHECK(length(CAST(index_text AS BLOB))<=8192),
    parts BLOB CHECK(length(parts)<=262144),
    UNIQUE(mailbox,uid_validity,uid)
) STRICT;
CREATE INDEX messages_page ON messages(mailbox,uid_validity,id) WHERE NOT tombstone;
CREATE TRIGGER count_message_insert AFTER INSERT ON messages BEGIN
    UPDATE mailboxes SET message_count=message_count+1
    WHERE mailbox=new.mailbox AND uid_validity=new.uid_validity;
END;
CREATE TRIGGER count_message_update AFTER UPDATE ON messages BEGIN
    UPDATE mailboxes SET
        message_count=message_count+old.tombstone-new.tombstone,
        complete_count=complete_count+new.complete_header-old.complete_header,
        indexed_count=indexed_count+(new.index_text IS NOT NULL)-(old.index_text IS NOT NULL),
        parts_count=parts_count+(new.parts IS NOT NULL)-(old.parts IS NOT NULL)
    WHERE mailbox=new.mailbox AND uid_validity=new.uid_validity;
END;
CREATE VIRTUAL TABLE message_index USING fts5(text, tokenize='trigram case_sensitive 1');

CREATE TABLE conversation_cursors (
    id INTEGER PRIMARY KEY,
    principal BLOB NOT NULL CHECK(length(principal)=32),
    active BLOB UNIQUE CHECK(length(active)=16),
    replay BLOB UNIQUE CHECK(length(replay)=16),
    revision INTEGER NOT NULL CHECK(revision>0),
    last_progress_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    codec_version INTEGER NOT NULL CHECK(codec_version BETWEEN 1 AND 65535),
    owned_bytes INTEGER NOT NULL CHECK(owned_bytes BETWEEN 1 AND 8388608),
    encoded_len INTEGER NOT NULL CHECK(encoded_len BETWEEN 1 AND 8388608),
    digest BLOB NOT NULL CHECK(length(digest)=32),
    payload BLOB NOT NULL CHECK(length(payload)=encoded_len),
    CHECK(active IS NOT NULL OR replay IS NOT NULL),
    CHECK(active IS NULL OR replay IS NULL OR active<>replay)
) STRICT;
CREATE INDEX cursor_expires ON conversation_cursors(expires_ms);
CREATE TABLE cursor_scopes (
    cursor_id INTEGER NOT NULL REFERENCES conversation_cursors(id) ON DELETE CASCADE,
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL CHECK(uid_validity BETWEEN 1 AND 4294967295),
    PRIMARY KEY(cursor_id,mailbox)
) STRICT;
CREATE INDEX cursor_generation ON cursor_scopes(mailbox,uid_validity);
";
