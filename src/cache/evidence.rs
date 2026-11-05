//! Attribute and LIST snapshots obtained from completed IMAP observations.
use super::{Cache, CacheError, DirtyStamp, Ref, ScopeSnapshot, thread_index::current_scope};
use crate::accounts::AccountKey;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

const FLAGS_BYTES: usize = 8192;
const MAX_PAGE: usize = 50;
const FIRST_BEFORE: i64 = 1 + u32::MAX as i64;
const MESSAGE_COLUMNS: &str = "m.uid,m.header,m.complete_header,l.flags,l.flags_checked_at_ms,
    l.internal_date,l.size,l.observed_at_ms,l.date_checked_at_ms,l.size_checked_at_ms";

pub struct ListingEvidence<'a> {
    pub flags: Option<&'a [String]>,
    pub internal_date: Option<&'a str>,
    pub size: Option<u32>,
    pub observed_at_ms: i64,
}

#[derive(Debug)]
pub struct CachedMessage {
    pub uid: u32,
    pub header: Option<Vec<u8>>,
    pub header_complete: bool,
    pub flags: Option<Vec<String>>,
    pub flags_checked_at_ms: Option<i64>,
    pub internal_date: Option<String>,
    pub size: Option<u32>,
    pub observed_at_ms: Option<i64>,
    pub date_checked_at_ms: Option<i64>,
    pub size_checked_at_ms: Option<i64>,
}

#[derive(Debug)]
pub struct CachedListingPage {
    pub scope: ScopeSnapshot,
    pub items: Vec<CachedMessage>,
    pub next_before_uid: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MailboxEvidence {
    pub name: String,
    pub attributes: String,
    pub attribute_flags: Vec<String>,
    pub delimiter: Option<String>,
    pub selectable: bool,
    pub special_use: Vec<String>,
    pub role_source: Option<String>,
    pub role_candidates: Vec<String>,
}

#[derive(Debug)]
pub struct CachedMailbox {
    pub name: String,
    pub evidence: Option<MailboxEvidence>,
    pub checked_at_ms: Option<i64>,
}

impl Cache {
    pub fn remember_listing(
        &self,
        reference: Ref<'_>,
        evidence: ListingEvidence<'_>,
    ) -> Result<(), CacheError> {
        if !self.full
            || evidence.observed_at_ms < 0
            || evidence
                .internal_date
                .is_some_and(|date| !valid_text(date, 64))
        {
            return Err(CacheError::OutOfScope);
        }
        let flags = evidence.flags.map(encode_flags).transpose()?;
        self.update_message(reference, |tx, id| {
            tx.execute(
                "INSERT INTO listing_evidence VALUES(
                    ?1,?2,CASE WHEN ?2 IS NOT NULL THEN ?5 END,
                    ?3,CASE WHEN ?3 IS NOT NULL THEN ?5 END,
                    ?4,CASE WHEN ?4 IS NOT NULL THEN ?5 END,?5)
                 ON CONFLICT(message_id) DO UPDATE SET
                   flags=CASE WHEN excluded.flags_checked_at_ms>=coalesce(flags_checked_at_ms,-1)
                              THEN excluded.flags ELSE flags END,
                   flags_checked_at_ms=coalesce(max(flags_checked_at_ms,excluded.flags_checked_at_ms),flags_checked_at_ms,excluded.flags_checked_at_ms),
                   internal_date=CASE WHEN excluded.date_checked_at_ms>=coalesce(date_checked_at_ms,-1)
                              THEN excluded.internal_date ELSE internal_date END,
                   date_checked_at_ms=coalesce(max(date_checked_at_ms,excluded.date_checked_at_ms),date_checked_at_ms,excluded.date_checked_at_ms),
                   size=CASE WHEN excluded.size_checked_at_ms>=coalesce(size_checked_at_ms,-1)
                              THEN excluded.size ELSE size END,
                   size_checked_at_ms=coalesce(max(size_checked_at_ms,excluded.size_checked_at_ms),size_checked_at_ms,excluded.size_checked_at_ms),
                   observed_at_ms=max(observed_at_ms,excluded.observed_at_ms)",
                params![id, flags, evidence.internal_date, evidence.size, evidence.observed_at_ms],
            )?;
            Ok(())
        })
    }

    pub fn list_cached(
        &self,
        account: &AccountKey,
        mailbox: &str,
        before_uid: Option<u32>,
        limit: usize,
    ) -> Result<CachedListingPage, CacheError> {
        if !self.full || !(1..=MAX_PAGE).contains(&limit) || before_uid == Some(0) {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let scope = current_scope(self, db, namespace, mailbox)?;
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages AS m LEFT JOIN listing_evidence AS l ON l.message_id=m.id
             WHERE m.mailbox=?1 AND m.uid_validity=?2 AND NOT m.tombstone AND m.uid<=?3 AND m.uid<?4
             ORDER BY m.uid DESC LIMIT ?5"
        );
        let mut statement = db.prepare_cached(&sql)?;
        let mut rows = statement.query(params![
            mailbox,
            scope.uid_validity,
            scope.high_uid,
            before_uid.map_or(FIRST_BEFORE, i64::from),
            (limit + 1) as u32
        ])?;
        let mut items: Vec<CachedMessage> = Vec::with_capacity(limit);
        let mut next_before_uid = None;
        while let Some(row) = rows.next()? {
            if items.len() == limit {
                next_before_uid = items.last().map(|item| item.uid);
                break;
            }
            items.push(cached_row(row)?);
        }
        Ok(CachedListingPage {
            scope,
            items,
            next_before_uid,
        })
    }

    pub fn cached_message(&self, reference: Ref<'_>) -> Result<Option<CachedMessage>, CacheError> {
        self.read_cached_message(reference, true)
    }

    /// Use after online UIDVALIDITY and UID existence verification.
    pub fn cached_message_live_verified(
        &self,
        reference: Ref<'_>,
    ) -> Result<Option<CachedMessage>, CacheError> {
        self.read_cached_message(reference, false)
    }

    fn read_cached_message(
        &self,
        reference: Ref<'_>,
        offline: bool,
    ) -> Result<Option<CachedMessage>, CacheError> {
        if !self.full {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        if offline {
            self.check_offline(db, reference.mailbox, reference.uid_validity, namespace)?;
        }
        cached_message(db, reference)
    }

    pub fn offline_stamp(&self, reference: Ref<'_>) -> Result<DirtyStamp, CacheError> {
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let stamp = self.check_offline(db, reference.mailbox, reference.uid_validity, namespace)?;
        let present: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE mailbox=?1 AND uid_validity=?2 AND uid=?3 AND NOT tombstone)",
            params![reference.mailbox, reference.uid_validity, reference.uid], |row| row.get(0),
        )?;
        if !present {
            return Err(CacheError::OutOfScope);
        }
        Ok(stamp)
    }

    pub fn cached_parts(
        &self,
        reference: Ref<'_>,
    ) -> Result<Option<(Vec<u8>, DirtyStamp)>, CacheError> {
        let namespace = self.scope(reference.account, reference.mailbox)?;
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let stamp = self.check_offline(db, reference.mailbox, reference.uid_validity, namespace)?;
        Ok(db
            .query_row(
                "SELECT parts FROM messages WHERE mailbox=?1 AND uid_validity=?2 AND uid=?3
             AND NOT tombstone AND length(parts)<=262144",
                params![reference.mailbox, reference.uid_validity, reference.uid],
                |row| row.get(0),
            )
            .optional()?
            .map(|parts| (parts, stamp)))
    }

    pub fn remember_mailboxes(
        &self,
        account: &AccountKey,
        rows: &[MailboxEvidence],
        observed_at_ms: i64,
    ) -> Result<(), CacheError> {
        if !self.full || observed_at_ms < 0 || rows.len() > 2000 {
            return Err(CacheError::OutOfScope);
        }
        let namespace = account.reference_namespace();
        let mut encoded: Vec<(&String, Vec<u8>)> = Vec::new();
        for row in rows {
            if !self.enabled_for_folder(account, &row.name) {
                continue;
            }
            if encoded.iter().any(|(name, _)| *name == &row.name) || !valid_mailbox_evidence(row) {
                return Err(CacheError::OutOfScope);
            }
            let value = serde_json::to_vec(row).map_err(|_| CacheError::Corrupt)?;
            if value.len() > 32768 {
                return Err(CacheError::OutOfScope);
            }
            encoded.push((&row.name, value));
        }
        if encoded.is_empty() {
            return Ok(());
        }
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        self.write(namespace, database, |tx| {
            for (mailbox, evidence) in &encoded {
                tx.execute(
                    "INSERT INTO mailbox_evidence VALUES(?1,?2,?3)
                     ON CONFLICT(mailbox) DO UPDATE SET evidence=excluded.evidence,checked_at_ms=excluded.checked_at_ms
                     WHERE excluded.checked_at_ms>=mailbox_evidence.checked_at_ms",
                    params![mailbox, evidence, observed_at_ms],
                )?;
            }
            Ok(())
        })
    }

    pub fn cached_mailboxes(&self, account: &AccountKey) -> Result<Vec<CachedMailbox>, CacheError> {
        let namespace = account.reference_namespace();
        let scope = self.scopes.get(&namespace).ok_or(CacheError::OutOfScope)?;
        let databases = self.lock()?;
        self.require_offline_health()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let mut rows = Vec::with_capacity(scope.mailboxes.len());
        for name in &scope.mailboxes {
            let observed: Option<(Vec<u8>, i64)> = if super::dirty::scope_dirty(db, name)? {
                None
            } else {
                db.query_row(
                    "SELECT evidence,checked_at_ms FROM mailbox_evidence WHERE mailbox=?1 AND length(evidence)<=32768",
                    [name], |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?
            };
            let (evidence, checked_at_ms) = match observed {
                Some((payload, time)) => {
                    let evidence: MailboxEvidence =
                        serde_json::from_slice(&payload).map_err(|_| CacheError::Corrupt)?;
                    if evidence.name != *name || !valid_mailbox_evidence(&evidence) {
                        return Err(CacheError::Corrupt);
                    }
                    (Some(evidence), Some(time))
                }
                None => (None, None),
            };
            rows.push(CachedMailbox {
                name: name.clone(),
                evidence,
                checked_at_ms,
            });
        }
        Ok(rows)
    }

    /// Last server-confirmed Sent roles for the pre-SMTP dirty fence.
    pub fn confirmed_sent(&self, account: &AccountKey) -> Result<Vec<String>, CacheError> {
        let namespace = account.reference_namespace();
        let scope = self.scopes.get(&namespace).ok_or(CacheError::OutOfScope)?;
        if !self.full {
            return Err(CacheError::OutOfScope);
        }
        let databases = self.lock()?;
        let db = &databases
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .connection;
        let mut sent = Vec::new();
        for name in &scope.mailboxes {
            let payload: Option<Vec<u8>> = db.query_row(
                "SELECT evidence FROM mailbox_evidence WHERE mailbox=?1 AND length(evidence)<=32768",
                [name], |row| row.get(0),
            ).optional()?;
            let Some(payload) = payload else { continue };
            let evidence: MailboxEvidence =
                serde_json::from_slice(&payload).map_err(|_| CacheError::Corrupt)?;
            if evidence.name != *name || !valid_mailbox_evidence(&evidence) {
                return Err(CacheError::Corrupt);
            }
            if evidence.selectable
                && evidence.role_source.as_deref() == Some("server")
                && evidence
                    .attribute_flags
                    .iter()
                    .any(|flag| flag.eq_ignore_ascii_case("\\Sent"))
                && evidence.special_use.iter().any(|role| role == "sent")
            {
                sent.push(name.clone());
            }
        }
        Ok(sent)
    }
}

pub(super) fn cached_message(
    db: &Connection,
    reference: Ref<'_>,
) -> Result<Option<CachedMessage>, CacheError> {
    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages AS m JOIN mailboxes AS f USING(mailbox,uid_validity)
         LEFT JOIN listing_evidence AS l ON l.message_id=m.id
         WHERE m.mailbox=?1 AND m.uid_validity=?2 AND m.uid=?3 AND NOT m.tombstone"
    );
    let mut statement = db.prepare_cached(&sql)?;
    let mut rows = statement.query(params![
        reference.mailbox,
        reference.uid_validity,
        reference.uid
    ])?;
    rows.next()?.map(cached_row).transpose()
}

fn cached_row(row: &Row<'_>) -> Result<CachedMessage, CacheError> {
    use rusqlite::types::ValueRef;
    let flags = match row.get_ref(3)? {
        ValueRef::Null => None,
        ValueRef::Blob(bytes) if bytes.len() <= FLAGS_BYTES => {
            let flags: Vec<String> =
                serde_json::from_slice(bytes).map_err(|_| CacheError::Corrupt)?;
            if !valid_flags(&flags) {
                return Err(CacheError::Corrupt);
            }
            Some(flags)
        }
        _ => return Err(CacheError::Corrupt),
    };
    let header = match row.get_ref(1)? {
        ValueRef::Null => None,
        ValueRef::Blob(bytes) if bytes.len() <= 128 * 1024 => Some(bytes.to_vec()),
        _ => return Err(CacheError::Corrupt),
    };
    Ok(CachedMessage {
        uid: row.get(0)?,
        header,
        header_complete: row.get(2)?,
        flags,
        flags_checked_at_ms: row.get(4)?,
        internal_date: row.get(5)?,
        size: row.get(6)?,
        observed_at_ms: row.get(7)?,
        date_checked_at_ms: row.get(8)?,
        size_checked_at_ms: row.get(9)?,
    })
}

fn valid_text(text: &str, max_bytes: usize) -> bool {
    text.len() <= max_bytes && !text.chars().any(char::is_control)
}

fn valid_flags(flags: &[String]) -> bool {
    flags.len() <= 128
        && flags
            .iter()
            .all(|flag| !flag.is_empty() && valid_text(flag, 256))
        && flags.iter().map(String::len).sum::<usize>() <= 3072
}

fn encode_flags(flags: &[String]) -> Result<Vec<u8>, CacheError> {
    if !valid_flags(flags) {
        return Err(CacheError::OutOfScope);
    }
    let mut flags: Vec<_> = flags.iter().map(String::as_str).collect();
    flags.sort_unstable();
    flags.dedup();
    let bytes = serde_json::to_vec(&flags).map_err(|_| CacheError::Corrupt)?;
    if bytes.len() > FLAGS_BYTES {
        return Err(CacheError::OutOfScope);
    }
    Ok(bytes)
}

fn valid_mailbox_evidence(value: &MailboxEvidence) -> bool {
    valid_text(&value.attributes, 4096)
        && value
            .delimiter
            .as_ref()
            .is_none_or(|value| valid_text(value, 8))
        && value
            .role_source
            .as_deref()
            .is_none_or(|value| matches!(value, "server" | "configuration" | "protocol"))
        && valid_flags(&value.attribute_flags)
        && valid_flags(&value.special_use)
        && valid_flags(&value.role_candidates)
}
