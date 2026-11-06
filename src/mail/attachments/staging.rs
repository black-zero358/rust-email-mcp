//! Private, bounded attachment files. Blocking jobs own the file and permits
//! until I/O completes, including when their async caller has been cancelled.
use crate::{config::Config, mail::parts::fetch::CompletionEvidence};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, DirBuilder, File, Metadata, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

pub const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_LIVE_FILES: usize = 4;
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
const MAX_RECOVERY_ENTRIES: usize = 256;
const LOCK_NAME: &str = ".attachment-lifecycle-v1.lock";
const PREFIX: &str = "attachment-v1-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotConfigured,
    UnsupportedPlatform,
    UnsafeRoot,
    UnsafeEntry,
    InUse,
    RecoveryEntryLimit,
    Capacity,
    Poisoned,
    SizeLimit,
    SizeMismatch,
    ChecksumMismatch,
    CompletionMismatch,
    Closed,
    InvalidChunk,
    WorkerFailed,
    Io(io::ErrorKind),
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "Attachment staging failed: {self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageId(Uuid);

pub struct Store {
    root: Option<Arc<StagingRoot>>,
}
impl Store {
    /// Called once by the instance's shared AttachmentStore. Filesystem failures
    /// disable authenticated downloads without changing account/journal startup.
    pub fn from_config(config: &Config) -> Self {
        Self {
            root: configured_root(config)
                .and_then(|path| StagingRoot::open(&path))
                .ok(),
        }
    }
    pub fn is_available(&self) -> bool {
        self.root.as_ref().is_some_and(|root| !root.is_poisoned())
    }
    pub async fn begin(&self, transfer: OwnedSemaphorePermit) -> Result<Stage> {
        let root = self.root.as_ref().ok_or(Error::NotConfigured)?.clone();
        blocking(move || root.begin_sync(transfer)).await
    }
}

fn configured_root(config: &Config) -> Result<PathBuf> {
    let requested = if let Some(explicit) = &config.attachment_staging_dir {
        explicit.clone()
    } else if config.database == ":memory:" {
        return Err(Error::NotConfigured);
    } else {
        let database = absolute_without_parent(Path::new(&config.database))?;
        let parent = database.parent().ok_or(Error::UnsafeRoot)?;
        validate_components(parent)?; // Reject symlinks before canonicalization.
        fs::canonicalize(parent)?.join("mail-downloads")
    };
    absolute_without_parent(&requested)
}
fn absolute_without_parent(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(Error::UnsafeRoot);
    }
    let mut absolute = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir()?
    };
    for part in path.components() {
        if !matches!(part, Component::CurDir) {
            absolute.push(part);
        }
    }
    Ok(absolute)
}
async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| Error::WorkerFailed)?
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ExpectedFile {
    /// Set only for a genuinely exact decoded size, never an estimate.
    pub decoded_size: Option<usize>,
    pub sha256: Option<[u8; 32]>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadyMetadata {
    pub decoded_size: usize,
    pub sha256: [u8; 32],
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub removed_files: usize,
}

pub struct StagingRoot {
    path: PathBuf,
    identity: (u64, u64),
    owner: u32,
    _lock: File,
    active: AtomicUsize,
    poisoned: AtomicBool,
    recovery: RecoveryReport,
    #[cfg(test)]
    gate: std::sync::Mutex<Option<(tests::IoPhase, Arc<tests::IoGate>)>>,
}
impl StagingRoot {
    /// The caller chooses an absolute, dedicated private root. No environment
    /// variable, default directory, user filename or external path is accepted.
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        if !cfg!(unix) {
            return Err(Error::UnsupportedPlatform);
        }
        validate_components(path)?;
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(path)?;
            }
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || !private_mode(&metadata, 0o700)
        {
            return Err(Error::UnsafeRoot);
        }
        let canonical = fs::canonicalize(path)?;
        if canonical != path {
            return Err(Error::UnsafeRoot);
        }
        let lock_path = path.join(LOCK_NAME);
        let before = match fs::symlink_metadata(&lock_path) {
            Ok(metadata) => {
                if !private_file(&metadata, owner(&metadata_root(path)?)) {
                    return Err(Error::UnsafeEntry);
                }
                Some(identity(&metadata))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let lock = private_options().create(true).open(&lock_path)?;
        let lock_metadata = lock.metadata()?;
        if !private_file(&lock_metadata, owner(&metadata))
            || before.is_some_and(|before| before != identity(&lock_metadata))
        {
            return Err(Error::UnsafeEntry);
        }
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(Error::InUse),
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let mut root = Self {
            path: canonical,
            identity: identity(&metadata),
            owner: owner(&metadata),
            _lock: lock,
            active: AtomicUsize::new(0),
            poisoned: AtomicBool::new(false),
            recovery: RecoveryReport::default(),
            #[cfg(test)]
            gate: Default::default(),
        };
        root.validate()?;
        root.recovery = root.recover()?;
        Ok(Arc::new(root))
    }
    #[cfg(test)]
    pub fn recovery_report(&self) -> RecoveryReport {
        self.recovery
    }
    #[cfg(test)]
    pub fn live_files(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }
    fn validate(&self) -> Result<()> {
        validate_components(&self.path)?;
        let metadata = metadata_root(&self.path)?;
        if identity(&metadata) != self.identity || owner(&metadata) != self.owner {
            return Err(Error::UnsafeRoot);
        }
        Ok(())
    }
    fn recover(&self) -> Result<RecoveryReport> {
        let mut report = RecoveryReport::default();
        for (index, entry) in fs::read_dir(&self.path)?.enumerate() {
            if index >= MAX_RECOVERY_ENTRIES {
                return Err(Error::RecoveryEntryLimit);
            }
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(Error::UnsafeEntry);
            }
            if recognized(&entry.file_name()) {
                let metadata = entry.metadata()?;
                if !private_file(&metadata, self.owner) {
                    return Err(Error::UnsafeEntry);
                }
                fs::remove_file(entry.path())?;
                report.removed_files += 1;
            }
        }
        Ok(report)
    }
    fn begin_sync(self: &Arc<Self>, transfer: OwnedSemaphorePermit) -> Result<Stage> {
        #[cfg(test)]
        self.io_gate(tests::IoPhase::Begin);
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        self.validate()?;
        self.active
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < MAX_LIVE_FILES).then_some(count + 1)
            })
            .map_err(|_| Error::Capacity)?;
        let lease = Slot {
            root: self.clone(),
            release: true,
        };
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let id = Uuid::new_v4();
        let path = self.path.join(format!("{PREFIX}{}.part", id.simple()));
        let file = private_options().create_new(true).open(&path)?;
        let owned = OwnedFile {
            file: Some(file),
            paths: [Some(path), None],
            slot: lease,
            _transfer: transfer,
        };
        if !private_file(
            &owned.file.as_ref().ok_or(Error::Closed)?.metadata()?,
            self.owner,
        ) {
            return Err(Error::UnsafeEntry);
        }
        Ok(Stage {
            owned: Some(owned),
            id: StageId(id),
            bytes: 0,
            hash: Sha256::new(),
            capture: None,
        })
    }
}

fn metadata_root(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || !private_mode(&metadata, 0o700) {
        return Err(Error::UnsafeRoot);
    }
    Ok(metadata)
}
fn validate_components(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::UnsafeRoot);
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if !matches!(
            component,
            Component::RootDir | Component::Prefix(_) | Component::Normal(_)
        ) {
            return Err(Error::UnsafeRoot);
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(Error::UnsafeRoot),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && current == path => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn private_mode(metadata: &Metadata, expected: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777 == expected
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, expected);
        false
    }
}
fn identity(metadata: &Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        (0, 0)
    }
}
fn owner(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.uid()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}
fn private_file(metadata: &Metadata, uid: u32) -> bool {
    metadata.is_file()
        && !metadata.file_type().is_symlink()
        && private_mode(metadata, 0o600)
        && owner(metadata) == uid
}
fn recognized(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(name) = name.strip_prefix(PREFIX) else {
        return false;
    };
    let Some(id) = name
        .strip_suffix(".part")
        .or_else(|| name.strip_suffix(".ready"))
    else {
        return false;
    };
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

struct Slot {
    root: Arc<StagingRoot>,
    release: bool,
}
impl Drop for Slot {
    fn drop(&mut self) {
        if self.release {
            self.root.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
struct OwnedFile {
    file: Option<File>,
    paths: [Option<PathBuf>; 2],
    slot: Slot,
    _transfer: OwnedSemaphorePermit,
}
impl Drop for OwnedFile {
    fn drop(&mut self) {
        drop(self.file.take());
        let mut clean = self.slot.root.validate().is_ok();
        if clean {
            for path in self.paths.iter().flatten() {
                if let Err(error) = fs::remove_file(path) {
                    clean &= error.kind() == io::ErrorKind::NotFound;
                }
            }
        }
        if !clean {
            // Failed unlink keeps its logical quota reserved and stops new
            // files. A restart under the exclusive lifecycle lock recovers it.
            self.slot.release = false;
            self.slot.root.poisoned.store(true, Ordering::SeqCst);
        }
    }
}

pub struct Stage {
    owned: Option<OwnedFile>,
    id: StageId,
    bytes: usize,
    hash: Sha256,
    capture: Option<Capture>,
}

pub(in crate::mail) struct Capture {
    pub bytes: Vec<u8>,
    pub _permit: super::super::persistence::ContentAdmission,
}

impl Stage {
    pub fn id(&self) -> StageId {
        self.id
    }
    pub(in crate::mail) fn capture(
        &mut self,
        limit: usize,
        permit: super::super::persistence::ContentAdmission,
    ) {
        self.capture = Some(Capture {
            bytes: Vec::with_capacity(limit.min(MAX_FILE_BYTES)),
            _permit: permit,
        });
    }
    pub async fn write_chunk(mut self, chunk: Vec<u8>) -> Result<Self> {
        if chunk.len() > MAX_CHUNK_BYTES {
            return Err(Error::InvalidChunk);
        }
        blocking(move || {
            #[cfg(test)]
            self.owned
                .as_ref()
                .ok_or(Error::Closed)?
                .slot
                .root
                .io_gate(tests::IoPhase::Write);
            self.write_checked(&chunk)?;
            Ok(self)
        })
        .await
    }
    fn write_checked(&mut self, chunk: &[u8]) -> Result<()> {
        let owned = self.owned.as_mut().ok_or(Error::Closed)?;
        if owned.slot.root.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let next = self
            .bytes
            .checked_add(chunk.len())
            .filter(|size| *size <= MAX_FILE_BYTES)
            .ok_or(Error::SizeLimit)?;
        owned.file.as_mut().ok_or(Error::Closed)?.write_all(chunk)?;
        self.hash.update(chunk);
        self.bytes = next;
        if let Some(capture) = &mut self.capture {
            if next <= capture.bytes.capacity() {
                capture.bytes.extend_from_slice(chunk);
            } else {
                self.capture = None;
            }
        }
        Ok(())
    }
    pub async fn finish(
        self,
        evidence: CompletionEvidence,
        expected: ExpectedFile,
    ) -> Result<Ready> {
        blocking(move || self.finish_sync(evidence, expected)).await
    }
    fn finish_sync(
        mut self,
        evidence: CompletionEvidence,
        expected: ExpectedFile,
    ) -> Result<Ready> {
        #[cfg(test)]
        self.owned
            .as_ref()
            .ok_or(Error::Closed)?
            .slot
            .root
            .io_gate(tests::IoPhase::Finish);
        if !evidence.matches(self.id) {
            return Err(Error::CompletionMismatch);
        }
        if expected.decoded_size.is_some_and(|size| size != self.bytes) {
            return Err(Error::SizeMismatch);
        }
        let digest: [u8; 32] = self.hash.clone().finalize().into();
        if expected.sha256.is_some_and(|hash| hash != digest) {
            return Err(Error::ChecksumMismatch);
        }
        let mut owned = self.owned.take().ok_or(Error::Closed)?;
        if owned.slot.root.is_poisoned() {
            return Err(Error::Poisoned);
        }
        owned.slot.root.validate()?;
        let file = owned.file.as_mut().ok_or(Error::Closed)?;
        file.sync_all()?;
        let ready = owned
            .slot
            .root
            .path
            .join(format!("{PREFIX}{}.ready", self.id.0.simple()));
        let part = owned.paths[0].as_ref().ok_or(Error::Closed)?.clone();
        if let Err(error) = fs::hard_link(&part, &ready) {
            if error.kind() == io::ErrorKind::AlreadyExists {
                owned.slot.root.poisoned.store(true, Ordering::SeqCst);
            }
            return Err(error.into());
        }
        owned.paths[1] = Some(ready);
        fs::remove_file(&part)?;
        owned.paths[0] = None;
        File::open(&owned.slot.root.path)?.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Ready {
            owned,
            capture: self.capture.take(),
            metadata: ReadyMetadata {
                decoded_size: self.bytes,
                sha256: digest,
            },
        })
    }
}
pub struct Ready {
    owned: OwnedFile,
    metadata: ReadyMetadata,
    capture: Option<Capture>,
}
impl Ready {
    pub(in crate::mail) fn take_capture(&mut self) -> Option<Capture> {
        self.capture.take()
    }
    pub fn metadata(&self) -> ReadyMetadata {
        self.metadata
    }
    pub async fn read_chunk(mut self) -> Result<(Self, Vec<u8>)> {
        blocking(move || {
            #[cfg(test)]
            self.owned.slot.root.io_gate(tests::IoPhase::Read);
            let mut chunk = vec![0; MAX_CHUNK_BYTES];
            let read = self
                .owned
                .file
                .as_mut()
                .ok_or(Error::Closed)?
                .read(&mut chunk)?;
            chunk.truncate(read);
            Ok((self, chunk))
        })
        .await
    }
}

#[cfg(test)]
#[path = "staging/tests.rs"]
mod tests;
