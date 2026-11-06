//! Full, versioned body evidence with hidden incremental index construction.
use super::{
    ArtifactKey, ArtifactState, BlobId, Cache, CacheError, CachedMessage, Ref, ScopeSnapshot,
    blob::{artifact, present_message},
    evidence::cached_message,
    thread_index::current_scope,
};
use crate::accounts::AccountKey;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
const SEGMENT_BYTES: usize = 16 * 1024;
const OVERLAP_CHARACTERS: usize = 511;
const SEGMENT_STRIDE: i64 = 4096;
const MAX_CANDIDATES: usize = 200;
const MAX_HITS: usize = 50;

pub(crate) const BODY_INDEX_VERSION: u16 = 2;
pub(crate) const BODY_INDEX_REBUILD_BATCH: u32 = 4;

// Version 1 used the artifact's HTML version directly. Keep the artifact key
// and leaf metadata readable, and give the projected index its own namespace.
pub(super) const fn index_html_version(html_version: u16) -> i64 {
    (html_version as i64) << 16 | BODY_INDEX_VERSION as i64
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchPosition {
    pub uid: u32,
    pub segment: Option<u16>,
}

#[derive(Debug)]
pub struct CachedSearchHit {
    pub message: CachedMessage,
    pub matched_metadata: bool,
    pub matched_body: bool,
}

#[derive(Debug)]
pub struct CachedSearchPage {
    pub scope: ScopeSnapshot,
    pub search_revision: u64,
    pub items: Vec<CachedSearchHit>,
    pub next: Option<SearchPosition>,
    pub indexed_metadata: u64,
    pub indexed_body: u64,
}

enum Build {
    Done,
    Sweep,
    Append(i64),
}

struct StoredIndex {
    id: i64,
    blob: [u8; 16],
    digest: [u8; 32],
    complete: bool,
    version: i64,
}

impl Cache {
    /// Pick old or interrupted body indexes to rebuild from their existing
    /// local body blob. Search results only expose the current projection.
    pub(crate) fn pending_body_index_rebuild(
        &self,
        account: &AccountKey,
        scope: &ScopeSnapshot,
        (mime, html): (u16, u16),
    ) -> Result<Vec<(u32, BlobId, u64)>, CacheError> {
        let namespace = self.scope(account, &scope.mailbox)?;
        if !self.full || namespace != scope.account_namespace {
            return Err(CacheError::OutOfScope);
        }
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let mut statement = db.prepare_cached(
            "SELECT m.uid,a.blob_id,d.bytes FROM messages m
             JOIN artifacts a ON a.message_id=m.id AND a.kind=1 AND a.part=0
                AND a.mime_version=?4 AND a.html_version=?5 AND a.state=1
             JOIN blobs d ON d.id=a.blob_id
             WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid<=?3 AND NOT m.tombstone
                AND EXISTS(SELECT 1 FROM body_indexes b WHERE b.message_id=m.id AND b.mime_version=?4
                    AND b.blob_id=a.blob_id AND (b.html_version=?5 OR (b.html_version=?6 AND NOT b.complete)))
                AND NOT EXISTS(SELECT 1 FROM body_indexes b WHERE b.message_id=m.id AND b.mime_version=?4
                    AND b.html_version=?6 AND b.blob_id=a.blob_id AND b.complete)
             ORDER BY m.uid DESC LIMIT ?7",
        )?;
        Ok(statement
            .query_map(
                params![
                    scope.mailbox,
                    scope.uid_validity,
                    scope.high_uid,
                    mime,
                    html,
                    index_html_version(html),
                    BODY_INDEX_REBUILD_BATCH
                ],
                |row| {
                    Ok((
                        row.get(0)?,
                        BlobId::from_bytes(row.get(1)?),
                        u64::from(row.get::<_, u32>(2)?),
                    ))
                },
            )?
            .collect::<Result<_, _>>()?)
    }

    pub(crate) fn discard_old_body_index(
        &self,
        reference: Ref<'_>,
        kind: ArtifactKey,
        blob: BlobId,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), CacheError> {
        let ArtifactKey::FullBody {
            mime_version,
            html_version,
        } = kind
        else {
            return Err(CacheError::OutOfScope);
        };
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let epoch = self.disk.restore_epoch();
        let check = || {
            if cancelled() || epoch != self.disk.restore_epoch() {
                Err(CacheError::NeedsVerification)
            } else {
                Ok(())
            }
        };
        loop {
            check()?;
            let mut databases = self.lock()?;
            let database = databases
                .get_mut(&namespace)
                .ok_or(CacheError::OutOfScope)?;
            let more =
                self.write_checked(
                    namespace,
                    database,
                    |tx| {
                        verify_body(tx, reference, kind, blob)?;
                        let id = tx.query_row(
                    "SELECT b.id FROM body_indexes b JOIN messages m ON b.message_id=m.id
                     WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid=?3 AND b.mime_version=?4
                     AND (b.html_version=?5 OR (b.html_version=?6 AND NOT b.complete))
                     AND b.blob_id=?7 ORDER BY b.html_version LIMIT 1",
                    params![reference.mailbox, reference.uid_validity, reference.uid, mime_version,
                        html_version, index_html_version(html_version), blob.bytes()],
                    |row| row.get(0),
                ).optional()?;
                        match id {
                            Some(id) => {
                                sweep_index(tx, id)?;
                                Ok(true)
                            }
                            None => Ok(false),
                        }
                    },
                    check,
                )?;
            if !more {
                return Ok(());
            }
        }
    }

    /// Search cursors freeze search visibility independently of the thread index.
    pub fn search_revision(
        &self,
        account: &AccountKey,
        scope: &ScopeSnapshot,
    ) -> Result<u64, CacheError> {
        let namespace = self.scope(account, &scope.mailbox)?;
        if !self.full || namespace != scope.account_namespace {
            return Err(CacheError::OutOfScope);
        }
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let current = current_scope(self, db, namespace, &scope.mailbox)?;
        if current.uid_validity != scope.uid_validity
            || current.high_uid != scope.high_uid
            || current.dirty_revision != scope.dirty_revision
            || current.restore_epoch != scope.restore_epoch
        {
            return Err(CacheError::NeedsVerification);
        }
        search_revision(db, scope)
    }

    /// The caller supplies full, successfully rendered text for the exact
    /// canonical body artifact. Display prefixes never enter this entry point.
    pub fn remember_full_body_index(
        &self,
        reference: Ref<'_>,
        kind: ArtifactKey,
        blob: BlobId,
        text: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), CacheError> {
        let ArtifactKey::FullBody {
            mime_version,
            html_version,
        } = kind
        else {
            return Err(CacheError::OutOfScope);
        };
        if !self.full || text.len() > MAX_TEXT_BYTES || text.contains('\0') || cancelled() {
            return Err(CacheError::OutOfScope);
        }
        kind.sql()?;
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let epoch = self.disk.restore_epoch();
        let check = || {
            if cancelled() || epoch != self.disk.restore_epoch() {
                Err(CacheError::NeedsVerification)
            } else {
                Ok(())
            }
        };
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        let index_version = index_html_version(html_version);
        let id = loop {
            check()?;
            let mut databases = self.lock()?;
            let database = databases
                .get_mut(&namespace)
                .ok_or(CacheError::OutOfScope)?;
            let start = self.write_checked(
                namespace,
                database,
                |tx| {
                    verify_body(tx, reference, kind, blob)?;
                    let message = present_message(tx, reference)?;
                    let prior = tx
                        .query_row(
                            "SELECT id,blob_id,digest,complete,html_version FROM body_indexes
                     WHERE message_id=?1 AND mime_version=?2 AND html_version IN (?3,?4)
                     ORDER BY html_version LIMIT 1",
                            params![message, mime_version, html_version, index_version],
                            |row| {
                                Ok(StoredIndex {
                                    id: row.get(0)?,
                                    blob: row.get(1)?,
                                    digest: row.get(2)?,
                                    complete: row.get(3)?,
                                    version: row.get(4)?,
                                })
                            },
                        )
                        .optional()?;
                    if let Some(prior) = prior {
                        if prior.version == index_version && prior.blob == blob.bytes() {
                            if prior.digest != digest {
                                return Err(CacheError::Corrupt);
                            }
                            return Ok(if prior.complete {
                                Build::Done
                            } else {
                                Build::Append(prior.id)
                            });
                        }
                        if sweep_index(tx, prior.id)?
                            || tx.query_row(
                                "SELECT EXISTS(SELECT 1 FROM body_indexes
                                 WHERE message_id=?1 AND mime_version=?2 AND html_version=?3)",
                                params![message, mime_version, index_version],
                                |row| row.get::<_, bool>(0),
                            )?
                        {
                            return Ok(Build::Sweep);
                        }
                    }
                    tx.execute(
                    "INSERT INTO body_indexes(message_id,mime_version,html_version,blob_id,digest)
                     VALUES(?1,?2,?3,?4,?5)",
                    params![message,mime_version,index_version,blob.bytes(),digest],
                )?;
                    Ok(Build::Append(tx.last_insert_rowid()))
                },
                check,
            )?;
            match start {
                Build::Done => return Ok(()),
                Build::Sweep => continue,
                Build::Append(id) => break id,
            }
        };
        let mut start = 0;
        let mut ordinal = 0i64;
        while start < text.len() {
            check()?;
            let end = segment_end(text, start);
            let overlap = text[..start]
                .char_indices()
                .rev()
                .nth(OVERLAP_CHARACTERS - 1)
                .map_or(0, |(index, _)| index);
            if ordinal >= SEGMENT_STRIDE {
                return Err(CacheError::Corrupt);
            }
            let mut databases = self.lock()?;
            let database = databases
                .get_mut(&namespace)
                .ok_or(CacheError::OutOfScope)?;
            self.write_checked(
                namespace,
                database,
                |tx| {
                    verify_body(tx, reference, kind, blob)?;
                    tx.execute(
                        "INSERT INTO body_segments VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO NOTHING",
                        params![segment_id(id, ordinal)?, id, ordinal, &text[overlap..end]],
                    )?;
                    Ok(())
                },
                check,
            )?;
            start = end;
            ordinal += 1;
        }
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        self.write_checked(
            namespace,
            database,
            |tx| {
                verify_body(tx, reference, kind, blob)?;
                let count: i64 = tx.query_row(
                    "SELECT count(*) FROM body_segments WHERE index_id=?1",
                    [id],
                    |row| row.get(0),
                )?;
                if count != ordinal {
                    return Err(CacheError::Corrupt);
                }
                if tx.execute(
                    "UPDATE body_indexes SET complete=1 WHERE id=?1 AND blob_id=?2 AND digest=?3",
                    params![id, blob.bytes(), digest],
                )? != 1
                {
                    return Err(CacheError::OutOfScope);
                }
                Ok(())
            },
            check,
        )
    }

    pub fn search_cached(
        &self,
        account: &AccountKey,
        mailbox: &str,
        query: &str,
        position: Option<SearchPosition>,
        limit: usize,
        body: Option<(u16, u16)>,
    ) -> Result<CachedSearchPage, CacheError> {
        if !self.full
            || !super::valid_mailbox(query)
            || !(1..=MAX_HITS).contains(&limit)
            || body.is_some_and(|(mime, html)| mime == 0 || html == 0)
            || position.is_some_and(|p| {
                p.uid == 0 || p.segment.is_some_and(|s| s >= SEGMENT_STRIDE as u16)
            })
        {
            return Err(CacheError::OutOfScope);
        }
        let short = query.chars().count() < 3;
        if position.is_some_and(|p| p.segment.is_some()) && (!short || body.is_none()) {
            return Err(CacheError::OutOfScope);
        }
        let phrase = (!short).then(|| format!("\"{}\"", query.replace('"', "\"\"")));
        let (mime, html) = body.unwrap_or_default();
        let index_version = index_html_version(html);
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let scope = current_scope(self, db, namespace, mailbox)?;
        let search_revision = search_revision(db, &scope)?;
        let (indexed_metadata, indexed_body): (i64, i64) = db.query_row(
            "SELECT indexed_count, coalesce((
                SELECT indexed_count FROM body_scope_count b WHERE b.mailbox=f.mailbox
                AND b.uid_validity=f.uid_validity AND b.mime_version=?3 AND b.html_version=?4
             ),0)
             FROM mailboxes f WHERE f.mailbox=?1 AND f.uid_validity=?2",
            params![mailbox, scope.uid_validity, mime, index_version],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let before = position.map_or(i64::from(u32::MAX) + 1, |value| {
            i64::from(value.uid) + i64::from(value.segment.is_some())
        });
        let mut candidates = db.prepare_cached(
            "SELECT m.id,m.uid,m.index_text,b.id FROM messages m
             LEFT JOIN artifacts a ON a.message_id=m.id AND a.kind=1 AND a.part=0
                 AND a.mime_version=?3 AND a.html_version=?4 AND a.state=1
             LEFT JOIN body_indexes b ON b.message_id=m.id AND b.mime_version=?3 AND b.html_version=?7
                 AND b.blob_id=a.blob_id AND b.complete
             WHERE m.mailbox=?1 AND m.uid_validity=?2 AND NOT m.tombstone AND m.uid<=?5 AND m.uid<?6
             ORDER BY m.uid DESC LIMIT 201",
        )?;
        let mut rows = candidates.query(params![
            mailbox,
            scope.uid_validity,
            mime,
            html,
            scope.high_uid,
            before,
            index_version,
        ])?;
        let mut items = Vec::with_capacity(limit);
        let mut last = None;
        let mut next = None;
        let mut scanned = 0;
        let mut remaining_segments = MAX_CANDIDATES;
        while let Some(row) = rows.next()? {
            if scanned == MAX_CANDIDATES || items.len() == limit {
                next = last.map(|uid| SearchPosition { uid, segment: None });
                break;
            }
            scanned += 1;
            let id: i64 = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let body_id: Option<i64> = row.get(3)?;
            let matched_metadata = match (phrase.as_deref(), row.get_ref(2)?) {
                (_, rusqlite::types::ValueRef::Null) => false,
                (Some(phrase), _) => db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM message_index WHERE message_index MATCH ?1 AND rowid=?2)",
                    params![phrase,id], |row| row.get(0),
                )?,
                (None, value) => value.as_str().map_err(|_| CacheError::Corrupt)?.contains(query),
            };
            let mut matched_body = false;
            if let Some(body_id) = body_id {
                if let Some(phrase) = phrase.as_deref() {
                    matched_body = db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM body_search WHERE body_search MATCH ?1
                           AND rowid>=?2 AND rowid<?3)",
                        params![
                            phrase,
                            segment_id(body_id, 0)?,
                            segment_id(body_id, SEGMENT_STRIDE)?
                        ],
                        |row| row.get(0),
                    )?;
                } else {
                    let first = position
                        .filter(|value| value.uid == uid)
                        .and_then(|value| value.segment)
                        .unwrap_or(0);
                    let (matched, more) =
                        short_match(db, body_id, first, query, &mut remaining_segments)?;
                    matched_body = matched;
                    if let Some(segment) = more.filter(|_| !matched_metadata) {
                        next = Some(SearchPosition {
                            uid,
                            segment: Some(segment),
                        });
                        break;
                    }
                }
            }
            if matched_metadata || matched_body {
                let reference = Ref {
                    account,
                    mailbox,
                    uid_validity: scope.uid_validity,
                    uid,
                };
                let message = cached_message(db, reference)?.ok_or(CacheError::Corrupt)?;
                items.push(CachedSearchHit {
                    message,
                    matched_metadata,
                    matched_body,
                });
            }
            last = Some(uid);
        }
        Ok(CachedSearchPage {
            scope,
            search_revision,
            items,
            next,
            indexed_metadata: indexed_metadata as u64,
            indexed_body: indexed_body as u64,
        })
    }
}

/// Returns true while the parent may still have segments to sweep.
fn sweep_index(tx: &rusqlite::Transaction<'_>, id: i64) -> Result<bool, CacheError> {
    tx.execute("UPDATE body_indexes SET complete=0 WHERE id=?1", [id])?;
    if tx.execute(
        "DELETE FROM body_segments WHERE id IN (
            SELECT id FROM body_segments WHERE index_id=?1 LIMIT 16
         )",
        [id],
    )? != 0
    {
        return Ok(true);
    }
    tx.execute("DELETE FROM body_indexes WHERE id=?1", [id])?;
    Ok(false)
}

fn search_revision(db: &Connection, scope: &ScopeSnapshot) -> Result<u64, CacheError> {
    let revision: i64 = db
        .query_row(
            "SELECT revision FROM search_scope WHERE mailbox=?1 AND uid_validity=?2",
            params![scope.mailbox, scope.uid_validity],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default();
    revision.try_into().map_err(|_| CacheError::Corrupt)
}

fn short_match(
    db: &rusqlite::Connection,
    body: i64,
    first: u16,
    query: &str,
    budget: &mut usize,
) -> Result<(bool, Option<u16>), CacheError> {
    let mut select = db.prepare_cached(
        "SELECT ordinal,text FROM body_segments WHERE index_id=?1 AND ordinal>=?2
         ORDER BY ordinal LIMIT ?3",
    )?;
    let mut rows = select.query(params![body, first, (*budget + 1) as u32])?;
    while let Some(row) = rows.next()? {
        if *budget == 0 {
            return Ok((false, Some(row.get(0)?)));
        }
        *budget -= 1;
        let text = row.get_ref(1)?.as_str().map_err(|_| CacheError::Corrupt)?;
        if text.len() > SEGMENT_BYTES + 4 * OVERLAP_CHARACTERS {
            return Err(CacheError::Corrupt);
        }
        if text.contains(query) {
            return Ok((true, None));
        }
    }
    Ok((false, None))
}

fn segment_end(text: &str, start: usize) -> usize {
    let mut end = (start + SEGMENT_BYTES).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn segment_id(index: i64, ordinal: i64) -> Result<i64, CacheError> {
    index
        .checked_mul(SEGMENT_STRIDE)
        .and_then(|id| id.checked_add(ordinal))
        .ok_or(CacheError::Corrupt)
}

fn verify_body(
    db: &rusqlite::Connection,
    reference: Ref<'_>,
    kind: ArtifactKey,
    blob: BlobId,
) -> Result<(), CacheError> {
    if !matches!(artifact(db, reference, kind)?, ArtifactState::Materialized(info) if info.id == blob)
    {
        return Err(CacheError::OutOfScope);
    }
    let verified: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM body_leaf_metadata WHERE blob_id=?1)",
        [blob.bytes()],
        |row| row.get(0),
    )?;
    if !verified {
        return Err(CacheError::OutOfScope);
    }
    Ok(())
}
