//! Bounded administrator configuration; credentials never enter serialized views.
use super::{Config, MailValues, lookup_string};
use crate::accounts::{DEFAULT_ACCOUNT_ID, LEGACY_REGISTRATION_KEY, valid_account_id};
use serde::Deserialize;
use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const MAX_ACCOUNT_CONFIG_BYTES: usize = 128 * 1024;
pub const MAX_ACCOUNTS: usize = 16;
const MAX_SECRET_BYTES: usize = 4096;
const CONFIG_ERROR: &str = "Account configuration is invalid. Check its version, fields, limits, identities and explicit access lists.";

#[cfg(test)]
#[path = "account_config/tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountConfigSource {
    Legacy,
    ExtendLegacy,
    FileOnly,
}

#[derive(Clone)]
pub struct ValidatedAccountConfig {
    pub registration_key: String,
    pub alias: String,
    pub display_name: String,
    pub config: Arc<Config>,
    pub read_subjects: Vec<String>,
    pub write_subjects: Vec<String>,
    pub is_legacy: bool,
}
#[derive(Clone)]
pub struct ValidatedAccountSet {
    pub service_config: Arc<Config>,
    pub default_read_account: String,
    pub accounts: Vec<ValidatedAccountConfig>,
    pub source: AccountConfigSource,
}
impl ValidatedAccountSet {
    pub fn legacy(config: Arc<Config>) -> Self {
        Self {
            service_config: config.clone(),
            default_read_account: DEFAULT_ACCOUNT_ID.into(),
            source: AccountConfigSource::Legacy,
            accounts: vec![ValidatedAccountConfig {
                registration_key: LEGACY_REGISTRATION_KEY.into(),
                alias: DEFAULT_ACCOUNT_ID.into(),
                display_name: "Default".into(),
                read_subjects: config.subjects.clone(),
                write_subjects: config.subjects.clone(),
                config,
                is_legacy: true,
            }],
        }
    }
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.accounts.is_empty() && self.accounts.len() <= MAX_ACCOUNTS,
            CONFIG_ERROR
        );
        let mut keys = HashSet::new();
        let mut aliases = HashSet::new();
        let mut legacy = 0;
        if self.source != AccountConfigSource::Legacy {
            for value in [&self.service_config.resource, &self.service_config.issuer] {
                let url = reqwest::Url::parse(value)?;
                anyhow::ensure!(
                    url.scheme() == "https" && url.host_str().is_some(),
                    CONFIG_ERROR
                );
            }
        }
        for account in &self.accounts {
            super::validate_archive(account.config.archive.as_deref())
                .map_err(|_| anyhow::anyhow!(CONFIG_ERROR))?;
            anyhow::ensure!(
                valid_account_id(&account.registration_key)
                    && valid_account_id(&account.alias)
                    && keys.insert(&account.registration_key)
                    && aliases.insert(&account.alias),
                CONFIG_ERROR
            );
            if self.source != AccountConfigSource::Legacy {
                check_text(&account.display_name, 128, 512)?;
                check_text(&account.config.address, 512, 2048)?;
                check_text(&account.config.imap_host, 253, 253)?;
                tokio_rustls::rustls::pki_types::ServerName::try_from(
                    account.config.imap_host.clone(),
                )
                .map_err(|_| anyhow::anyhow!(CONFIG_ERROR))?;
                anyhow::ensure!(
                    account.config.imap_port != 0
                        && !account.config.password.is_empty()
                        && account.config.password.len() <= MAX_SECRET_BYTES,
                    CONFIG_ERROR
                );
                check_acl(&account.read_subjects)?;
                check_acl(&account.write_subjects)?;
            }
            if account.is_legacy {
                legacy += 1;
                anyhow::ensure!(
                    account.registration_key == LEGACY_REGISTRATION_KEY
                        && account.alias == DEFAULT_ACCOUNT_ID
                        && account
                            .write_subjects
                            .iter()
                            .all(|subject| account.read_subjects.contains(subject)),
                    CONFIG_ERROR
                );
                anyhow::ensure!(
                    same_legacy_mail(&account.config, &self.service_config),
                    CONFIG_ERROR
                );
                if self.source != AccountConfigSource::Legacy
                    && !account.config.smtp_host.is_empty()
                {
                    check_text(&account.config.smtp_host, 253, 253)?;
                    anyhow::ensure!(account.config.smtp_port != 0, CONFIG_ERROR);
                    tokio_rustls::rustls::pki_types::ServerName::try_from(
                        account.config.smtp_host.clone(),
                    )
                    .map_err(|_| anyhow::anyhow!(CONFIG_ERROR))?;
                    lettre::transport::smtp::client::TlsParameters::new(
                        account.config.smtp_host.clone(),
                    )
                    .map_err(|_| anyhow::anyhow!(CONFIG_ERROR))?;
                }
            } else {
                anyhow::ensure!(
                    account.registration_key != LEGACY_REGISTRATION_KEY
                        && account.alias != DEFAULT_ACCOUNT_ID
                        && account.config.mode == "read"
                        && account.write_subjects.is_empty(),
                    CONFIG_ERROR
                );
            }
        }
        anyhow::ensure!(
            legacy == 1
                && valid_account_id(&self.default_read_account)
                && aliases.contains(&self.default_read_account),
            CONFIG_ERROR
        );
        Ok(())
    }
}

fn same_legacy_mail(a: &Config, b: &Config) -> bool {
    a.address == b.address
        && a.imap_host.eq_ignore_ascii_case(&b.imap_host)
        && a.imap_port == b.imap_port
        && a.database == b.database
        && a.mode == b.mode
}
fn check_text(value: &str, characters: usize, bytes: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.chars().count() <= characters
            && value.len() <= bytes
            && !value.chars().any(char::is_control),
        CONFIG_ERROR
    );
    Ok(())
}
fn check_acl(subjects: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(subjects.len() <= 64, CONFIG_ERROR);
    let mut seen = HashSet::new();
    for subject in subjects {
        check_text(subject, 512, 2048)?;
        anyhow::ensure!(seen.insert(subject), CONFIG_ERROR);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Document {
    ExtendLegacy {
        version: u32,
        default_account_id: String,
        legacy_default: LegacyOverlay,
        accounts: Vec<ExtraAccount>,
    },
    FileOnly {
        version: u32,
        default_account_id: String,
        legacy_default: Box<FileLegacy>,
        accounts: Vec<ExtraAccount>,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyOverlay {
    display_name: String,
    read_subjects: Vec<String>,
    write_subjects: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Imap {
    host: String,
    #[serde(default = "imap_port")]
    port: u16,
}
fn imap_port() -> u16 {
    993
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Smtp {
    host: String,
    port: u16,
    starttls: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Folders {
    drafts: String,
    trash: String,
    archive: Option<String>,
}
impl Default for Folders {
    fn default() -> Self {
        Self {
            drafts: "Drafts".into(),
            trash: "Trash".into(),
            archive: None,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtraAccount {
    registration_key: String,
    id: String,
    display_name: String,
    address: String,
    imap: Imap,
    password: SecretRef,
    #[serde(default)]
    folders: Folders,
    read_subjects: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLegacy {
    display_name: String,
    address: String,
    imap: Imap,
    password: SecretRef,
    smtp: Option<Smtp>,
    #[serde(default)]
    folders: Folders,
    permission_mode: String,
    read_subjects: Vec<String>,
    write_subjects: Vec<String>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum SecretRef {
    Environment(EnvironmentSecret),
    File(FileSecret),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentSecret {
    env: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSecret {
    file: PathBuf,
}

pub fn load_runtime() -> anyhow::Result<ValidatedAccountSet> {
    load_checked(&|key| std::env::var_os(key))
}
fn load_checked(lookup: &dyn Fn(&str) -> Option<OsString>) -> anyhow::Result<ValidatedAccountSet> {
    load_with(lookup).map_err(|_| anyhow::anyhow!(CONFIG_ERROR))
}
fn bounded_file(path: &Path, max: usize) -> anyhow::Result<Vec<u8>> {
    bounded_handle(File::open(path)?, max)
}
fn bounded_handle(file: File, max: usize) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(file.metadata()?.len() <= max as u64, CONFIG_ERROR);
    let mut bytes = Vec::with_capacity(max + 1);
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= max, CONFIG_ERROR);
    Ok(bytes)
}
fn resolve_secret(
    secret: SecretRef,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> anyhow::Result<String> {
    let value = match secret {
        SecretRef::Environment(source) => {
            anyhow::ensure!(
                !source.env.is_empty()
                    && source.env.len() <= 128
                    && source.env.bytes().all(|byte| byte.is_ascii_uppercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'),
                CONFIG_ERROR
            );
            lookup_string(lookup, &source.env)?.ok_or_else(|| anyhow::anyhow!(CONFIG_ERROR))?
        }
        SecretRef::File(source) => resolve_file_secret(source, lookup)?,
    };
    anyhow::ensure!(
        !value.is_empty() && value.len() <= MAX_SECRET_BYTES && !value.contains(['\0', '\r', '\n']),
        CONFIG_ERROR
    );
    Ok(value)
}

// Windows callers use environment references until native ACL validation is
// available. Do not silently skip the owner-only file permission boundary.
#[cfg(not(unix))]
fn resolve_file_secret(
    source: FileSecret,
    _lookup: &dyn Fn(&str) -> Option<OsString>,
) -> anyhow::Result<String> {
    let _ = source.file;
    anyhow::bail!(CONFIG_ERROR)
}

#[cfg(unix)]
fn resolve_file_secret(
    source: FileSecret,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> anyhow::Result<String> {
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::Component,
    };
    let root = lookup("MAIL_SECRET_ROOT")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!(CONFIG_ERROR))?;
    anyhow::ensure!(
        !source.file.as_os_str().is_empty()
            && source
                .file
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        CONFIG_ERROR
    );
    let root = fs::canonicalize(root)?;
    anyhow::ensure!(root.is_dir(), CONFIG_ERROR);
    anyhow::ensure!(
        fs::metadata(&root)?.permissions().mode() & 0o022 == 0,
        CONFIG_ERROR
    );
    let mut path = root.clone();
    for component in source.file.components() {
        path.push(component);
        let metadata = fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink()
                && (!metadata.is_dir() || metadata.permissions().mode() & 0o022 == 0),
            CONFIG_ERROR
        );
    }
    let canonical = fs::canonicalize(&path)?;
    anyhow::ensure!(canonical.starts_with(&root), CONFIG_ERROR);
    let before = fs::symlink_metadata(&canonical)?;
    anyhow::ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        CONFIG_ERROR
    );
    let file = File::open(&canonical)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), CONFIG_ERROR);
    anyhow::ensure!(
        before.dev() == metadata.dev() && before.ino() == metadata.ino(),
        CONFIG_ERROR
    );
    anyhow::ensure!(metadata.permissions().mode() & 0o077 == 0, CONFIG_ERROR);
    let mut value = String::from_utf8(bounded_handle(file, MAX_SECRET_BYTES)?)?;
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    Ok(value)
}
fn load_with(lookup: &dyn Fn(&str) -> Option<OsString>) -> anyhow::Result<ValidatedAccountSet> {
    let Some(path) = lookup("ACCOUNT_CONFIG_PATH").map(PathBuf::from) else {
        return Ok(ValidatedAccountSet::legacy(
            Config::legacy_from_environment(lookup)?,
        ));
    };
    let bytes = bounded_file(&path, MAX_ACCOUNT_CONFIG_BYTES)?;
    let document: Document = serde_json::from_slice(&bytes)?;
    let (source, version, default_read_account, config, overlay, extras) = match document {
        Document::ExtendLegacy {
            version,
            default_account_id,
            legacy_default,
            accounts,
        } => (
            AccountConfigSource::ExtendLegacy,
            version,
            default_account_id,
            Config::legacy_from_environment(lookup)?,
            legacy_default,
            accounts,
        ),
        Document::FileOnly {
            version,
            default_account_id,
            legacy_default: legacy,
            accounts,
        } => {
            for key in [
                "EMAIL_ADDRESS",
                "EMAIL_PASSWORD",
                "IMAP_HOST",
                "IMAP_PORT",
                "SMTP_HOST",
                "SMTP_PORT",
                "SMTP_STARTTLS",
                "DRAFTS_MAILBOX",
                "TRASH_MAILBOX",
                "ARCHIVE_MAILBOX",
                "MAIL_MODE",
            ] {
                anyhow::ensure!(lookup(key).is_none(), CONFIG_ERROR);
            }
            anyhow::ensure!(
                ["read", "draft", "send", "full"].contains(&legacy.permission_mode.as_str()),
                CONFIG_ERROR
            );
            anyhow::ensure!(
                legacy.smtp.is_some() || legacy.permission_mode == "read",
                CONFIG_ERROR
            );
            if let Some(smtp) = &legacy.smtp {
                check_text(&smtp.host, 253, 253)?;
                anyhow::ensure!(smtp.port != 0, CONFIG_ERROR);
            }
            let config = Config::from_mail(
                MailValues {
                    address: legacy.address,
                    password: resolve_secret(legacy.password, lookup)?,
                    imap_host: legacy.imap.host,
                    imap_port: legacy.imap.port,
                    smtp_host: legacy
                        .smtp
                        .as_ref()
                        .map_or_else(String::new, |smtp| smtp.host.clone()),
                    smtp_port: legacy.smtp.as_ref().map_or(465, |smtp| smtp.port),
                    smtp_starttls: legacy.smtp.as_ref().is_some_and(|smtp| smtp.starttls),
                    drafts: legacy.folders.drafts,
                    trash: legacy.folders.trash,
                    archive: legacy.folders.archive,
                    mode: legacy.permission_mode,
                },
                lookup,
            )?;
            (
                AccountConfigSource::FileOnly,
                version,
                default_account_id,
                config,
                LegacyOverlay {
                    display_name: legacy.display_name,
                    read_subjects: legacy.read_subjects,
                    write_subjects: legacy.write_subjects,
                },
                accounts,
            )
        }
    };
    anyhow::ensure!(version == 1 && extras.len() < MAX_ACCOUNTS, CONFIG_ERROR);
    let mut accounts = vec![ValidatedAccountConfig {
        registration_key: LEGACY_REGISTRATION_KEY.into(),
        alias: DEFAULT_ACCOUNT_ID.into(),
        display_name: overlay.display_name,
        config: config.clone(),
        read_subjects: overlay.read_subjects,
        write_subjects: overlay.write_subjects,
        is_legacy: true,
    }];
    for account in extras {
        let mut read_config = (*config).clone();
        read_config.address = account.address;
        read_config.password = resolve_secret(account.password, lookup)?;
        read_config.imap_host = account.imap.host;
        read_config.imap_port = account.imap.port;
        read_config.drafts = account.folders.drafts;
        read_config.trash = account.folders.trash;
        read_config.archive = account.folders.archive;
        read_config.mode = "read".into();
        accounts.push(ValidatedAccountConfig {
            registration_key: account.registration_key,
            alias: account.id,
            display_name: account.display_name,
            config: Arc::new(read_config),
            read_subjects: account.read_subjects,
            write_subjects: vec![],
            is_legacy: false,
        });
    }
    let set = ValidatedAccountSet {
        service_config: config,
        default_read_account,
        accounts,
        source,
    };
    set.validate()?;
    Ok(set)
}
