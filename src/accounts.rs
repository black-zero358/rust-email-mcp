//! Persistent identity for the legacy default account, independent of send records.
use crate::config::Config;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

#[path = "accounts/catalog.rs"]
mod catalog;
pub use catalog::{AccountCatalog, AccountLease, open_catalog};

pub const DEFAULT_ACCOUNT_ID: &str = "default";
pub const LEGACY_REGISTRATION_KEY: &str = "legacy-default";
pub const ACCOUNT_ID_MAX_CHARACTERS: usize = 64;
pub fn valid_account_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= ACCOUNT_ID_MAX_CHARACTERS
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

const MAX_REGISTRY_BYTES: u64 = 256 * 1024;
const REGISTRY_ERROR: &str = "Account registry is unavailable or invalid. Restore the account registry with its operation database before starting.";
const BINDING_ERROR: &str = "Legacy default account binding changed. Keep its mailbox and operation database path fixed; use an explicit account migration before starting.";
const TABLE_SQL: &str = "CREATE TABLE legacy_default (
            alias TEXT PRIMARY KEY CHECK(alias='default'),
            uuid TEXT NOT NULL UNIQUE,
            generation INTEGER NOT NULL CHECK(generation=1),
            identity_fingerprint TEXT NOT NULL,
            operation_path_fingerprint TEXT NOT NULL)";

#[cfg(test)]
#[path = "accounts/tests.rs"]
mod tests;

/// External account aliases may change in later configurations; references use this key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AccountKey {
    uuid: Uuid,
    generation: u64,
}

impl AccountKey {
    pub(crate) fn reference_namespace(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"email-mcp-account-key-v1\0");
        hash.update(self.uuid.as_bytes());
        hash.update(self.generation.to_be_bytes());
        hash.finalize().into()
    }
    #[cfg(test)]
    pub(crate) fn synthetic() -> Self {
        Self {
            uuid: Uuid::new_v4(),
            generation: 1,
        }
    }
}

/// Validate before opening the journal: journal recovery itself changes send state.
pub fn open_default(config: &Config) -> anyhow::Result<AccountKey> {
    open_default_inner(config).map_err(|error| {
        if error.to_string() == BINDING_ERROR {
            anyhow::anyhow!(BINDING_ERROR)
        } else {
            // Filesystem and SQLite errors can include paths and configured identities.
            anyhow::anyhow!(REGISTRY_ERROR)
        }
    })
}

fn open_default_inner(config: &Config) -> anyhow::Result<AccountKey> {
    let operation = if config.database == ":memory:" {
        None
    } else {
        Some(normalized_file(Path::new(&config.database))?)
    };
    let requested = config.account_registry.clone().unwrap_or_else(|| {
        operation
            .as_ref()
            .map(|path| {
                path.parent()
                    .expect("absolute operation path")
                    .join("accounts.sqlite")
            })
            .unwrap_or_else(|| PathBuf::from("data/accounts.sqlite"))
    });
    let registry = normalized_file(&requested)?;
    if let Some(operation) = &operation {
        anyhow::ensure!(!paths_conflict(operation, &registry)?, REGISTRY_ERROR);
    }
    let identity = fingerprint(&serde_json::to_vec(&(
        "legacy-default-v1",
        &config.address,
        config.imap_host.to_ascii_lowercase(),
        config.imap_port,
    ))?);
    let operation_path = match &operation {
        Some(path) => fingerprint(&serde_json::to_vec(&(
            "persistent-operation-path-v1",
            path.as_os_str().as_encoded_bytes(),
        ))?),
        None => fingerprint(b"memory-operation-database-v1"),
    };
    let lock_path = sidecar(&registry, ".lock");
    let mut options = private_file_options();
    let lock = options.create(true).open(&lock_path)?;
    ensure_owner_only(&lock.metadata()?)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => anyhow::bail!(REGISTRY_ERROR),
        }
    }
    if let Some(operation) = &operation {
        anyhow::ensure!(!paths_conflict(operation, &registry)?, REGISTRY_ERROR);
    }
    // A single fixed stage is recoverable after a crash, protected by the persistent lock.
    let staged = StagedRegistry(sidecar(&registry, ".staging"));
    staged.cleanup()?;
    if !registry.try_exists()? {
        publish_registry(&registry, &staged.0, &identity, &operation_path)?;
    }
    // Reopen only the official path after publishing, including a competing winner.
    load_registry(&registry, &identity, &operation_path)
}

fn fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn normalized_file(path: &Path) -> anyhow::Result<PathBuf> {
    if path.try_exists()? {
        anyhow::ensure!(path.is_file(), REGISTRY_ERROR);
        return Ok(fs::canonicalize(path)?);
    }
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!(REGISTRY_ERROR))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)?;
    Ok(fs::canonicalize(parent)?.join(name))
}

fn paths_conflict(operation: &Path, registry: &Path) -> anyhow::Result<bool> {
    for operation_suffix in ["", "-journal", "-wal", "-shm"] {
        let operation_file = sidecar(operation, operation_suffix);
        for registry_suffix in [
            "",
            "-journal",
            "-wal",
            "-shm",
            ".lock",
            ".staging",
            ".staging-journal",
            ".staging-wal",
            ".staging-shm",
            ".v1-backup",
            ".previous",
            ".previous-staging",
        ] {
            if same_file(&operation_file, &sidecar(registry, registry_suffix))? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn same_file(a: &Path, b: &Path) -> anyhow::Result<bool> {
    if a == b {
        return Ok(true);
    }
    if !(a.try_exists()? && b.try_exists()?) {
        return Ok(false);
    }
    let canonical = |path| match fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    let (Some(canonical_a), Some(canonical_b)) = (canonical(a)?, canonical(b)?) else {
        return Ok(false);
    };
    if canonical_a == canonical_b {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = |path| match fs::metadata(path) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        };
        let (Some(a), Some(b)) = (metadata(a)?, metadata(b)?) else {
            return Ok(false);
        };
        if a.dev() == b.dev() && a.ino() == b.ino() {
            return Ok(true);
        }
    }
    Ok(false)
}

struct StagedRegistry(PathBuf);
impl StagedRegistry {
    fn cleanup(&self) -> std::io::Result<()> {
        for path in [
            self.0.clone(),
            sidecar(&self.0, "-journal"),
            sidecar(&self.0, "-wal"),
            sidecar(&self.0, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
impl Drop for StagedRegistry {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn private_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn ensure_owner_only(metadata: &fs::Metadata) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(metadata.permissions().mode() & 0o077 == 0, REGISTRY_ERROR);
    }
    Ok(())
}

/// Publish a fully initialized SQLite file without replacing an existing identity.
fn publish_registry(
    path: &Path,
    staged: &Path,
    identity: &str,
    operation_path: &str,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!(REGISTRY_ERROR))?;
    let file = private_file_options().create_new(true).open(staged)?;
    let mut db = Connection::open(staged)?;
    db.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA max_page_count=64;",
    )?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch("PRAGMA user_version=1;")?;
    tx.execute_batch(TABLE_SQL)?;
    tx.execute(
        "INSERT INTO legacy_default VALUES ('default',?1,1,?2,?3)",
        params![Uuid::new_v4().to_string(), identity, operation_path],
    )?;
    tx.commit()?;
    db.close().map_err(|(_, error)| error)?;
    file.sync_all()?;
    match fs::hard_link(staged, path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    fs::remove_file(staged)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn load_registry(path: &Path, identity: &str, operation_path: &str) -> anyhow::Result<AccountKey> {
    let metadata = fs::metadata(path)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() <= MAX_REGISTRY_BYTES,
        REGISTRY_ERROR
    );
    ensure_owner_only(&metadata)?;
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_secs(2))?;
    let version: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    anyhow::ensure!(version == 1, REGISTRY_ERROR);
    let entries: i64 = db.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;
    let table_sql: Option<String> = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='legacy_default' AND type='table'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    anyhow::ensure!(
        entries == 1 && table_sql.as_deref() == Some(TABLE_SQL),
        REGISTRY_ERROR
    );
    let integrity: String = db.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    anyhow::ensure!(integrity == "ok", REGISTRY_ERROR);
    let count: i64 = db.query_row("SELECT count(*) FROM legacy_default", [], |row| row.get(0))?;
    anyhow::ensure!(count == 1, REGISTRY_ERROR);
    let record: Option<(String, i64, String, String)> = db.query_row(
        "SELECT uuid,generation,identity_fingerprint,operation_path_fingerprint FROM legacy_default WHERE alias='default'",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    let (uuid, generation, stored_identity, stored_path) =
        record.ok_or_else(|| anyhow::anyhow!(REGISTRY_ERROR))?;
    anyhow::ensure!(
        stored_identity == identity && stored_path == operation_path,
        BINDING_ERROR
    );
    let uuid = Uuid::parse_str(&uuid)?;
    anyhow::ensure!(!uuid.is_nil() && generation == 1, REGISTRY_ERROR);
    Ok(AccountKey {
        uuid,
        generation: 1,
    })
}
