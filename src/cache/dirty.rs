//! Durable mutation claims and compare-and-swap online reconciliation.
use super::{Cache, CacheError, MAX_FOLDERS_PER_ACCOUNT, Namespace};
use crate::accounts::AccountKey;
use rusqlite::{Connection, params};
use std::sync::atomic::Ordering;
use uuid::Uuid;

const MAX_CLAIMS: u32 = 20;

pub struct DirtyTicket {
    namespace: Namespace,
    id: [u8; 16],
    instance: Uuid,
    restore_epoch: u64,
}

#[derive(Clone, Eq, PartialEq)]
pub struct DirtyStamp {
    namespace: Namespace,
    instance: Uuid,
    mailbox: String,
    revision: i64,
    validity: Option<u32>,
    restore_epoch: u64,
}

impl Cache {
    /// Block explicit offline reads for this process before proceeding with a
    /// mailbox mutation whose durable cache claim could not be recorded.
    pub fn disable_offline(&self) {
        self.offline_disabled.store(true, Ordering::Release);
    }

    pub fn begin_dirty(
        &self,
        account: &AccountKey,
        mailboxes: &[&str],
    ) -> Result<DirtyTicket, CacheError> {
        let result = self.claim_dirty(account, mailboxes);
        if result.is_err() {
            self.disable_offline();
        }
        result
    }

    fn claim_dirty(
        &self,
        account: &AccountKey,
        mailboxes: &[&str],
    ) -> Result<DirtyTicket, CacheError> {
        if !self.full
            || mailboxes.is_empty()
            || mailboxes.len() > MAX_FOLDERS_PER_ACCOUNT
            || mailboxes.iter().enumerate().any(|(index, mailbox)| {
                !self.enabled_for_folder(account, mailbox) || mailboxes[..index].contains(mailbox)
            })
        {
            return Err(CacheError::OutOfScope);
        }
        let namespace = account.reference_namespace();
        let mut databases = self.lock()?;
        let database = databases
            .get_mut(&namespace)
            .ok_or(CacheError::OutOfScope)?;
        let restore_epoch = self.disk.restore_epoch();
        let id = *Uuid::new_v4().as_bytes();
        self.write(namespace, database, |tx| {
            let count: u32 =
                tx.query_row("SELECT count(*) FROM mutation_claims", [], |row| row.get(0))?;
            if count >= MAX_CLAIMS {
                return Err(CacheError::Quota);
            }
            tx.execute("INSERT INTO mutation_claims VALUES(?1)", [id])?;
            for mailbox in mailboxes {
                tx.execute(
                    "INSERT INTO scope_fences VALUES(?1,1,1)
                     ON CONFLICT(mailbox) DO UPDATE SET revision=revision+1,dirty=1",
                    [mailbox],
                )?;
                tx.execute(
                    "INSERT INTO mutation_scopes VALUES(?1,?2)",
                    params![id, mailbox],
                )?;
                tx.execute(
                    "UPDATE mailboxes SET dirty=1 WHERE mailbox=?1 AND NOT dirty",
                    [mailbox],
                )?;
            }
            Ok(DirtyTicket {
                namespace,
                id,
                instance: self.instance,
                restore_epoch,
            })
        })
    }

    /// A completed or uncertain remote write still needs online verification.
    pub fn finish_dirty(&self, ticket: DirtyTicket) -> Result<(), CacheError> {
        let mut databases = self.lock()?;
        if ticket.instance != self.instance || ticket.restore_epoch != self.disk.restore_epoch() {
            return Err(CacheError::OutOfScope);
        }
        let database = databases
            .get_mut(&ticket.namespace)
            .ok_or(CacheError::OutOfScope)?;
        self.write(ticket.namespace, database, |tx| {
            if tx.execute("DELETE FROM mutation_claims WHERE id=?1", [ticket.id])? != 1 {
                return Err(CacheError::OutOfScope);
            }
            Ok(())
        })
    }

    pub fn dirty_stamp(
        &self,
        account: &AccountKey,
        mailbox: &str,
    ) -> Result<DirtyStamp, CacheError> {
        if !self.full {
            return Err(CacheError::OutOfScope);
        }
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        if inflight(&database.connection, mailbox)? {
            return Err(CacheError::NeedsVerification);
        }
        let (validity, revision, _) = observation(&database.connection, mailbox)?;
        Ok(DirtyStamp {
            namespace,
            instance: self.instance,
            mailbox: mailbox.into(),
            revision,
            validity,
            restore_epoch: self.disk.restore_epoch(),
        })
    }

    /// Clear only after the caller has fully verified the affected scope
    /// online against the stamp. An EXAMINE alone supplies no such evidence.
    pub fn clear_dirty_if_verified(
        &self,
        stamp: &DirtyStamp,
        uid_validity: u32,
    ) -> Result<(), CacheError> {
        let mut databases = self.lock()?;
        if !self.full
            || stamp.instance != self.instance
            || stamp.restore_epoch != self.disk.restore_epoch()
            || stamp.validity != Some(uid_validity)
        {
            return Err(CacheError::NeedsVerification);
        }
        let database = databases
            .get_mut(&stamp.namespace)
            .ok_or(CacheError::OutOfScope)?;
        self.write(stamp.namespace, database, |tx| {
            let (validity, revision, _) = observation(tx, &stamp.mailbox)?;
            if validity != stamp.validity
                || revision != stamp.revision
                || inflight(tx, &stamp.mailbox)?
            {
                return Err(CacheError::NeedsVerification);
            }
            tx.execute(
                "UPDATE scope_fences SET dirty=0 WHERE mailbox=?1",
                [&stamp.mailbox],
            )?;
            tx.execute(
                "UPDATE mailboxes SET dirty=0 WHERE mailbox=?1",
                [&stamp.mailbox],
            )?;
            Ok(())
        })
    }

    pub(super) fn check_offline(
        &self,
        db: &Connection,
        mailbox: &str,
        uid_validity: u32,
        namespace: Namespace,
    ) -> Result<DirtyStamp, CacheError> {
        if self.offline_disabled.load(Ordering::Acquire) {
            return Err(CacheError::NeedsVerification);
        }
        self.write_health()?;
        let (validity, revision, dirty) = observation(db, mailbox)?;
        if validity != Some(uid_validity) || dirty {
            return Err(CacheError::NeedsVerification);
        }
        Ok(DirtyStamp {
            namespace,
            instance: self.instance,
            mailbox: mailbox.into(),
            revision,
            validity,
            restore_epoch: self.disk.restore_epoch(),
        })
    }

    pub fn offline_scope_allowed(
        &self,
        account: &AccountKey,
        mailbox: &str,
        uid_validity: u32,
    ) -> Result<(), CacheError> {
        let namespace = self.scope(account, mailbox)?;
        let databases = self.lock()?;
        let database = databases.get(&namespace).ok_or(CacheError::OutOfScope)?;
        self.check_offline(&database.connection, mailbox, uid_validity, namespace)?;
        Ok(())
    }
}

fn inflight(db: &Connection, mailbox: &str) -> Result<bool, CacheError> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM mutation_scopes WHERE mailbox=?1)",
        [mailbox],
        |row| row.get(0),
    )?)
}

pub(super) fn scope_dirty(db: &Connection, mailbox: &str) -> Result<bool, CacheError> {
    Ok(observation(db, mailbox)?.2)
}

fn observation(db: &Connection, mailbox: &str) -> Result<(Option<u32>, i64, bool), CacheError> {
    Ok(db.query_row(
        "SELECT m.uid_validity,coalesce(f.revision,0),coalesce(m.dirty,0) OR coalesce(f.dirty,0)
         FROM (SELECT ?1 AS mailbox) AS s
         LEFT JOIN mailboxes AS m USING(mailbox)
         LEFT JOIN scope_fences AS f USING(mailbox)",
        [mailbox],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

pub(super) fn has_dirty(db: &Connection) -> Result<bool, CacheError> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM scope_fences WHERE dirty)
              OR EXISTS(SELECT 1 FROM mutation_claims)",
        [],
        |row| row.get(0),
    )?)
}
