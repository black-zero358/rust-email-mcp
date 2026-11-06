//! Durable instance witness shared by cache-off and cache-enabled runs.
use super::{normalized_file, paths_conflict, private_file_options, sidecar};
use crate::config::Config;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata},
    io::{ErrorKind, Read, Write},
    path::Path,
    time::{Duration, Instant},
};
use uuid::Uuid;

const ERROR: &str = "Cache run witness is unavailable or invalid. Check its private files and ensure only one service owns this account registry.";
const MAGIC: &[u8; 8] = b"REMCPRUN";
const VERSION: [u8; 2] = 1u16.to_be_bytes();
const PAYLOAD_BYTES: usize = 8 + 2 + 32 + 16 + 16;
const RECORD_BYTES: usize = PAYLOAD_BYTES + 32;
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Receipts from the immediately preceding managed process are comparable
/// only within the same instance.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct CacheRunIdentity {
    pub instance: Uuid,
    pub previous: Option<Uuid>,
    pub current: Uuid,
}

/// Owns the witness lock through request admission, shutdown and final cache
/// receipt publication. It is independent of both SQLite databases.
pub struct CacheRun {
    identity: CacheRunIdentity,
    _lock: File,
}

impl CacheRun {
    /// Call after validating the account registry and before opening Journal
    /// or accepting any requests, including in off and recovery modes.
    pub fn begin(config: &Config) -> anyhow::Result<Self> {
        Self::begin_inner(config, LOCK_WAIT).map_err(|_| anyhow::anyhow!(ERROR))
    }

    pub fn identity(&self) -> CacheRunIdentity {
        self.identity
    }

    fn begin_inner(config: &Config, wait: Duration) -> anyhow::Result<Self> {
        let operation = (config.database != ":memory:")
            .then(|| normalized_file(Path::new(&config.database)))
            .transpose()?;
        let requested = config.account_registry.clone().unwrap_or_else(|| {
            operation
                .as_deref()
                .and_then(Path::parent)
                .unwrap_or_else(|| Path::new("data"))
                .join("accounts.sqlite")
        });
        let registry = normalized_file(&requested)?;
        if let Some(operation) = &operation {
            anyhow::ensure!(!paths_conflict(operation, &registry)?, ERROR);
        }
        let metadata = fs::symlink_metadata(&registry)?;
        anyhow::ensure!(metadata.is_file(), ERROR);
        let witness = sidecar(&registry, ".cache-run");
        let staged = sidecar(&registry, ".cache-run.stage");
        let lock_path = sidecar(&registry, ".cache-run.lock");
        let lock = match private_file_options().create_new(true).open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                open_checked(&lock_path, &metadata)?.ok_or_else(|| anyhow::anyhow!(ERROR))?
            }
            Err(error) => return Err(error.into()),
        };
        check_private(&lock.metadata()?, &metadata)?;
        let deadline = Instant::now() + wait;
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }

        if open_checked(&staged, &metadata)?.is_some() {
            fs::remove_file(&staged)?;
        }
        let registry_binding = registry_binding(&registry);
        let prior = open_checked(&witness, &metadata)?
            .map(|file| read_record(file, registry_binding))
            .transpose()?
            .flatten();
        let identity = CacheRunIdentity {
            instance: prior.map_or_else(Uuid::new_v4, |(instance, _)| instance),
            previous: prior.map(|(_, current)| current),
            current: Uuid::new_v4(),
        };
        publish(
            &registry,
            &witness,
            &staged,
            &metadata,
            registry_binding,
            identity,
        )?;
        Ok(Self {
            identity,
            _lock: lock,
        })
    }
}

fn registry_binding(registry: &Path) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"rust-email-mcp/cache-run-registry/v1\0");
    hash.update(registry.as_os_str().as_encoded_bytes());
    hash.finalize().into()
}

fn open_checked(path: &Path, registry: &Metadata) -> anyhow::Result<Option<File>> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    check_private(&before, registry)?;
    let file = private_file_options().open(path)?;
    let after = file.metadata()?;
    check_private(&after, registry)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            before.dev() == after.dev() && before.ino() == after.ino(),
            ERROR
        );
    }
    Ok(Some(file))
}

fn check_private(file: &Metadata, registry: &Metadata) -> anyhow::Result<()> {
    anyhow::ensure!(file.is_file(), ERROR);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            file.mode() & 0o7777 == 0o600 && file.nlink() == 1 && file.uid() == registry.uid(),
            ERROR
        );
    }
    #[cfg(not(unix))]
    let _ = registry;
    Ok(())
}

fn read_record(mut file: File, binding: [u8; 32]) -> anyhow::Result<Option<(Uuid, Uuid)>> {
    let mut record = [0; RECORD_BYTES];
    file.read_exact(&mut record)?;
    let (payload, checksum) = record.split_at(PAYLOAD_BYTES);
    anyhow::ensure!(
        file.read(&mut [0])? == 0
            && &payload[..8] == MAGIC
            && payload[8..10] == VERSION
            && checksum == <[u8; 32]>::from(Sha256::digest(payload)),
        ERROR
    );
    let instance = Uuid::from_bytes(payload[42..58].try_into()?);
    let current = Uuid::from_bytes(payload[58..74].try_into()?);
    anyhow::ensure!(
        instance.get_version_num() == 4 && current.get_version_num() == 4,
        ERROR
    );
    Ok((payload[10..42] == binding).then_some((instance, current)))
}

fn publish(
    registry: &Path,
    witness: &Path,
    staged: &Path,
    registry_metadata: &Metadata,
    binding: [u8; 32],
    identity: CacheRunIdentity,
) -> anyhow::Result<()> {
    let mut record = [0; RECORD_BYTES];
    let (payload, checksum) = record.split_at_mut(PAYLOAD_BYTES);
    payload[..8].copy_from_slice(MAGIC);
    payload[8..10].copy_from_slice(&VERSION);
    payload[10..42].copy_from_slice(&binding);
    payload[42..58].copy_from_slice(identity.instance.as_bytes());
    payload[58..74].copy_from_slice(identity.current.as_bytes());
    checksum.copy_from_slice(&Sha256::digest(payload));

    let mut file = private_file_options().create_new(true).open(staged)?;
    check_private(&file.metadata()?, registry_metadata)?;
    file.write_all(&record)?;
    file.sync_all()?;
    drop(file);
    fs::rename(staged, witness)?;
    #[cfg(unix)]
    File::open(registry.parent().ok_or_else(|| anyhow::anyhow!(ERROR))?)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[path = "cache_run/tests.rs"]
mod tests;
