//! Current request authorization and bounded transactional account generations.
use super::*;
use crate::{
    config::{AccountConfigSource, ValidatedAccountSet},
    error::{MailError, Result},
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, RwLock},
};
use tokio_util::sync::CancellationToken;

const MAX_V2_BYTES: u64 = 1024 * 1024;
const MAX_RECORDS: usize = 256;
const RECORD_SQL: &str = "CREATE TABLE account_records (registration_key TEXT PRIMARY KEY, uuid TEXT NOT NULL UNIQUE, generation INTEGER NOT NULL CHECK(generation>0), identity_fingerprint TEXT NOT NULL, alias TEXT NOT NULL, active INTEGER NOT NULL CHECK(active IN(0,1)), legacy INTEGER NOT NULL CHECK(legacy IN(0,1)))";
const META_SQL: &str = "CREATE TABLE registry_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1), requires_accounts_file INTEGER NOT NULL CHECK(requires_accounts_file=1))";
const INDEX_SQL: &str =
    "CREATE UNIQUE INDEX active_account_aliases ON account_records(alias) WHERE active=1";

#[cfg(test)]
#[path = "catalog/tests.rs"]
mod tests;

#[derive(Clone)]
pub struct AccountLease {
    pub key: AccountKey,
    pub alias: String,
    pub display_name: String,
    pub config: Arc<Config>,
    pub is_legacy: bool,
    pub cancellation: CancellationToken,
}
struct RegisteredAccount {
    lease: AccountLease,
    read_subjects: Vec<String>,
    write_subjects: Vec<String>,
}
pub struct AccountCatalog {
    config: Arc<Config>,
    default_read_account: String,
    accounts: RwLock<BTreeMap<String, RegisteredAccount>>,
    _registry_lock: Option<File>,
}
impl AccountCatalog {
    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }
    pub fn default_read_account_id(&self) -> &str {
        &self.default_read_account
    }
    fn global(&self, principal: &str) -> bool {
        self.config
            .subjects
            .iter()
            .any(|subject| subject == principal)
    }
    pub fn read_account(&self, principal: &str, requested: Option<&str>) -> Result<AccountLease> {
        if !self.global(principal) {
            return Err(MailError::account_unavailable());
        }
        let alias = requested.unwrap_or(&self.default_read_account);
        self.accounts
            .read()
            .map_err(|_| MailError::account_unavailable())?
            .get(alias)
            .filter(|account| {
                !account.lease.cancellation.is_cancelled()
                    && account.read_subjects.iter().any(|s| s == principal)
            })
            .map(|account| account.lease.clone())
            .ok_or_else(MailError::account_unavailable)
    }
    pub fn read_key(&self, principal: &str, key: &AccountKey) -> Result<AccountLease> {
        if !self.global(principal) {
            return Err(MailError::account_unavailable());
        }
        self.accounts
            .read()
            .map_err(|_| MailError::account_unavailable())?
            .values()
            .find(|account| {
                &account.lease.key == key
                    && !account.lease.cancellation.is_cancelled()
                    && account.read_subjects.iter().any(|s| s == principal)
            })
            .map(|account| account.lease.clone())
            .ok_or_else(MailError::account_unavailable)
    }
    pub fn write_account(&self, principal: &str) -> Result<AccountLease> {
        if !self.global(principal) {
            return Err(MailError::account_unavailable());
        }
        self.accounts
            .read()
            .map_err(|_| MailError::account_unavailable())?
            .get(DEFAULT_ACCOUNT_ID)
            .filter(|account| {
                account.lease.is_legacy
                    && account.lease.config.mode != "read"
                    && !account.lease.cancellation.is_cancelled()
                    && account.write_subjects.iter().any(|s| s == principal)
            })
            .map(|account| account.lease.clone())
            .ok_or_else(MailError::account_unavailable)
    }
    pub fn visible_accounts(&self, principal: &str) -> Vec<AccountLease> {
        if !self.global(principal) {
            return vec![];
        }
        self.accounts
            .read()
            .map(|accounts| {
                accounts
                    .values()
                    .filter(|account| {
                        !account.lease.cancellation.is_cancelled()
                            && account.read_subjects.iter().any(|s| s == principal)
                    })
                    .map(|account| account.lease.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn registered_accounts(&self) -> Vec<AccountLease> {
        self.accounts
            .read()
            .map(|accounts| {
                accounts
                    .values()
                    .map(|account| account.lease.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(test)]
    pub(crate) fn from_test_accounts(set: ValidatedAccountSet) -> anyhow::Result<Arc<Self>> {
        set.validate()?;
        let keys = set
            .accounts
            .iter()
            .map(|account| (account.registration_key.clone(), AccountKey::synthetic()))
            .collect();
        Ok(Arc::new(Self::build(set, keys, None)?))
    }
    #[cfg(test)]
    pub(crate) fn revoke_read_for_test(&self, alias: &str, principal: &str) -> Result<()> {
        let mut accounts = self
            .accounts
            .write()
            .map_err(|_| MailError::account_unavailable())?;
        let account = accounts
            .get_mut(alias)
            .ok_or_else(MailError::account_unavailable)?;
        account.read_subjects.retain(|s| s != principal);
        account.write_subjects.retain(|s| s != principal);
        account.lease.cancellation.cancel();
        account.lease.cancellation = CancellationToken::new();
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn remove_account_for_test(&self, alias: &str) -> Result<()> {
        let mut accounts = self
            .accounts
            .write()
            .map_err(|_| MailError::account_unavailable())?;
        if accounts
            .get(alias)
            .is_none_or(|account| account.lease.is_legacy)
        {
            return Err(MailError::account_unavailable());
        }
        if let Some(account) = accounts.remove(alias) {
            account.lease.cancellation.cancel();
        }
        Ok(())
    }
    fn build(
        set: ValidatedAccountSet,
        mut keys: BTreeMap<String, AccountKey>,
        lock: Option<File>,
    ) -> anyhow::Result<Self> {
        let mut accounts = BTreeMap::new();
        for spec in set.accounts {
            let key = keys
                .remove(&spec.registration_key)
                .ok_or_else(|| anyhow::anyhow!(REGISTRY_ERROR))?;
            accounts.insert(
                spec.alias.clone(),
                RegisteredAccount {
                    lease: AccountLease {
                        key,
                        alias: spec.alias,
                        display_name: spec.display_name,
                        config: spec.config,
                        is_legacy: spec.is_legacy,
                        cancellation: CancellationToken::new(),
                    },
                    read_subjects: spec.read_subjects,
                    write_subjects: spec.write_subjects,
                },
            );
        }
        Ok(Self {
            config: set.service_config,
            default_read_account: set.default_read_account,
            accounts: RwLock::new(accounts),
            _registry_lock: lock,
        })
    }
}
impl Drop for AccountCatalog {
    fn drop(&mut self) {
        if let Ok(accounts) = self.accounts.read() {
            for account in accounts.values() {
                account.lease.cancellation.cancel();
            }
        }
    }
}

pub fn open_catalog(set: ValidatedAccountSet) -> anyhow::Result<Arc<AccountCatalog>> {
    set.validate()?;
    open_inner(set, false).map(Arc::new).map_err(|error| {
        if error.to_string() == BINDING_ERROR {
            anyhow::anyhow!(BINDING_ERROR)
        } else {
            anyhow::anyhow!(REGISTRY_ERROR)
        }
    })
}

#[derive(Clone, PartialEq, Eq)]
struct Record {
    uuid: Uuid,
    generation: u64,
    identity: String,
    alias: String,
    active: bool,
    legacy: bool,
}
struct Registry {
    legacy: AccountKey,
    records: BTreeMap<String, Record>,
    version: u32,
}
fn identity(config: &Config) -> anyhow::Result<String> {
    Ok(fingerprint(&serde_json::to_vec(&(
        "legacy-default-v1",
        &config.address,
        config.imap_host.to_ascii_lowercase(),
        config.imap_port,
    ))?))
}
fn operation_fingerprint(operation: Option<&Path>) -> anyhow::Result<String> {
    Ok(match operation {
        Some(path) => fingerprint(&serde_json::to_vec(&(
            "persistent-operation-path-v1",
            path.as_os_str().as_encoded_bytes(),
        ))?),
        None => fingerprint(b"memory-operation-database-v1"),
    })
}

fn inspect(
    path: &Path,
    expected_identity: &str,
    expected_operation: &str,
) -> anyhow::Result<Registry> {
    let metadata = fs::metadata(path)?;
    ensure_owner_only(&metadata)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() <= MAX_V2_BYTES,
        REGISTRY_ERROR
    );
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let version: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 1 {
        drop(db);
        return Ok(Registry {
            legacy: load_registry(path, expected_identity, expected_operation)?,
            records: BTreeMap::new(),
            version,
        });
    }
    anyhow::ensure!(version == 2, REGISTRY_ERROR);
    let quick: String = db.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    anyhow::ensure!(quick == "ok", REGISTRY_ERROR);
    let mut schemas =
        db.prepare("SELECT name,sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'")?;
    let actual: BTreeMap<String, String> = schemas
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let expected = BTreeMap::from([
        ("legacy_default".into(), TABLE_SQL.into()),
        ("account_records".into(), RECORD_SQL.into()),
        ("registry_meta".into(), META_SQL.into()),
        ("active_account_aliases".into(), INDEX_SQL.into()),
    ]);
    anyhow::ensure!(actual == expected, REGISTRY_ERROR);
    let legacy_count: i64 =
        db.query_row("SELECT count(*) FROM legacy_default", [], |row| row.get(0))?;
    anyhow::ensure!(legacy_count == 1, REGISTRY_ERROR);
    let (uuid,generation,stored_identity,stored_operation):(String,i64,String,String)=db.query_row("SELECT uuid,generation,identity_fingerprint,operation_path_fingerprint FROM legacy_default WHERE alias='default'",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
    anyhow::ensure!(
        stored_identity == expected_identity && stored_operation == expected_operation,
        BINDING_ERROR
    );
    let legacy = AccountKey {
        uuid: Uuid::parse_str(&uuid)?,
        generation: u64::try_from(generation)?,
    };
    anyhow::ensure!(
        !legacy.uuid.is_nil() && legacy.generation == 1,
        REGISTRY_ERROR
    );
    let meta: (i64, i64) = db.query_row(
        "SELECT count(*),sum(requires_accounts_file) FROM registry_meta",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    anyhow::ensure!(meta == (1, 1), REGISTRY_ERROR);
    let count: i64 = db.query_row("SELECT count(*) FROM account_records", [], |row| row.get(0))?;
    anyhow::ensure!((1..=MAX_RECORDS as i64).contains(&count), REGISTRY_ERROR);
    let mut statement=db.prepare("SELECT registration_key,uuid,generation,identity_fingerprint,alias,active,legacy FROM account_records")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut records = BTreeMap::new();
    let mut uuids = HashSet::new();
    for row in rows {
        let (key, uuid, generation, identity, alias, active, is_legacy) = row?;
        let uuid = Uuid::parse_str(&uuid)?;
        anyhow::ensure!(
            valid_account_id(&key)
                && valid_account_id(&alias)
                && !uuid.is_nil()
                && uuids.insert(uuid)
                && generation > 0
                && identity.len() == 64
                && identity
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && (0..=1).contains(&active)
                && (0..=1).contains(&is_legacy),
            REGISTRY_ERROR
        );
        let record = Record {
            uuid,
            generation: generation as u64,
            identity,
            alias,
            active: active == 1,
            legacy: is_legacy == 1,
        };
        if record.legacy {
            anyhow::ensure!(
                key == LEGACY_REGISTRATION_KEY
                    && record.uuid == legacy.uuid
                    && record.generation == 1
                    && record.identity == expected_identity
                    && record.alias == DEFAULT_ACCOUNT_ID
                    && record.active,
                REGISTRY_ERROR
            );
        } else {
            anyhow::ensure!(key != LEGACY_REGISTRATION_KEY, REGISTRY_ERROR);
        }
        records.insert(key, record);
    }
    anyhow::ensure!(
        records
            .get(LEGACY_REGISTRATION_KEY)
            .is_some_and(|record| record.legacy),
        REGISTRY_ERROR
    );
    Ok(Registry {
        legacy,
        records,
        version,
    })
}

fn open_inner(
    set: ValidatedAccountSet,
    fail_before_publish: bool,
) -> anyhow::Result<AccountCatalog> {
    if set.source == AccountConfigSource::Legacy {
        let key = open_default(&set.service_config)?;
        return AccountCatalog::build(
            set,
            BTreeMap::from([(LEGACY_REGISTRATION_KEY.into(), key)]),
            None,
        );
    }
    let config = &set.service_config;
    let operation = if config.database == ":memory:" {
        None
    } else {
        Some(normalized_file(Path::new(&config.database))?)
    };
    let registry = normalized_file(&config.account_registry.clone().unwrap_or_else(|| {
        operation
            .as_ref()
            .and_then(|p| p.parent())
            .unwrap_or_else(|| Path::new("data"))
            .join("accounts.sqlite")
    }))?;
    if let Some(operation) = &operation {
        anyhow::ensure!(!paths_conflict(operation, &registry)?, REGISTRY_ERROR);
    }
    let lock = private_file_options()
        .create(true)
        .open(sidecar(&registry, ".lock"))?;
    ensure_owner_only(&lock.metadata()?)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => anyhow::bail!(REGISTRY_ERROR),
        }
    }
    if let Some(operation) = &operation {
        anyhow::ensure!(!paths_conflict(operation, &registry)?, REGISTRY_ERROR);
    }
    let stage = StagedRegistry(sidecar(&registry, ".staging"));
    stage.cleanup()?;
    let previous_stage = StagedRegistry(sidecar(&registry, ".previous-staging"));
    previous_stage.cleanup()?;
    let legacy_identity = identity(config)?;
    let operation_hash = operation_fingerprint(operation.as_deref())?;
    let current = if registry.try_exists()? {
        Some(inspect(&registry, &legacy_identity, &operation_hash)?)
    } else {
        None
    };
    let legacy = current
        .as_ref()
        .map(|state| state.legacy.clone())
        .unwrap_or(AccountKey {
            uuid: Uuid::new_v4(),
            generation: 1,
        });
    let original = current
        .as_ref()
        .map(|state| state.records.clone())
        .unwrap_or_default();
    let mut records = original.clone();
    for record in records.values_mut() {
        record.active = false;
    }
    for spec in &set.accounts {
        let identity = identity(&spec.config)?;
        let record = if spec.is_legacy {
            Record {
                uuid: legacy.uuid,
                generation: 1,
                identity,
                alias: spec.alias.clone(),
                active: true,
                legacy: true,
            }
        } else if let Some(old) = original.get(&spec.registration_key) {
            Record {
                uuid: old.uuid,
                generation: if !old.active || old.identity != identity {
                    old.generation
                        .checked_add(1)
                        .filter(|generation| *generation <= i64::MAX as u64)
                        .ok_or_else(|| anyhow::anyhow!(REGISTRY_ERROR))?
                } else {
                    old.generation
                },
                identity,
                alias: spec.alias.clone(),
                active: true,
                legacy: false,
            }
        } else {
            Record {
                uuid: Uuid::new_v4(),
                generation: 1,
                identity,
                alias: spec.alias.clone(),
                active: true,
                legacy: false,
            }
        };
        records.insert(spec.registration_key.clone(), record);
    }
    anyhow::ensure!(records.len() <= MAX_RECORDS, REGISTRY_ERROR);
    if current
        .as_ref()
        .is_none_or(|state| state.version != 2 || state.records != records)
    {
        let stage_file = private_file_options().create_new(true).open(&stage.0)?;
        if registry.try_exists()? {
            fs::copy(&registry, &stage.0)?;
        }
        let mut db = Connection::open(&stage.0)?;
        db.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA max_page_count=256;",
        )?;
        let transaction = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if current.is_none() {
            transaction.execute_batch(TABLE_SQL)?;
            transaction.execute(
                "INSERT INTO legacy_default VALUES ('default',?1,1,?2,?3)",
                params![legacy.uuid.to_string(), legacy_identity, operation_hash],
            )?;
        }
        if current.as_ref().is_none_or(|state| state.version == 1) {
            transaction.execute_batch(RECORD_SQL)?;
            transaction.execute_batch(META_SQL)?;
            transaction.execute_batch(INDEX_SQL)?;
            transaction.execute("INSERT INTO registry_meta VALUES (1,1)", [])?;
        }
        transaction.execute("DELETE FROM account_records", [])?;
        for (key, record) in &records {
            transaction.execute(
                "INSERT INTO account_records VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    key,
                    record.uuid.to_string(),
                    record.generation as i64,
                    record.identity,
                    record.alias,
                    record.active,
                    record.legacy
                ],
            )?;
        }
        transaction.execute_batch("PRAGMA user_version=2;")?;
        transaction.commit()?;
        db.close().map_err(|(_, error)| error)?;
        stage_file.sync_all()?;
        let verified = inspect(&stage.0, &legacy_identity, &operation_hash)?;
        anyhow::ensure!(verified.records == records, REGISTRY_ERROR);
        anyhow::ensure!(!fail_before_publish, REGISTRY_ERROR);
        if current.as_ref().is_some_and(|state| state.version == 1) {
            let backup = sidecar(&registry, ".v1-backup");
            if backup.try_exists()? {
                let verified_backup = inspect(&backup, &legacy_identity, &operation_hash)?;
                anyhow::ensure!(verified_backup.version == 1, REGISTRY_ERROR);
                anyhow::ensure!(fs::read(&backup)? == fs::read(&registry)?, REGISTRY_ERROR);
            } else {
                fs::hard_link(&registry, &backup)?;
            }
        }
        if current.is_some() {
            // Keep the complete immediately preceding registry even if a later
            // directory sync reports an I/O failure after atomic publication.
            fs::hard_link(&registry, &previous_stage.0)?;
            fs::rename(&previous_stage.0, sidecar(&registry, ".previous"))?;
        }
        fs::rename(&stage.0, &registry)?;
        #[cfg(unix)]
        File::open(
            registry
                .parent()
                .ok_or_else(|| anyhow::anyhow!(REGISTRY_ERROR))?,
        )?
        .sync_all()?;
    }
    let keys = records
        .into_iter()
        .filter(|(_, record)| record.active)
        .map(|(key, record)| {
            (
                key,
                AccountKey {
                    uuid: record.uuid,
                    generation: record.generation,
                },
            )
        })
        .collect();
    AccountCatalog::build(set, keys, Some(lock))
}
