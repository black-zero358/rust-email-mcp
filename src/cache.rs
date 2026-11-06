//! Private, quota-bounded storage for administrator-selected account scopes.
use crate::accounts::{AccountKey, CacheRunIdentity};
use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

mod blob;
mod body_index;
#[cfg(all(test, unix))]
mod content_tests;
pub mod cursor;
mod dirty;
mod disk;
mod evidence;
#[cfg(all(test, unix))]
mod full_tests;
mod metadata;
mod schema;
pub mod sync_state;
#[cfg(all(test, unix))]
mod tests;
mod thread_index;

pub use blob::{
    Access, ArtifactKey, ArtifactLayer, ArtifactState, BlobId, BlobInfo, BlobLease, BlobStage,
    LayerCoverage, Omission,
};
pub(crate) use body_index::{BODY_INDEX_REBUILD_BATCH, BODY_INDEX_VERSION};
pub use body_index::{CachedSearchHit, CachedSearchPage, SearchPosition};
pub use dirty::{DirtyStamp, DirtyTicket, ListingStamp};
pub use evidence::{
    CachedListingPage, CachedMailbox, CachedMessage, ListingEvidence, MailboxEvidence,
};
pub(crate) use metadata::MAX_CACHED_HEADER_BYTES;
pub use metadata::{AccountStatus, FolderStatus, SearchHit, SearchPage};
pub use thread_index::{ScopeSnapshot, THREAD_INDEX_VERSION, ThreadId, ThreadRole};

const MAX_ACCOUNTS: usize = 16;
const PAGE_BYTES: u64 = 4096;
pub const MIN_BYTES: u64 = 1024 * 1024;
pub const MAX_BYTES: u64 = i64::MAX as u64;
pub const MAX_FOLDERS_PER_ACCOUNT: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("Cache scope is unavailable.")]
    OutOfScope,
    #[error("Cache disk quota is exhausted.")]
    Quota,
    #[error("Cache content or schema is invalid.")]
    Corrupt,
    #[error("Cache storage is unavailable.")]
    Storage,
    #[error("Cache content needs online verification.")]
    NeedsVerification,
}

impl From<rusqlite::Error> for CacheError {
    fn from(error: rusqlite::Error) -> Self {
        match error.sqlite_error_code() {
            Some(ErrorCode::DiskFull) => Self::Quota,
            Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => Self::Corrupt,
            _ => Self::Storage,
        }
    }
}

impl From<std::io::Error> for CacheError {
    fn from(_: std::io::Error) -> Self {
        Self::Storage
    }
}

pub struct AccountScope {
    pub account: AccountKey,
    pub mailboxes: Vec<String>,
    pub max_bytes: u64,
}

#[derive(Clone, Copy)]
pub struct Ref<'a> {
    pub account: &'a AccountKey,
    pub mailbox: &'a str,
    pub uid_validity: u32,
    pub uid: u32,
}

type Namespace = [u8; 32];

struct Scope {
    mailboxes: Vec<String>,
    max_bytes: u64,
}

struct Database {
    connection: Connection,
    name: String,
    full_schema: bool,
    body_index_rebuild: BTreeMap<String, body_index::RebuildPosition>,
}

pub struct Cache {
    max_bytes: u64,
    scopes: BTreeMap<Namespace, Scope>,
    databases: Mutex<BTreeMap<Namespace, Database>>,
    // Drop every SQLite connection before releasing the instance lock.
    disk: disk::Root,
    startup_unclean: bool,
    full: bool,
    instance: uuid::Uuid,
    offline_disabled: AtomicBool,
    blob_leases: Mutex<BTreeMap<(Namespace, BlobId), usize>>,
    write_fault: AtomicU8,
    #[cfg(test)]
    fail_post_commit_check: std::sync::atomic::AtomicBool,
}

impl Cache {
    pub fn open(
        directory: &Path,
        max_bytes: u64,
        accounts: Vec<AccountScope>,
        reserved: &[PathBuf],
    ) -> Result<Arc<Self>, CacheError> {
        Self::open_mode(directory, max_bytes, accounts, reserved, false, None)
    }

    /// Open a full storage instance after administrator and runtime setup.
    /// Opening includes a durable unclean marker before any offline access.
    pub fn open_full(
        directory: &Path,
        max_bytes: u64,
        accounts: Vec<AccountScope>,
        reserved: &[PathBuf],
    ) -> Result<Arc<Self>, CacheError> {
        Self::open_mode(directory, max_bytes, accounts, reserved, true, None)
    }

    /// Open full storage using the process's durable runtime witness.
    pub fn open_full_with_run(
        directory: &Path,
        max_bytes: u64,
        accounts: Vec<AccountScope>,
        reserved: &[PathBuf],
        run: CacheRunIdentity,
    ) -> Result<Arc<Self>, CacheError> {
        Self::open_mode(directory, max_bytes, accounts, reserved, true, Some(run))
    }

    fn open_mode(
        directory: &Path,
        max_bytes: u64,
        accounts: Vec<AccountScope>,
        reserved: &[PathBuf],
        full: bool,
        run: Option<CacheRunIdentity>,
    ) -> Result<Arc<Self>, CacheError> {
        if accounts.is_empty()
            || accounts.len() > MAX_ACCOUNTS
            || !(MIN_BYTES..=MAX_BYTES).contains(&max_bytes)
        {
            return Err(CacheError::OutOfScope);
        }
        let mut scopes = BTreeMap::new();
        for scope in accounts {
            if !(MIN_BYTES..=max_bytes).contains(&scope.max_bytes)
                || scope.mailboxes.is_empty()
                || scope.mailboxes.len() > MAX_FOLDERS_PER_ACCOUNT
                || scope.mailboxes.iter().any(|name| !valid_mailbox(name))
            {
                return Err(CacheError::OutOfScope);
            }
            let mut mailboxes = scope.mailboxes;
            mailboxes.sort_unstable();
            let count = mailboxes.len();
            mailboxes.dedup();
            if mailboxes.len() != count
                || scopes
                    .insert(
                        scope.account.reference_namespace(),
                        Scope {
                            mailboxes,
                            max_bytes: scope.max_bytes,
                        },
                    )
                    .is_some()
            {
                return Err(CacheError::OutOfScope);
            }
        }
        let witness = run.map(|run| {
            let mut hash = Sha256::new();
            hash.update(b"email-mcp-full-cache-scope-v1\0");
            for (namespace, scope) in &scopes {
                hash.update(namespace);
                hash.update((scope.mailboxes.len() as u32).to_be_bytes());
                for mailbox in &scope.mailboxes {
                    hash.update((mailbox.len() as u32).to_be_bytes());
                    hash.update(mailbox.as_bytes());
                }
            }
            (run, hash.finalize().into())
        });
        let disk = disk::Root::open(directory, reserved, max_bytes, witness)?;
        let startup_unclean = disk.was_unclean();
        let cache = Arc::new(Self {
            disk,
            max_bytes,
            scopes,
            databases: Mutex::default(),
            startup_unclean,
            full,
            instance: uuid::Uuid::new_v4(),
            offline_disabled: AtomicBool::new(!full),
            blob_leases: Mutex::default(),
            write_fault: AtomicU8::new(0),
            #[cfg(test)]
            fail_post_commit_check: std::sync::atomic::AtomicBool::new(false),
        });
        {
            let mut databases = cache.lock()?;
            for (namespace, scope) in &cache.scopes {
                let name = disk::namespace_name(namespace);
                let path = cache.disk.prepare_partition(&name, max_bytes)?;
                cache.disk.verify_usage(&name, scope.max_bytes, max_bytes)?;
                let mut connection = schema::open(&path)?;
                cache
                    .disk
                    .reserve_write(&connection, &name, scope.max_bytes, max_bytes)?;
                let full_schema = schema::initialize(
                    &mut connection,
                    namespace,
                    startup_unclean,
                    cache.disk.restore_epoch(),
                    full,
                )?;
                cache.disk.verify_usage(&name, scope.max_bytes, max_bytes)?;
                let mut database = Database {
                    connection,
                    name,
                    full_schema,
                    body_index_rebuild: BTreeMap::new(),
                };
                if full {
                    cache.recover_blobs(*namespace, &mut database)?;
                }
                databases.insert(*namespace, database);
            }
        }
        Ok(cache)
    }

    pub fn enabled_for(&self, account: &AccountKey) -> bool {
        self.scopes.contains_key(&account.reference_namespace())
    }

    pub fn enabled_for_folder(&self, account: &AccountKey, mailbox: &str) -> bool {
        self.namespace_has_folder(&account.reference_namespace(), mailbox)
    }

    fn namespace_has_folder(&self, namespace: &Namespace, mailbox: &str) -> bool {
        self.scopes.get(namespace).is_some_and(|scope| {
            scope
                .mailboxes
                .binary_search_by(|name| name.as_str().cmp(mailbox))
                .is_ok()
        })
    }

    pub fn cursor_backend(self: &Arc<Self>) -> cursor::CursorBackend {
        cursor::CursorBackend(Arc::clone(self))
    }

    /// Called after all cache workers and writes have stopped.
    pub fn mark_clean(&self) -> Result<(), CacheError> {
        let databases = self.lock()?;
        self.write_health()?;
        if self.full
            && (self.offline_disabled.load(Ordering::Acquire)
                || !self
                    .blob_leases
                    .lock()
                    .map_err(|_| CacheError::Storage)?
                    .is_empty()
                || self.disk.has_staged_blobs()?)
        {
            return Ok(());
        }
        for (namespace, database) in databases.iter() {
            if self.disk.has_witness() {
                let selected = &self.scopes[namespace].mailboxes;
                for mailbox in selected {
                    if dirty::scope_dirty(&database.connection, mailbox)? {
                        return Ok(());
                    }
                }
                if dirty::has_claims(&database.connection)? {
                    return Ok(());
                }
                continue;
            }
            let dirty: bool = database.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM mailboxes WHERE dirty)",
                [],
                |row| row.get(0),
            )?;
            if dirty || (database.full_schema && dirty::has_dirty(&database.connection)?) {
                return Ok(());
            }
        }
        self.disk.mark_clean(self.max_bytes)
    }

    /// Run on an offline restored copy before reopening it for reads.
    /// Ordinary restarts retain cursor checkpoints for live revalidation.
    pub fn mark_restored(&self) -> Result<(), CacheError> {
        let mut databases = self.lock()?;
        let restore_epoch = self.disk.mark_restored(self.max_bytes)?;
        for (namespace, database) in databases.iter_mut() {
            let full_schema = database.full_schema;
            self.write(*namespace, database, |tx| {
                schema::restore(tx, restore_epoch, full_schema)
            })?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<Namespace, Database>>, CacheError> {
        self.databases.lock().map_err(|_| CacheError::Storage)
    }

    fn write_health(&self) -> Result<(), CacheError> {
        match self.write_fault.load(Ordering::Relaxed) {
            0 => Ok(()),
            1 => Err(CacheError::Quota),
            _ => Err(CacheError::Storage),
        }
    }

    fn write<T, E>(
        &self,
        namespace: Namespace,
        database: &mut Database,
        apply: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<CacheError> + From<rusqlite::Error>,
    {
        self.write_checked(namespace, database, apply, || Ok(()))
    }

    fn write_checked<T, E>(
        &self,
        namespace: Namespace,
        database: &mut Database,
        apply: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
        before_commit: impl FnOnce() -> Result<(), E>,
    ) -> Result<T, E>
    where
        E: From<CacheError> + From<rusqlite::Error>,
    {
        self.write_health()?;
        let quota = self
            .scopes
            .get(&namespace)
            .ok_or(CacheError::OutOfScope)?
            .max_bytes;
        self.disk
            .reserve_write(&database.connection, &database.name, quota, self.max_bytes)?;
        let tx = database
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = apply(&tx)?;
        self.disk
            .verify_usage(&database.name, quota, self.max_bytes)?;
        before_commit()?;
        tx.commit()?;
        // A fault observed after SQLite has committed cannot revoke a
        // published checkpoint. Stop subsequent writes and report fixed
        // health while leaving the committed revision available for replay.
        let checked = self
            .disk
            .verify_usage(&database.name, quota, self.max_bytes);
        #[cfg(test)]
        let checked = if self.fail_post_commit_check.swap(false, Ordering::Relaxed) {
            Err(CacheError::Storage)
        } else {
            checked
        };
        if let Err(error) = checked {
            let fault = if matches!(error, CacheError::Quota) {
                1
            } else {
                2
            };
            self.write_fault.store(fault, Ordering::Relaxed);
        }
        Ok(result)
    }

    fn scope(&self, account: &AccountKey, mailbox: &str) -> Result<Namespace, CacheError> {
        if !self.enabled_for_folder(account, mailbox) {
            return Err(CacheError::OutOfScope);
        }
        Ok(account.reference_namespace())
    }
}

fn valid_mailbox(mailbox: &str) -> bool {
    !mailbox.is_empty()
        && !mailbox.chars().any(char::is_control)
        && crate::input_limits::InputLimits::Read
            .mailbox(mailbox, "mailbox")
            .is_ok()
}
