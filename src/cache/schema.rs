//! Versioned private database schema and SQLite connection policy.
use super::{CacheError, Namespace, PAGE_BYTES};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::path::Path;

const APPLICATION_ID: u32 = 0x454d4343;
const METADATA_VERSION: u32 = 1;
const FULL_VERSION: u32 = 2;
const CONTENT_VERSION: u32 = 3;
const SYNC_VERSION: u32 = 4;

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
    full: bool,
) -> Result<bool, CacheError> {
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
            tx.pragma_update(None, "user_version", METADATA_VERSION)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.commit()?;
        }
        (METADATA_VERSION | FULL_VERSION | CONTENT_VERSION | SYNC_VERSION, APPLICATION_ID) => {}
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
    let full_schema = full || version >= FULL_VERSION;
    if full && version < FULL_VERSION {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(V2)?;
        tx.pragma_update(None, "user_version", FULL_VERSION)?;
        tx.commit()?;
    }
    if full && version < CONTENT_VERSION {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(V3)?;
        tx.pragma_update(None, "user_version", CONTENT_VERSION)?;
        tx.commit()?;
    }
    if full && version < SYNC_VERSION {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(V4)?;
        tx.pragma_update(None, "user_version", SYNC_VERSION)?;
        tx.commit()?;
    }
    if (saved_epoch as u64) < restore_epoch {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        restore(&tx, restore_epoch, full_schema)?;
        tx.commit()?;
    } else if unclean {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        flag_unclean(&tx, full_schema)?;
        tx.commit()?;
    }
    Ok(full_schema)
}

const V4: &str = "
CREATE TABLE sync_checkpoints (
    mailbox TEXT NOT NULL,
    kind INTEGER NOT NULL CHECK(kind BETWEEN 0 AND 2),
    scan_id BLOB NOT NULL CHECK(length(scan_id)=16),
    uid_validity INTEGER NOT NULL CHECK(uid_validity BETWEEN 1 AND 4294967295),
    high_uid INTEGER NOT NULL CHECK(high_uid BETWEEN 0 AND 4294967294),
    revision INTEGER NOT NULL CHECK(revision>0),
    restore_epoch INTEGER NOT NULL CHECK(restore_epoch>=0),
    dirty_revision INTEGER NOT NULL CHECK(dirty_revision>=0),
    presence_revision INTEGER NOT NULL CHECK(presence_revision>=0),
    proof_presence_revision INTEGER NOT NULL CHECK(proof_presence_revision>=0),
    next_low INTEGER CHECK(next_low BETWEEN 1 AND high_uid),
    width_hint INTEGER NOT NULL CHECK(width_hint BETWEEN 1 AND 4294967295),
    known_after INTEGER NOT NULL CHECK(known_after BETWEEN 0 AND high_uid),
    checked_at_ms INTEGER NOT NULL CHECK(checked_at_ms>=0),
    completed_at_ms INTEGER CHECK(completed_at_ms>=checked_at_ms),
    PRIMARY KEY(mailbox,kind)
) STRICT;
CREATE TRIGGER sync_presence_insert AFTER INSERT ON messages WHEN NOT new.tombstone BEGIN
    UPDATE sync_checkpoints SET presence_revision=presence_revision+1
    WHERE mailbox=new.mailbox AND uid_validity=new.uid_validity AND kind=2 AND new.uid<=high_uid;
END;
CREATE TRIGGER sync_presence_update AFTER UPDATE OF tombstone ON messages
WHEN old.tombstone<>new.tombstone BEGIN
    UPDATE sync_checkpoints SET presence_revision=presence_revision+1
    WHERE mailbox=new.mailbox AND uid_validity=new.uid_validity AND kind=2 AND new.uid<=high_uid;
END;
CREATE TABLE sync_work (
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    layer INTEGER NOT NULL CHECK(layer BETWEEN 0 AND 5),
    part INTEGER NOT NULL CHECK(part BETWEEN 0 AND 1023),
    state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 4),
    reason INTEGER CHECK(reason BETWEEN 1 AND 8),
    not_before_ms INTEGER NOT NULL DEFAULT 0 CHECK(not_before_ms>=0),
    revision INTEGER NOT NULL DEFAULT 1 CHECK(revision>0),
    PRIMARY KEY(mailbox,uid_validity,uid,layer,part)
) STRICT;
CREATE INDEX sync_due_work ON sync_work(mailbox,uid_validity,state,not_before_ms,uid,layer,part);
CREATE TRIGGER sync_tombstone_work AFTER UPDATE OF tombstone ON messages WHEN new.tombstone BEGIN
    DELETE FROM sync_work WHERE mailbox=new.mailbox AND uid_validity=new.uid_validity AND uid=new.uid;
END;
CREATE TRIGGER sync_artifact_lost AFTER UPDATE OF state,blob_id ON artifacts
WHEN old.state=1 AND (new.state<>1 OR old.blob_id IS NOT new.blob_id)
  AND (old.kind=0 OR (old.mime_version=1 AND (old.kind=2 OR old.html_version=1))) BEGIN
    UPDATE sync_work SET state=0,reason=NULL,not_before_ms=0,revision=revision+1
    WHERE (mailbox,uid_validity,uid)=(SELECT mailbox,uid_validity,uid FROM messages WHERE id=old.message_id)
      AND layer=CASE old.kind WHEN 0 THEN 2 WHEN 1 THEN 3 ELSE 5 END
      AND part=old.part AND state=1;
    UPDATE sync_work SET state=4,reason=NULL,not_before_ms=0,revision=revision+1
    WHERE old.kind=1 AND layer=4 AND part=0 AND state IN(0,1)
      AND (mailbox,uid_validity,uid)=(SELECT mailbox,uid_validity,uid FROM messages WHERE id=old.message_id);
END;
CREATE INDEX sync_message_uid ON messages(mailbox,uid_validity,uid) WHERE NOT tombstone;
";

const V3: &str = "
CREATE TABLE listing_evidence (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    flags BLOB CHECK(length(flags)<=8192),
    flags_checked_at_ms INTEGER,
    internal_date TEXT CHECK(length(internal_date)<=64),
    date_checked_at_ms INTEGER,
    size INTEGER CHECK(size BETWEEN 0 AND 4294967295),
    size_checked_at_ms INTEGER,
    observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms>=0)
) STRICT;
CREATE TABLE mailbox_evidence (
    mailbox TEXT PRIMARY KEY,
    evidence BLOB NOT NULL CHECK(length(evidence)<=32768),
    checked_at_ms INTEGER NOT NULL CHECK(checked_at_ms>=0)
) STRICT;

CREATE TABLE search_scope (
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    revision INTEGER NOT NULL CHECK(revision>0),
    PRIMARY KEY(mailbox,uid_validity)
) STRICT;
CREATE TRIGGER search_message_insert AFTER INSERT ON messages WHEN NOT new.tombstone BEGIN
    INSERT INTO search_scope VALUES(new.mailbox,new.uid_validity,1)
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER search_message_changed AFTER UPDATE OF header,complete_header,index_text,tombstone,parts ON messages
WHEN old.tombstone<>new.tombstone OR (NOT new.tombstone AND (
    old.header IS NOT new.header OR old.complete_header<>new.complete_header
    OR old.index_text IS NOT new.index_text OR old.parts IS NOT new.parts)) BEGIN
    INSERT INTO search_scope VALUES(new.mailbox,new.uid_validity,1)
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER search_listing_insert AFTER INSERT ON listing_evidence
WHEN new.flags IS NOT NULL OR new.internal_date IS NOT NULL OR new.size IS NOT NULL BEGIN
    INSERT INTO search_scope
        SELECT mailbox,uid_validity,1 FROM messages WHERE id=new.message_id AND NOT tombstone
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER search_listing_changed AFTER UPDATE OF flags,internal_date,size ON listing_evidence
WHEN old.flags IS NOT new.flags OR old.internal_date IS NOT new.internal_date OR old.size IS NOT new.size BEGIN
    INSERT INTO search_scope
        SELECT mailbox,uid_validity,1 FROM messages WHERE id=new.message_id AND NOT tombstone
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;

CREATE TABLE thread_scope (
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    revision INTEGER NOT NULL CHECK(revision>0),
    indexed_count INTEGER NOT NULL CHECK(indexed_count>=0),
    PRIMARY KEY(mailbox,uid_validity)
) STRICT;
CREATE TABLE thread_ready (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    version INTEGER NOT NULL CHECK(version=1)
) STRICT;
CREATE TABLE thread_ids (
    message_id INTEGER NOT NULL REFERENCES thread_ready(message_id) ON DELETE CASCADE,
    role INTEGER NOT NULL CHECK(role BETWEEN 0 AND 2),
    normalized_id TEXT NOT NULL CHECK(length(CAST(normalized_id AS BLOB)) BETWEEN 1 AND 998),
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    PRIMARY KEY(message_id,role,normalized_id)
) STRICT;
CREATE INDEX thread_id_window ON thread_ids(mailbox,uid_validity,role,normalized_id,uid);
CREATE TRIGGER thread_ready_insert AFTER INSERT ON thread_ready BEGIN
    INSERT INTO thread_scope VALUES(new.mailbox,new.uid_validity,1,1)
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1,indexed_count=indexed_count+1;
END;
CREATE TRIGGER thread_ready_remove AFTER DELETE ON thread_ready BEGIN
    UPDATE thread_scope SET revision=revision+1,indexed_count=indexed_count-1
    WHERE mailbox=old.mailbox AND uid_validity=old.uid_validity;
END;
CREATE TRIGGER thread_header_changed AFTER UPDATE OF header,complete_header,tombstone ON messages
WHEN (new.complete_header AND old.header IS NOT new.header)
    OR old.complete_header<>new.complete_header OR old.tombstone<>new.tombstone BEGIN
    DELETE FROM thread_ready WHERE message_id=new.id;
    INSERT INTO thread_scope VALUES(new.mailbox,new.uid_validity,1,0)
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER thread_raw_insert AFTER INSERT ON artifacts WHEN new.kind=0 AND new.state=1 BEGIN
    INSERT INTO thread_scope
        SELECT mailbox,uid_validity,1,0 FROM messages WHERE id=new.message_id
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER thread_raw_change AFTER UPDATE OF state,blob_id ON artifacts
WHEN new.kind=0 AND (old.state=1 OR new.state=1) AND (old.state<>new.state OR old.blob_id IS NOT new.blob_id) BEGIN
    INSERT INTO thread_scope
        SELECT mailbox,uid_validity,1,0 FROM messages WHERE id=new.message_id
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER thread_raw_remove AFTER DELETE ON artifacts WHEN old.kind=0 AND old.state=1 BEGIN
    INSERT INTO thread_scope
        SELECT mailbox,uid_validity,1,0 FROM messages WHERE id=old.message_id
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;

CREATE TABLE body_indexes (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK(id<=2251799813685246),
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    mime_version INTEGER NOT NULL,
    html_version INTEGER NOT NULL,
    blob_id BLOB NOT NULL CHECK(length(blob_id)=16),
    digest BLOB NOT NULL CHECK(length(digest)=32),
    complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN (0,1)),
    UNIQUE(message_id,mime_version,html_version)
) STRICT;
CREATE TABLE body_scope_count (
    mailbox TEXT NOT NULL, uid_validity INTEGER NOT NULL,
    mime_version INTEGER NOT NULL, html_version INTEGER NOT NULL,
    indexed_count INTEGER NOT NULL CHECK(indexed_count>=0),
    PRIMARY KEY(mailbox,uid_validity,mime_version,html_version)
) STRICT;
CREATE TRIGGER body_index_visibility AFTER UPDATE OF complete ON body_indexes
WHEN old.complete<>new.complete BEGIN
    INSERT INTO body_scope_count
      SELECT mailbox,uid_validity,new.mime_version,new.html_version,new.complete
      FROM messages WHERE id=new.message_id
    ON CONFLICT(mailbox,uid_validity,mime_version,html_version)
      DO UPDATE SET indexed_count=indexed_count+new.complete-old.complete;
    INSERT INTO search_scope
        SELECT mailbox,uid_validity,1 FROM messages WHERE id=new.message_id AND NOT tombstone
    ON CONFLICT(mailbox,uid_validity) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER body_artifact_update BEFORE UPDATE OF state,blob_id ON artifacts
WHEN old.kind=1 AND old.state=1 AND (new.state<>1 OR old.blob_id IS NOT new.blob_id) BEGIN
    UPDATE body_indexes SET complete=0 WHERE message_id=old.message_id AND blob_id=old.blob_id;
END;
CREATE TRIGGER body_artifact_delete BEFORE DELETE ON artifacts
WHEN old.kind=1 AND old.state=1 BEGIN
    UPDATE body_indexes SET complete=0 WHERE message_id=old.message_id AND blob_id=old.blob_id;
END;
CREATE TABLE body_leaf_metadata (
    blob_id BLOB PRIMARY KEY REFERENCES blobs(id) ON DELETE CASCADE,
    payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 16384),
    digest BLOB NOT NULL CHECK(length(digest)=32)
) STRICT;
CREATE TABLE body_segments (
    id INTEGER PRIMARY KEY,
    index_id INTEGER NOT NULL REFERENCES body_indexes(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 4095),
    text TEXT NOT NULL CHECK(length(CAST(text AS BLOB))<=18432),
    UNIQUE(index_id,ordinal)
) STRICT;
CREATE VIRTUAL TABLE body_search USING fts5(text, tokenize='trigram case_sensitive 1', content='body_segments', content_rowid='id');
CREATE TRIGGER body_segment_insert AFTER INSERT ON body_segments BEGIN
    INSERT INTO body_search(rowid,text) VALUES(new.id,new.text);
END;
CREATE TRIGGER body_segment_remove AFTER DELETE ON body_segments BEGIN
    INSERT INTO body_search(body_search,rowid,text) VALUES('delete',old.id,old.text);
END;
";

fn flag_unclean(tx: &Transaction<'_>, full_schema: bool) -> Result<(), CacheError> {
    tx.execute("UPDATE mailboxes SET dirty=1 WHERE NOT dirty", [])?;
    if full_schema {
        // Reconciliation must not revive a snapshot issued before an
        // unclean or unwitnessed run, even if its UIDVALIDITY is unchanged.
        tx.execute("UPDATE scope_fences SET revision=revision+1,dirty=1", [])?;
        tx.execute(
            "INSERT OR IGNORE INTO scope_fences SELECT mailbox,1,1 FROM mailboxes",
            [],
        )?;
        tx.execute("DELETE FROM mutation_claims", [])?;
    }
    Ok(())
}

pub(super) fn restore(
    tx: &Transaction<'_>,
    restore_epoch: u64,
    full_schema: bool,
) -> Result<(), CacheError> {
    tx.execute("DELETE FROM conversation_cursors", [])?;
    let version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version >= SYNC_VERSION {
        tx.execute("DELETE FROM sync_checkpoints", [])?;
        tx.execute("DELETE FROM sync_work", [])?;
    }
    flag_unclean(tx, full_schema)?;
    tx.execute(
        "UPDATE cache_owner SET restore_epoch=?1 WHERE id=1",
        [restore_epoch as i64],
    )?;
    Ok(())
}

pub(super) const V2: &str = "
CREATE TABLE blobs (
    id BLOB PRIMARY KEY CHECK(length(id)=16),
    bytes INTEGER NOT NULL CHECK(bytes BETWEEN 0 AND 33554432),
    sha256 BLOB NOT NULL CHECK(length(sha256)=32)
) STRICT;
CREATE TABLE raw_fingerprints (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    bytes INTEGER NOT NULL CHECK(bytes BETWEEN 0 AND 33554432),
    sha256 BLOB NOT NULL CHECK(length(sha256)=32)
) STRICT;
CREATE TABLE artifacts (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    kind INTEGER NOT NULL CHECK(kind BETWEEN 0 AND 2),
    part INTEGER NOT NULL,
    mime_version INTEGER NOT NULL,
    html_version INTEGER NOT NULL,
    state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 4),
    blob_id BLOB REFERENCES blobs(id),
    raw_id BLOB REFERENCES blobs(id),
    reason INTEGER,
    CHECK((state=0 AND blob_id IS NULL AND raw_id IS NULL AND reason IS NULL)
       OR (state=1 AND blob_id IS NOT NULL AND raw_id IS NULL AND reason IS NULL)
       OR (state=2 AND blob_id IS NULL AND raw_id IS NOT NULL AND reason IS NULL AND kind<>0)
       OR (state=3 AND blob_id IS NULL AND raw_id IS NULL AND reason IS NOT NULL AND reason BETWEEN 1 AND 3)
       OR (state=4 AND blob_id IS NULL AND raw_id IS NULL AND reason IS NOT NULL AND reason BETWEEN 4 AND 5)),
    CHECK((kind=0 AND part=0 AND mime_version=0 AND html_version=0)
       OR (kind=1 AND part=0 AND mime_version BETWEEN 1 AND 65535 AND html_version BETWEEN 1 AND 65535)
       OR (kind=2 AND part BETWEEN 0 AND 4294967295 AND mime_version BETWEEN 1 AND 65535 AND html_version=0)),
    PRIMARY KEY(message_id,kind,part,mime_version,html_version)
) STRICT;
CREATE INDEX artifacts_blob ON artifacts(blob_id);
CREATE INDEX artifacts_raw ON artifacts(raw_id);
CREATE TABLE scope_fences (
    mailbox TEXT PRIMARY KEY,
    revision INTEGER NOT NULL CHECK(revision>0),
    dirty INTEGER NOT NULL CHECK(dirty IN (0,1))
) STRICT;
CREATE TABLE mutation_claims (
    id BLOB PRIMARY KEY CHECK(length(id)=16)
) STRICT;
CREATE TABLE mutation_scopes (
    claim_id BLOB NOT NULL REFERENCES mutation_claims(id) ON DELETE CASCADE,
    mailbox TEXT NOT NULL REFERENCES scope_fences(mailbox),
    PRIMARY KEY(claim_id,mailbox)
) STRICT;
CREATE INDEX mutation_scopes_mailbox ON mutation_scopes(mailbox);
";

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
