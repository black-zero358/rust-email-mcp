//! Filesystem ownership, recovery marker, and physical SQLite reservations.
use super::{CacheError, Namespace, PAGE_BYTES};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, Metadata, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const LOCK: &str = ".cache-lock";
const MANIFEST: &str = "manifest.json";
const STAGE: &str = ".manifest-next";
const DB: &str = "cache.sqlite";
const MAX_PARTITIONS: usize = 256;
const MANIFEST_BYTES: u64 = 32 * 1024;
// SQLite rollback journals use at most 64 KiB sectors. With cache_spill
// disabled, the transaction writes a single header and journals each original
// database page at most once, preceded by its page number and followed by its checksum.
const JOURNAL_HEADER_RESERVE: u64 = 2 * 64 * 1024;
const ROOT_WRITE_RESERVE: u64 = MANIFEST_BYTES + 2 * PAGE_BYTES;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    clean: bool,
    restore_epoch: u64,
    partitions: Vec<String>,
}

pub(super) struct Root {
    path: PathBuf,
    identity: Identity,
    _lock: File,
    was_unclean: bool,
    restore_epoch: AtomicU64,
}

struct Usage {
    total: u64,
    partitions: BTreeMap<String, u64>,
}

impl Root {
    pub(super) fn open(path: &Path, reserved: &[PathBuf], limit: u64) -> Result<Self, CacheError> {
        let path = checked_path(path)?;
        for other in reserved {
            if other == Path::new(":memory:") {
                continue;
            }
            let other = checked_path(&std::path::absolute(other)?)?;
            if path.starts_with(&other) || other.starts_with(&path) {
                return Err(CacheError::OutOfScope);
            }
        }
        ensure_directory(&path, None)?;
        let identity = identity(&fs::symlink_metadata(&path)?);
        let lock = open_lock(&path.join(LOCK), identity)?;
        lock.try_lock().map_err(|_| CacheError::Storage)?;
        let mut root = Self {
            path,
            identity,
            _lock: lock,
            was_unclean: false,
            restore_epoch: AtomicU64::new(0),
        };
        let usage = root.usage()?;
        let manifest = root.manifest()?;
        if manifest.is_none() && !usage.partitions.is_empty() {
            return Err(CacheError::Corrupt);
        }
        if let Some(state) = &manifest {
            root.was_unclean = !state.clean;
            root.restore_epoch
                .store(state.restore_epoch, Ordering::Relaxed);
        }
        for partition in usage.partitions.keys() {
            if manifest
                .as_ref()
                .is_some_and(|state| state.partitions.binary_search(partition).is_ok())
            {
                continue;
            }
            // New partitions are registered before any file is created. Only
            // an empty directory can be left by an interrupted registration.
            if fs::read_dir(root.path.join(partition))?.next().is_some() {
                return Err(CacheError::Corrupt);
            }
            root.was_unclean = true;
        }
        let stage = root.path.join(STAGE);
        if stage.exists() {
            fs::remove_file(stage)?;
        }
        root.write_manifest(false, root.restore_epoch(), limit)?;
        Ok(root)
    }

    pub(super) fn was_unclean(&self) -> bool {
        self.was_unclean
    }

    pub(super) fn prepare_partition(&self, name: &str, limit: u64) -> Result<PathBuf, CacheError> {
        let usage = self.usage()?;
        let partition = self.path.join(name);
        if !usage.partitions.contains_key(name)
            && (usage.partitions.len() == MAX_PARTITIONS
                || usage.total.saturating_add(ROOT_WRITE_RESERVE) > limit)
        {
            return Err(CacheError::Quota);
        }
        ensure_directory(&partition, Some(self.identity))?;
        self.write_manifest(false, self.restore_epoch(), limit)?;
        let path = partition.join(DB);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => validate_file(&metadata, self.identity)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                private_file_options().create_new(true).open(&path)?;
                File::open(&partition)?.sync_all()?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(path)
    }

    pub(super) fn mark_clean(&self, limit: u64) -> Result<(), CacheError> {
        self.write_manifest(true, self.restore_epoch(), limit)
    }

    pub(super) fn restore_epoch(&self) -> u64 {
        self.restore_epoch.load(Ordering::Relaxed)
    }

    pub(super) fn mark_restored(&self, limit: u64) -> Result<u64, CacheError> {
        let restore_epoch = self
            .restore_epoch()
            .checked_add(1)
            .filter(|value| *value <= i64::MAX as u64)
            .ok_or(CacheError::Corrupt)?;
        self.write_manifest(false, restore_epoch, limit)?;
        self.restore_epoch.store(restore_epoch, Ordering::Relaxed);
        Ok(restore_epoch)
    }

    pub(super) fn reserve_write(
        &self,
        db: &Connection,
        name: &str,
        account_limit: u64,
        global_limit: u64,
    ) -> Result<(), CacheError> {
        let (total, own) = self.bytes(name)?;
        let current =
            u64::from(db.pragma_query_value(None, "page_count", |row| row.get::<_, u32>(0))?);
        let overhead = own.saturating_sub(current * PAGE_BYTES);
        let limit = account_limit
            .min(
                global_limit
                    .saturating_sub(total.saturating_sub(own))
                    .saturating_sub(ROOT_WRITE_RESERVE),
            )
            .saturating_sub(overhead);
        let reserve = JOURNAL_HEADER_RESERVE
            .saturating_add(current.saturating_mul(PAGE_BYTES + 8))
            .saturating_add(2 * PAGE_BYTES);
        let max_pages = (limit.saturating_sub(reserve) / PAGE_BYTES)
            // Leave room for the next transaction to journal the committed
            // database even if all of its pages are modified.
            .min(
                limit.saturating_sub(JOURNAL_HEADER_RESERVE + 2 * PAGE_BYTES)
                    / (2 * PAGE_BYTES + 8),
            )
            .min(u64::from(u32::MAX));
        if max_pages == 0 || max_pages < current || own > account_limit {
            return Err(CacheError::Quota);
        }
        let set: u32 =
            db.pragma_update_and_check(None, "max_page_count", max_pages as u32, |row| row.get(0))?;
        if u64::from(set) > max_pages {
            return Err(CacheError::Quota);
        }
        Ok(())
    }

    pub(super) fn bytes(&self, name: &str) -> Result<(u64, u64), CacheError> {
        let usage = self.usage()?;
        let own = usage.partitions.get(name).ok_or(CacheError::Corrupt)?;
        Ok((usage.total, *own))
    }

    pub(super) fn verify_usage(
        &self,
        name: &str,
        account_limit: u64,
        global_limit: u64,
    ) -> Result<(), CacheError> {
        let (total, own) = self.bytes(name)?;
        if own > account_limit || total > global_limit {
            return Err(CacheError::Quota);
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), CacheError> {
        checked_path(&self.path)?;
        let metadata = fs::symlink_metadata(&self.path)?;
        if !private_directory(&metadata) || identity(&metadata) != self.identity {
            return Err(CacheError::Storage);
        }
        Ok(())
    }

    fn usage(&self) -> Result<Usage, CacheError> {
        self.validate()?;
        let mut total = allocated(&fs::symlink_metadata(&self.path)?);
        let mut partitions = BTreeMap::new();
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| CacheError::Corrupt)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            match name.as_str() {
                LOCK | MANIFEST | STAGE => {
                    validate_file(&metadata, self.identity)?;
                    total = total.saturating_add(allocated(&metadata));
                }
                name if is_namespace(name) && partitions.len() < MAX_PARTITIONS => {
                    if !private_directory(&metadata)
                        || identity(&metadata).owner != self.identity.owner
                    {
                        return Err(CacheError::Storage);
                    }
                    let mut bytes = allocated(&metadata);
                    for file in fs::read_dir(entry.path())? {
                        let file = file?;
                        if !matches!(
                            file.file_name().to_str(),
                            Some(
                                DB | "cache.sqlite-journal"
                                    | "cache.sqlite-wal"
                                    | "cache.sqlite-shm"
                            )
                        ) {
                            return Err(CacheError::Corrupt);
                        }
                        let metadata = fs::symlink_metadata(file.path())?;
                        validate_file(&metadata, self.identity)?;
                        bytes = bytes.saturating_add(allocated(&metadata));
                    }
                    total = total.saturating_add(bytes);
                    partitions.insert(name.to_owned(), bytes);
                }
                _ => return Err(CacheError::Corrupt),
            }
        }
        Ok(Usage { total, partitions })
    }

    fn manifest(&self) -> Result<Option<Manifest>, CacheError> {
        let file = match File::open(self.path.join(MANIFEST)) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MANIFEST_BYTES {
            return Err(CacheError::Corrupt);
        }
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| CacheError::Corrupt)?;
        if manifest.version != 1
            || manifest.restore_epoch > i64::MAX as u64
            || manifest.partitions.len() > MAX_PARTITIONS
            || !manifest.partitions.iter().all(|name| is_namespace(name))
            || !manifest
                .partitions
                .windows(2)
                .all(|names| names[0] < names[1])
        {
            return Err(CacheError::Corrupt);
        }
        Ok(Some(manifest))
    }

    fn write_manifest(
        &self,
        clean: bool,
        restore_epoch: u64,
        limit: u64,
    ) -> Result<(), CacheError> {
        let usage = self.usage()?;
        let manifest = Manifest {
            version: 1,
            clean,
            restore_epoch,
            partitions: usage.partitions.into_keys().collect(),
        };
        let bytes = serde_json::to_vec(&manifest).map_err(|_| CacheError::Storage)?;
        if usage
            .total
            .saturating_add((bytes.len() as u64).next_multiple_of(PAGE_BYTES))
            > limit
        {
            return Err(CacheError::Quota);
        }
        let stage = self.path.join(STAGE);
        let mut file = private_file_options().create_new(true).open(&stage)?;
        let result = (|| -> Result<(), CacheError> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&stage, self.path.join(MANIFEST))?;
            File::open(&self.path)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(stage);
        }
        result
    }
}

pub(super) fn namespace_name(namespace: &Namespace) -> String {
    use std::fmt::Write as _;
    let mut name = String::with_capacity(64);
    for byte in namespace {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

fn is_namespace(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn checked_path(path: &Path) -> Result<PathBuf, CacheError> {
    if !path.is_absolute() || path.parent().is_none() {
        return Err(CacheError::OutOfScope);
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err(CacheError::OutOfScope);
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(CacheError::Storage),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    owner: u32,
}

#[cfg(unix)]
fn identity(metadata: &Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
        owner: metadata.uid(),
    }
}

#[cfg(not(unix))]
fn identity(_: &Metadata) -> Identity {
    Identity {
        device: 0,
        inode: 0,
        owner: 0,
    }
}

#[cfg(unix)]
fn allocated(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.len().max(metadata.blocks().saturating_mul(512))
}

#[cfg(not(unix))]
fn allocated(metadata: &Metadata) -> u64 {
    metadata.len()
}

#[cfg(unix)]
fn private_directory(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_dir() && metadata.mode() & 0o7777 == 0o700
}

#[cfg(not(unix))]
fn private_directory(_: &Metadata) -> bool {
    false
}

fn ensure_directory(path: &Path, parent: Option<Identity>) -> Result<(), CacheError> {
    if !cfg!(unix) {
        return Err(CacheError::Storage);
    }
    if let Err(error) = fs::symlink_metadata(path) {
        if error.kind() != ErrorKind::NotFound {
            return Err(error.into());
        }
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    if !private_directory(&metadata)
        || parent.is_some_and(|parent| identity(&metadata).owner != parent.owner)
    {
        return Err(CacheError::Storage);
    }
    Ok(())
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

fn open_lock(path: &Path, parent: Identity) -> Result<File, CacheError> {
    let lock = match private_file_options().create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            validate_file(&metadata, parent)?;
            let file = private_file_options().open(path)?;
            if identity(&file.metadata()?) != identity(&metadata) {
                return Err(CacheError::Storage);
            }
            file
        }
        Err(error) => return Err(error.into()),
    };
    validate_file(&lock.metadata()?, parent)?;
    Ok(lock)
}

fn validate_file(metadata: &Metadata, parent: Identity) -> Result<(), CacheError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.is_file()
            && metadata.mode() & 0o7777 == 0o600
            && metadata.nlink() == 1
            && metadata.uid() == parent.owner
        {
            return Ok(());
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, parent);
    Err(CacheError::Storage)
}
