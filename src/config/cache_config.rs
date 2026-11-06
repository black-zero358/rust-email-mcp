//! Explicit cache configuration, validated before opening any database.
use super::ValidatedAccountSet;
use crate::{
    accounts::valid_account_id,
    cache::{MAX_BYTES, MAX_FOLDERS_PER_ACCOUNT, MIN_BYTES},
    imap_text::mailbox_name,
    input_limits::InputLimits,
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Component, Path, PathBuf},
};

const CONFIG_ERROR: &str = "Cache configuration is invalid. Check its mode, paths, accounts, folders, byte quotas and synchronization policy.";
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Metadata,
    Full,
}

impl Mode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Full => "full",
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Document {
    Off {
        version: u32,
    },
    Metadata {
        version: u32,
        directory: PathBuf,
        max_bytes: u64,
        accounts: Vec<AccountSettings>,
    },
    Full {
        version: u32,
        directory: PathBuf,
        max_bytes: u64,
        accounts: Vec<AccountSettings>,
        #[serde(default)]
        sync: SyncPolicy,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyncPolicy {
    pub(crate) enabled: bool,
    #[serde(default = "sync_interval")]
    pub(crate) interval_seconds: u64,
    #[serde(default = "reconcile_interval")]
    pub(crate) reconcile_interval_seconds: u64,
    #[serde(default)]
    pub(crate) protocol: SyncProtocol,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncProtocol {
    #[default]
    Rev1,
    Auto,
}

impl Default for SyncPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: sync_interval(),
            reconcile_interval_seconds: reconcile_interval(),
            protocol: SyncProtocol::Rev1,
        }
    }
}

const fn sync_interval() -> u64 {
    60
}

const fn reconcile_interval() -> u64 {
    900
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountSettings {
    pub(crate) account_id: String,
    pub(crate) mailboxes: Vec<String>,
    pub(crate) max_bytes: u64,
}

pub struct CacheSettings {
    pub(crate) mode: Mode,
    pub(crate) directory: PathBuf,
    pub(crate) max_bytes: u64,
    pub(crate) accounts: Vec<AccountSettings>,
    pub(crate) reserved: Vec<PathBuf>,
    pub(crate) sync: SyncPolicy,
}

pub fn load_cache_configuration(
    accounts: &ValidatedAccountSet,
) -> anyhow::Result<Option<CacheSettings>> {
    let path = std::env::var_os("CACHE_CONFIG_PATH");
    match path {
        Some(path) => load_cache_file(Path::new(&path), accounts),
        None => Ok(None),
    }
}

pub(crate) fn load_cache_file(
    path: &Path,
    registered: &ValidatedAccountSet,
) -> anyhow::Result<Option<CacheSettings>> {
    load(path, registered).map_err(|_| anyhow::anyhow!(CONFIG_ERROR))
}

fn load(path: &Path, registered: &ValidatedAccountSet) -> anyhow::Result<Option<CacheSettings>> {
    anyhow::ensure!(fs::metadata(path)?.is_file());
    let file = File::open(path)?;
    anyhow::ensure!(file.metadata()?.is_file());
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= MAX_CONFIG_BYTES);
    let (mode, version, directory, max_bytes, mut accounts, sync) =
        match serde_json::from_slice(&bytes)? {
            Document::Off { version } => {
                anyhow::ensure!(version == 1);
                return Ok(None);
            }
            Document::Full {
                version,
                directory,
                max_bytes,
                accounts,
                sync,
            } => (Mode::Full, version, directory, max_bytes, accounts, sync),
            Document::Metadata {
                version,
                directory,
                max_bytes,
                accounts,
            } => (
                Mode::Metadata,
                version,
                directory,
                max_bytes,
                accounts,
                SyncPolicy::default(),
            ),
        };
    anyhow::ensure!(version == 1 && !directory.as_os_str().is_empty());
    anyhow::ensure!((MIN_BYTES..=MAX_BYTES).contains(&max_bytes));
    anyhow::ensure!((1..=86_400).contains(&sync.interval_seconds));
    anyhow::ensure!((1..=86_400).contains(&sync.reconcile_interval_seconds));
    anyhow::ensure!(!accounts.is_empty() && accounts.len() <= registered.accounts.len());

    let mut selected = HashSet::with_capacity(accounts.len());
    for scope in &mut accounts {
        anyhow::ensure!(
            valid_account_id(&scope.account_id)
                && registered
                    .accounts
                    .iter()
                    .any(|account| account.alias == scope.account_id)
                && selected.insert(scope.account_id.as_str())
        );
        anyhow::ensure!((MIN_BYTES..=max_bytes).contains(&scope.max_bytes));
        anyhow::ensure!((1..=MAX_FOLDERS_PER_ACCOUNT).contains(&scope.mailboxes.len()));
        let mut mailboxes = HashSet::with_capacity(scope.mailboxes.len());
        for mailbox in &mut scope.mailboxes {
            anyhow::ensure!(!mailbox.is_empty() && !mailbox.chars().any(char::is_control));
            mailbox_name(mailbox, InputLimits::Read)?;
            if mailbox.eq_ignore_ascii_case("INBOX") {
                *mailbox = "INBOX".into();
            }
            anyhow::ensure!(mailboxes.insert(mailbox.as_str()));
        }
    }
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let directory = normalize(&parent.join(directory), true)?;
    anyhow::ensure!(directory.parent().is_some());
    if directory.exists() {
        anyhow::ensure!(directory.is_dir());
    }

    let service = &registered.service_config;
    let operation = (service.database != ":memory:")
        .then(|| normalize(Path::new(&service.database), false))
        .transpose()?;
    let database_parent = operation.as_deref().and_then(Path::parent);
    let registry = match &service.account_registry {
        Some(path) => normalize(path, false)?,
        None => normalize(
            &database_parent
                .unwrap_or_else(|| Path::new("data"))
                .join("accounts.sqlite"),
            false,
        )?,
    };
    let staging = match &service.attachment_staging_dir {
        Some(path) if !path.as_os_str().is_empty() => Some(normalize(path, false)?),
        Some(_) => None,
        None => database_parent.map(|parent| parent.join("mail-downloads")),
    };
    let mut reserved = Vec::new();
    if let Some(operation) = &operation {
        reserve_paths(&mut reserved, operation, &["", "-journal", "-wal", "-shm"])?;
    }
    reserve_paths(
        &mut reserved,
        &registry,
        &[
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
            ".cache-run",
            ".cache-run.lock",
            ".cache-run.stage",
        ],
    )?;
    reserved.extend(staging);
    anyhow::ensure!(
        !reserved
            .iter()
            .any(|path| { directory.starts_with(path) || path.starts_with(&directory) })
    );
    Ok(Some(CacheSettings {
        mode,
        directory,
        max_bytes,
        accounts,
        reserved,
        sync,
    }))
}

fn reserve_paths(paths: &mut Vec<PathBuf>, base: &Path, suffixes: &[&str]) -> anyhow::Result<()> {
    for suffix in suffixes {
        let mut path = base.as_os_str().to_owned();
        path.push(suffix);
        paths.push(normalize(Path::new(&path), false)?);
    }
    Ok(())
}

/// Canonicalize each existing ancestor without creating missing paths. Cache
/// locations reject symlinks, and protected locations resolve them for overlap.
fn normalize(path: &Path, reject_symlinks: bool) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(!path.as_os_str().is_empty());
    let absolute = std::path::absolute(path)?;
    let mut result = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => anyhow::bail!(CONFIG_ERROR),
            Component::CurDir => continue,
            part => result.push(part),
        }
        if matches!(part, Component::Normal(_)) {
            match fs::symlink_metadata(&result) {
                Ok(metadata) => {
                    anyhow::ensure!(!reject_symlinks || !metadata.file_type().is_symlink());
                    result = fs::canonicalize(&result)?;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn synchronization_is_explicit_full_only_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cache.json");
        let registered = ValidatedAccountSet::legacy(crate::config::fixture());
        let mut document = json!({
            "version": 1, "mode": "full", "directory": "cache",
            "max_bytes": 8 * MIN_BYTES,
            "accounts": [{"account_id": "default", "mailboxes": ["INBOX"], "max_bytes": 4 * MIN_BYTES}]
        });
        let load = |value: &serde_json::Value| {
            fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
            load_cache_file(&path, &registered)
        };
        assert!(!load(&document).unwrap().unwrap().sync.enabled);
        document["sync"] = json!({"enabled": true});
        assert_eq!(
            load(&document).unwrap().unwrap().sync.protocol,
            SyncProtocol::Rev1
        );

        document["sync"]["protocol"] = json!("auto");
        assert_eq!(
            load(&document).unwrap().unwrap().sync.protocol,
            SyncProtocol::Auto
        );
        document["sync"]["interval_seconds"] = json!(0);
        assert!(load(&document).is_err());

        document["sync"] = json!({"enabled": true});
        document["mode"] = json!("metadata");
        assert!(load(&document).is_err());
        assert!(!root.path().join("cache").exists());
    }
}
