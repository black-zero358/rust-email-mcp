use crate::error::{MailError, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Mutex, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Operation {
    pub status: String,
    pub message_id: String,
    pub warnings: Vec<String>,
}

pub struct Journal(Mutex<Connection>);
impl Journal {
    #[cfg(test)]
    pub(crate) fn fail_writes(&self) {
        self.0
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
    }
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).map_err(|_| MailError::storage())?;
        db.busy_timeout(Duration::from_secs(2))
            .map_err(|_| MailError::storage())?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS operations (
              principal TEXT NOT NULL, operation_key TEXT NOT NULL, fingerprint TEXT NOT NULL,
              status TEXT NOT NULL, message_id TEXT NOT NULL, warnings TEXT NOT NULL DEFAULT '[]',
              created_at INTEGER NOT NULL DEFAULT (unixepoch()), updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
              PRIMARY KEY(principal, operation_key));
            UPDATE operations SET status='unknown' WHERE status='sending';")
            .map_err(|_| MailError::storage())?;
        Ok(Self(Mutex::new(db)))
    }
    /// Claim commits before SMTP. Keys survive restarts and are scoped to the authenticated principal.
    pub fn claim(
        &self,
        principal: &str,
        key: &str,
        fingerprint: &str,
        message_id: &str,
    ) -> Result<Option<Operation>> {
        let mut db = self.0.lock().map_err(|_| MailError::storage())?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| MailError::storage())?;
        let old: Option<(String,String,String,String)> = tx.query_row(
            "SELECT fingerprint,status,message_id,warnings FROM operations WHERE principal=?1 AND operation_key=?2",
            params![principal,key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_| MailError::storage())?;
        if let Some((hash, status, message_id, warnings)) = old {
            if hash != fingerprint {
                return Err(MailError {
                    code: "IDEMPOTENCY_CONFLICT",
                    message: "This operation key was already used with different arguments.",
                    retryable: false,
                });
            }
            return Ok(Some(Operation {
                status,
                message_id,
                warnings: serde_json::from_str(&warnings).map_err(|_| MailError::storage())?,
            }));
        }
        tx.execute("INSERT INTO operations(principal,operation_key,fingerprint,status,message_id) VALUES (?1,?2,?3,'sending',?4)", params![principal,key,fingerprint,message_id]).map_err(|_| MailError::storage())?;
        tx.commit().map_err(|_| MailError::storage())?;
        Ok(None)
    }
    pub fn finish(
        &self,
        principal: &str,
        key: &str,
        status: &str,
        warnings: &[String],
    ) -> Result<()> {
        let db = self.0.lock().map_err(|_| MailError::storage())?;
        let count = db.execute("UPDATE operations SET status=?3,warnings=?4,updated_at=unixepoch() WHERE principal=?1 AND operation_key=?2",
            params![principal,key,status,serde_json::to_string(warnings).map_err(|_| MailError::storage())?]).map_err(|_| MailError::storage())?;
        if count != 1 {
            return Err(MailError::storage());
        }
        Ok(())
    }
    pub fn get(&self, principal: &str, key: &str) -> Result<Option<Operation>> {
        self.0.lock().map_err(|_| MailError::storage())?.query_row(
            "SELECT status,message_id,warnings FROM operations WHERE principal=?1 AND operation_key=?2", params![principal,key],
            |r| Ok(Operation {status:r.get(0)?,message_id:r.get(1)?,warnings:serde_json::from_str(&r.get::<_,String>(2)?).unwrap_or_default()}))
            .optional().map_err(|_| MailError::storage())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_dedup_and_principal_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let j = Journal::open(&path).unwrap();
        assert!(j.claim("a", "key", "hash", "id").unwrap().is_none());
        assert_eq!(
            j.claim("a", "key", "hash", "other")
                .unwrap()
                .unwrap()
                .status,
            "sending"
        );
        assert_eq!(
            j.claim("a", "key", "different", "id").unwrap_err().code,
            "IDEMPOTENCY_CONFLICT"
        );
        assert!(j.claim("b", "key", "hash", "id").unwrap().is_none());
        j.finish("a", "key", "accepted", &["cleanup_failed".into()])
            .unwrap();
        drop(j);
        let j = Journal::open(&path).unwrap();
        assert_eq!(j.get("a", "key").unwrap().unwrap().status, "accepted");
        assert_eq!(j.get("b", "key").unwrap().unwrap().status, "unknown");
    }
    #[test]
    fn concurrent_claim_has_single_winner() {
        let dir = tempfile::tempdir().unwrap();
        let j = std::sync::Arc::new(Journal::open(&dir.path().join("db")).unwrap());
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let j = j.clone();
                std::thread::spawn(move || j.claim("a", "key", "hash", "id").unwrap().is_none())
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .filter_map(|t| t.join().ok())
                .filter(|v| *v)
                .count(),
            1
        );
    }
}
