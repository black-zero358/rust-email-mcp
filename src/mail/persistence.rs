//! Admission and provenance for write-through metadata observations.
use super::*;
use crate::{
    cache::{
        AccountScope, ArtifactKey, ArtifactState, Cache, CacheError, ListingEvidence,
        MailboxEvidence, Omission, Ref as CacheRef, ThreadId,
    },
    config::{CacheMode, CacheSettings},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

const CACHE_JOBS: usize = EXECUTION_SLOTS * 4;
const CACHE_JOB_BYTES: usize = 8 * 1024 * 1024;
const INDEX_BYTES: usize = 8 * 1024;
const PARTS_BYTES: usize = 256 * 1024;
const WRITE_FENCE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
// Two retained payloads plus one 32 MiB conversion buffer on the single FIFO
// worker. The worker releases raw before constructing the full-text index.
const CONTENT_PAYLOADS: usize = 2;

#[derive(Clone, Default)]
pub struct PreparedCache(Option<Arc<Persistent>>);

struct Persistent {
    mode: CacheMode,
    storage: Option<Arc<Cache>>,
    queue: Option<mpsc::Sender<CacheMessage>>,
    folders: HashMap<AccountKey, HashSet<String>>,
    health: Arc<WriteHealthByAccount>,
    generations: Arc<Generations>,
    job_count: Arc<Semaphore>,
    job_bytes: Arc<Semaphore>,
    content_jobs: Arc<Semaphore>,
    content_pending: HashMap<AccountKey, Arc<AtomicUsize>>,
    cache_read_jobs: Arc<Semaphore>,
    pending_tombstones: Arc<Mutex<HashSet<(GenerationKey, u32, u32)>>>,
}

type CacheJob = Box<dyn FnOnce(&Arc<Cache>) + Send>;
enum CacheMessage {
    Work(CacheJob),
    Finish(Arc<Persistent>, oneshot::Sender<()>),
}
type GenerationKey = ([u8; 32], String);
type Generations = Mutex<HashMap<GenerationKey, Generation>>;

struct Generation {
    current: u32,
    blocked: bool,
}

pub(super) struct ContentAdmission {
    _permit: OwnedSemaphorePermit,
    pending: Arc<AtomicUsize>,
}

impl Drop for ContentAdmission {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::Release);
    }
}

type WriteHealthByAccount = Mutex<HashMap<AccountKey, WriteHealth>>;

#[derive(Clone, Copy, Default, serde::Serialize)]
struct WriteHealth {
    failed_writes: u64,
    last_error: Option<WriteFailure>,
    quota_blocked: bool,
    #[serde(skip)]
    unavailable: bool,
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum WriteFailure {
    Quota,
    QueueFull,
    Storage,
}

impl PreparedCache {
    pub(super) fn open(
        catalog: &crate::accounts::AccountCatalog,
        settings: CacheSettings,
        run: Option<crate::accounts::CacheRunIdentity>,
    ) -> anyhow::Result<Self> {
        let CacheSettings {
            mode,
            directory,
            max_bytes,
            accounts,
            reserved,
        } = settings;
        let registered = catalog.registered_accounts();
        let mut folders = HashMap::with_capacity(accounts.len());
        let mut scopes = Vec::with_capacity(accounts.len());
        for account in accounts {
            let key = registered
                .iter()
                .find(|lease| lease.alias == account.account_id)
                .map(|lease| lease.key.clone())
                .ok_or_else(|| anyhow::anyhow!("Cache account is not registered"))?;
            folders.insert(key.clone(), account.mailboxes.iter().cloned().collect());
            scopes.push(AccountScope {
                account: key,
                mailboxes: account.mailboxes,
                max_bytes: account.max_bytes,
            });
        }
        let storage = match mode {
            CacheMode::Metadata => Cache::open(&directory, max_bytes, scopes, &reserved).ok(),
            CacheMode::Full => Some(
                Cache::open_full_with_run(
                    &directory,
                    max_bytes,
                    scopes,
                    &reserved,
                    run.ok_or_else(|| anyhow::anyhow!("Full cache requires a runtime witness"))?,
                )
                .map_err(|_| anyhow::anyhow!("Could not open the full cache"))?,
            ),
        };
        let (storage, queue) = storage
            .and_then(|storage| {
                let (queue, mut jobs) = mpsc::channel::<CacheMessage>(CACHE_JOBS);
                let worker = storage.clone();
                std::thread::Builder::new()
                    .name("metadata-cache".into())
                    .spawn(move || {
                        while let Some(job) = jobs.blocking_recv() {
                            match job {
                                CacheMessage::Work(job) => job(&worker),
                                CacheMessage::Finish(state, done) => {
                                    if state.can_mark_clean() {
                                        let _ = worker.mark_clean();
                                    }
                                    drop(state);
                                    drop(worker);
                                    let _ = done.send(());
                                    return;
                                }
                            }
                        }
                    })
                    .ok()
                    .map(|_| (Some(storage), Some(queue)))
            })
            .unwrap_or_default();
        if storage.is_none() {
            anyhow::ensure!(
                mode != CacheMode::Full,
                "Could not start the full cache worker"
            );
            tracing::warn!(event = "cache_unavailable");
        }
        Ok(Self(Some(Arc::new(Persistent {
            mode,
            storage,
            queue,
            content_pending: folders
                .keys()
                .map(|key| (key.clone(), Arc::default()))
                .collect(),
            folders,
            health: Arc::new(Mutex::new(HashMap::new())),
            generations: Arc::new(Mutex::new(HashMap::new())),
            job_count: Arc::new(Semaphore::new(CACHE_JOBS)),
            job_bytes: Arc::new(Semaphore::new(CACHE_JOB_BYTES)),
            content_jobs: Arc::new(Semaphore::new(CONTENT_PAYLOADS)),
            cache_read_jobs: Arc::new(Semaphore::new(EXECUTION_SLOTS)),
            pending_tombstones: Arc::new(Mutex::new(HashSet::new())),
        }))))
    }

    pub(super) fn full_storage(&self, account: &AccountKey) -> Option<&Arc<Cache>> {
        let state = self.0.as_ref()?;
        (state.mode == CacheMode::Full && state.folders.contains_key(account))
            .then_some(state.storage.as_ref())
            .flatten()
    }

    pub(super) fn cache_read_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.0
            .as_ref()?
            .cache_read_jobs
            .clone()
            .try_acquire_owned()
            .ok()
    }

    pub(super) fn full_folder(&self, account: &AccountKey, mailbox: &str) -> Option<&Arc<Cache>> {
        self.enabled_for_folder(account, mailbox)
            .then(|| self.full_storage(account))
            .flatten()
    }

    pub(super) fn configured_folders(&self, account: &AccountKey) -> impl Iterator<Item = &str> {
        self.0
            .as_ref()
            .and_then(|state| state.folders.get(account))
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    pub(super) fn offline_current(
        &self,
        account: &AccountKey,
        mailbox: &str,
        validity: u32,
        uid: Option<u32>,
    ) -> bool {
        let Some(state) = &self.0 else { return false };
        if !state.enabled(account, mailbox) {
            return false;
        }
        let mailbox = canonical_mailbox(mailbox);
        let namespace = account.reference_namespace();
        state.generation_matches(&namespace, mailbox, validity)
            && state.pending_tombstones.lock().is_ok_and(|pending| {
                !pending.iter().any(|((key, folder), epoch, tombstone)| {
                    *key == namespace
                        && folder == mailbox
                        && *epoch == validity
                        && uid.is_none_or(|uid| uid == *tombstone)
                })
            })
    }

    pub(super) fn configured(&self, account: &AccountKey) -> bool {
        self.0
            .as_ref()
            .is_some_and(|state| state.folders.contains_key(account))
    }

    pub(super) fn configured_folder(&self, account: &AccountKey, mailbox: &str) -> bool {
        self.0
            .as_ref()
            .is_some_and(|state| state.configured_folder(account, mailbox))
    }

    pub(super) fn durable_account(&self, account: &AccountKey) -> bool {
        self.0
            .as_ref()
            .is_some_and(|state| state.storage.is_some() && state.folders.contains_key(account))
    }

    pub(super) fn cursor_store(&self) -> conversation::CursorStore {
        let Some(state) = &self.0 else {
            return Default::default();
        };
        let Some(storage) = &state.storage else {
            return Default::default();
        };
        let scopes = Arc::new(
            state
                .folders
                .keys()
                .map(|account| (account.reference_namespace(), account.clone()))
                .collect::<HashMap<_, _>>(),
        );
        let configured = state.clone();
        let configured_accounts = scopes.clone();
        let observed_accounts = scopes.clone();
        let eligibility = state.clone();
        let generations = state.clone();
        conversation::CursorStore::with_backend(
            storage.cursor_backend(),
            move |namespace, folder| {
                configured_accounts
                    .get(namespace)
                    .is_some_and(|account| configured.configured_folder(account, folder))
            },
            move |namespace, folder, validity| {
                observed_accounts.get(namespace).is_none_or(|account| {
                    !generations.enabled(account, folder)
                        || generations.generation_matches(namespace, folder, validity)
                })
            },
            move |namespace, folder| {
                scopes
                    .get(namespace)
                    .is_some_and(|account| eligibility.enabled(account, folder))
            },
        )
        .unwrap_or_else(|_| {
            tracing::warn!(event = "conversation_cache_unavailable");
            Default::default()
        })
    }

    pub async fn finish(self) {
        if let Some(state) = &self.0
            && let Some(queue) = &state.queue
        {
            state.cache_read_jobs.close();
            let (done, wait) = oneshot::channel();
            if queue
                .send(CacheMessage::Finish(state.clone(), done))
                .await
                .is_ok()
            {
                let _ = wait.await;
            }
        }
    }

    pub fn mark_restored(&self) -> anyhow::Result<()> {
        let storage = self
            .0
            .as_ref()
            .and_then(|state| state.storage.as_ref())
            .ok_or_else(|| anyhow::anyhow!("Configured metadata cache is unavailable"))?;
        storage
            .mark_restored()
            .map_err(|_| anyhow::anyhow!("Could not mark the metadata cache restored"))
    }

    pub(super) async fn status(&self, account: &AccountKey, alias: &str) -> Value {
        let Some(state) = self
            .0
            .as_ref()
            .filter(|state| state.folders.contains_key(account))
        else {
            return json!({"account_id":alias,"mode":"off","status":"off","storage":null,"write_health":WriteHealth::default()});
        };
        let health = state.health.clone();
        let mode = state.mode.name();
        let full = state.mode == CacheMode::Full;
        let (done, wait) = oneshot::channel();
        let submitted = self.submit(account, 4096, false, move |cache, account| {
            match cache.status(account) {
                Ok(status) => {
                    let mut folders =
                        serde_json::to_value(&status.folders).map_err(|_| CacheError::Corrupt)?;
                    if full {
                        for (folder, value) in status
                            .folders
                            .iter()
                            .zip(folders.as_array_mut().ok_or(CacheError::Corrupt)?)
                        {
                            if let Some(validity) = folder.uid_validity {
                                value["layers"] =
                                    coverage(cache, account, &folder.mailbox, validity)?;
                            }
                        }
                    }
                    let _ = done.send((status, folders, read_health(&health, account)));
                    Ok(())
                }
                Err(error) => Err(error),
            }
        });
        let result = if submitted {
            tokio::time::timeout(std::time::Duration::from_secs(2), wait)
                .await
                .ok()
                .and_then(|result| result.ok())
        } else {
            None
        };
        let result =
            result.filter(|(status, _, _)| state.account_current(account, &status.folders));
        let mut value = match result {
            Some((status, folders, health)) => json!({
                "account_id":alias,"mode":mode,"status":"ready",
                "write_health":health,
                "storage":{
                    "max_bytes":status.max_bytes, "used_bytes":status.used_bytes,
                    "global_max_bytes":status.global_max_bytes, "startup_unclean":status.startup_unclean,
                    "folders":folders
                }
            }),
            None => {
                json!({"account_id":alias,"mode":mode,"status":"unavailable","storage":null,
                    "write_health":read_health(&state.health, account)})
            }
        };
        if full {
            value["content_pending"] = json!(
                state
                    .content_pending
                    .get(account)
                    .map_or(0, |count| count.load(Ordering::Acquire))
            );
        }
        value
    }

    fn enabled_for_folder(&self, account: &AccountKey, mailbox: &str) -> bool {
        self.0
            .as_ref()
            .is_some_and(|state| state.enabled(account, mailbox))
    }

    fn submit(
        &self,
        account: &AccountKey,
        bytes: usize,
        record_write: bool,
        operation: impl FnOnce(&Arc<Cache>, &AccountKey) -> std::result::Result<(), CacheError>
        + Send
        + 'static,
    ) -> bool {
        let Some(state) = &self.0 else { return false };
        let Some(queue) = &state.queue else {
            return false;
        };
        if read_health(&state.health, account).unavailable {
            return false;
        }
        let permits = (|| {
            let count = state.job_count.clone().try_acquire_owned().ok()?;
            let bytes = state
                .job_bytes
                .clone()
                .try_acquire_many_owned(bytes.try_into().ok()?)
                .ok()?;
            Some((count, bytes))
        })();
        let Some(permits) = permits else {
            record_failure(
                &state.health,
                account,
                record_write,
                WriteFailure::QueueFull,
            );
            return false;
        };
        let worker_health = state.health.clone();
        let worker_account = account.clone();
        let job = CacheMessage::Work(Box::new(move |storage| {
            let _permits = permits;
            if read_health(&worker_health, &worker_account).unavailable {
                record_failure(
                    &worker_health,
                    &worker_account,
                    record_write,
                    WriteFailure::Storage,
                );
                return;
            }
            match operation(storage, &worker_account) {
                Err(CacheError::Storage | CacheError::Corrupt) => {
                    record_failure(
                        &worker_health,
                        &worker_account,
                        record_write,
                        WriteFailure::Storage,
                    );
                    tracing::warn!(event = "cache_partition_unavailable");
                }
                Err(CacheError::Quota) => record_failure(
                    &worker_health,
                    &worker_account,
                    record_write,
                    WriteFailure::Quota,
                ),
                _ => {}
            }
        }));
        if let Err(error) = queue.try_send(job) {
            let reason = match error {
                mpsc::error::TrySendError::Closed(_) => WriteFailure::Storage,
                mpsc::error::TrySendError::Full(_) => WriteFailure::QueueFull,
            };
            record_failure(&state.health, account, record_write, reason);
            return false;
        }
        true
    }
}

fn read_health(health: &WriteHealthByAccount, account: &AccountKey) -> WriteHealth {
    health.lock().map_or(
        WriteHealth {
            unavailable: true,
            ..Default::default()
        },
        |entries| entries.get(account).copied().unwrap_or_default(),
    )
}

fn record_failure(
    health: &WriteHealthByAccount,
    account: &AccountKey,
    record_write: bool,
    reason: WriteFailure,
) {
    if !record_write && !matches!(reason, WriteFailure::Storage) {
        return;
    }
    if let Ok(mut entries) = health.lock() {
        let health = entries.entry(account.clone()).or_default();
        if record_write {
            health.failed_writes = health.failed_writes.saturating_add(1);
            health.last_error = Some(reason);
            health.quota_blocked |= matches!(reason, WriteFailure::Quota);
        }
        health.unavailable |= matches!(reason, WriteFailure::Storage);
    }
}

impl Persistent {
    fn configured_folder(&self, account: &AccountKey, mailbox: &str) -> bool {
        self.folders
            .get(account)
            .is_some_and(|folders| folders.contains(canonical_mailbox(mailbox)))
    }

    fn enabled(&self, account: &AccountKey, mailbox: &str) -> bool {
        let mailbox = canonical_mailbox(mailbox);
        self.storage.is_some()
            && self.queue.as_ref().is_some_and(|queue| !queue.is_closed())
            && self.configured_folder(account, mailbox)
            && !read_health(&self.health, account).unavailable
            && self.generations.lock().is_ok_and(|epochs| {
                epochs
                    .get(&(account.reference_namespace(), mailbox.to_owned()))
                    .is_none_or(|epoch| !epoch.blocked)
            })
    }

    fn generation_matches(&self, namespace: &[u8; 32], mailbox: &str, validity: u32) -> bool {
        self.generations.lock().is_ok_and(|epochs| {
            epochs
                .get(&(*namespace, canonical_mailbox(mailbox).to_owned()))
                .is_none_or(|epoch| epoch.blocked || epoch.current == validity)
        })
    }

    fn account_current(
        &self,
        account: &AccountKey,
        folders: &[crate::cache::FolderStatus],
    ) -> bool {
        let namespace = account.reference_namespace();
        !read_health(&self.health, account).unavailable
            && self.generations.lock().is_ok_and(|epochs| {
                folders.iter().all(|folder| {
                    epochs
                        .get(&(namespace, folder.mailbox.clone()))
                        .is_none_or(|epoch| {
                            !epoch.blocked && folder.uid_validity == Some(epoch.current)
                        })
                })
            })
    }

    fn can_mark_clean(&self) -> bool {
        if self.cache_read_jobs.available_permits() != EXECUTION_SLOTS
            || self.content_jobs.available_permits() != CONTENT_PAYLOADS
        {
            return false;
        }
        self.health
            .lock()
            .is_ok_and(|keys| keys.values().all(|health| !health.unavailable))
            && self
                .generations
                .lock()
                .is_ok_and(|epochs| epochs.values().all(|epoch| !epoch.blocked))
    }
}

pub(super) struct HeaderObservation {
    pub uid: u32,
    pub bytes: Vec<u8>,
    pub complete: bool,
    pub index: Option<String>,
}

impl HeaderObservation {
    pub(super) fn new(
        uid: u32,
        bytes: &[u8],
        complete: bool,
        headers: &[mailparse::MailHeader<'_>],
    ) -> Self {
        let index = complete.then(|| index_text(headers)).flatten();
        Self {
            uid,
            bytes: bytes.to_vec(),
            complete,
            index,
        }
    }

    fn owned_bytes(&self) -> usize {
        self.bytes.capacity() + self.index.as_ref().map_or(0, String::capacity)
    }
}

impl MailService {
    pub(super) fn observe_fetch_error(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid_next: Option<u32>,
        uid: u32,
        error: MailError,
    ) -> MailError {
        // These callers have verified the selected generation. NOT_FOUND
        // means the command completed successfully without the target UID.
        if error.code == "NOT_FOUND" {
            self.cache_tombstone(mailbox, validity, uid_next, uid);
        }
        error
    }

    pub(super) fn persists(&self, mailbox: &str) -> bool {
        self.persistent.enabled_for_folder(&self.account, mailbox)
    }

    pub(super) async fn cache_before_write(
        &self,
        folders: &[&str],
    ) -> Option<cache_write_guard::DirtyWriteGuard> {
        let cache = self.persistent.full_storage(&self.account)?;
        let folders = folders
            .iter()
            .map(|folder| canonical_mailbox(folder))
            .filter(|folder| self.persistent.configured_folder(&self.account, folder))
            .collect::<std::collections::BTreeSet<_>>();
        let folders = folders.into_iter().collect::<Vec<_>>();
        self.write_guards
            .begin(cache, &self.account, &folders, WRITE_FENCE_WAIT)
            .await
    }

    pub(super) async fn cache_before_folder_tree_write(
        &self,
    ) -> Option<cache_write_guard::DirtyWriteGuard> {
        let folders = self
            .persistent
            .configured_folders(&self.account)
            .collect::<Vec<_>>();
        self.cache_before_write(&folders).await
    }

    pub(super) async fn cache_before_send(&self) -> Option<cache_write_guard::DirtyWriteGuard> {
        let cache = self.persistent.full_storage(&self.account)?;
        let (storage, account) = (cache.clone(), self.account.clone());
        let Some(folders) = self
            .lookup_sent(move || storage.confirmed_sent(&account))
            .await
        else {
            cache.disable_offline();
            return None;
        };
        let folders = folders.iter().map(String::as_str).collect::<Vec<_>>();
        self.cache_before_write(&folders).await
    }

    async fn lookup_sent(
        &self,
        lookup: impl FnOnce() -> std::result::Result<Vec<String>, CacheError> + Send + 'static,
    ) -> Option<Vec<String>> {
        let permit = self.persistent.cache_read_permit()?;
        let admission = runtime::read_admission();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _admission = admission;
            lookup()
        });
        tokio::time::timeout(WRITE_FENCE_WAIT, task)
            .await
            .ok()?
            .ok()?
            .ok()
    }

    pub(super) fn persist_mailboxes(&self, rows: &[Value]) {
        if self.persistent.full_storage(&self.account).is_none() {
            return;
        }
        let Some(time) = checked_at_ms() else { return };
        let rows = rows
            .iter()
            .filter(|row| {
                row["name"]
                    .as_str()
                    .is_some_and(|folder| self.persistent.configured_folder(&self.account, folder))
            })
            .filter_map(|row| serde_json::from_value::<MailboxEvidence>(row.clone()).ok())
            .map(|mut row| {
                row.name = canonical_mailbox(&row.name).into();
                row
            })
            .collect::<Vec<_>>();
        let bytes = rows.iter().try_fold(4096usize, |used, row| {
            Some(used.saturating_add(serde_json::to_vec(row).ok()?.len().saturating_mul(2)))
        });
        if let Some(bytes) = bytes {
            self.persistent
                .submit(&self.account, bytes, true, move |cache, account| {
                    cache.remember_mailboxes(account, &rows, time)
                });
        }
    }

    pub(super) fn persist_listing(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        items: Vec<ListingObservation>,
    ) {
        let Some(uid_validity) = validity.filter(|value| *value > 0) else {
            return;
        };
        if items.is_empty()
            || self
                .persistent
                .full_folder(&self.account, mailbox)
                .is_none()
        {
            return;
        }
        let Some(time) = checked_at_ms() else { return };
        let mailbox = canonical_mailbox(mailbox).to_owned();
        let bytes = 4096
            + mailbox.capacity()
            + items.capacity() * std::mem::size_of::<ListingObservation>()
            + items
                .iter()
                .map(|item| {
                    item.internal_date.as_ref().map_or(0, String::capacity)
                        + item.flags.as_ref().map_or(0, |flags| {
                            flags.capacity() * std::mem::size_of::<String>()
                                + flags.iter().map(String::capacity).sum::<usize>()
                        })
                })
                .sum::<usize>();
        self.persistent
            .submit(&self.account, bytes, true, move |cache, account| {
                for item in items {
                    cache.remember_listing(
                        CacheRef {
                            account,
                            mailbox: &mailbox,
                            uid_validity,
                            uid: item.uid,
                        },
                        ListingEvidence {
                            flags: item.flags.as_deref(),
                            internal_date: item.internal_date.as_deref(),
                            size: item.size,
                            observed_at_ms: time,
                        },
                    )?;
                }
                Ok(())
            });
    }

    pub(super) fn content_capture(&self, mailbox: &str) -> Option<ContentAdmission> {
        self.persistent.full_folder(&self.account, mailbox)?;
        let state = self.persistent.0.as_ref()?;
        let permit = match state.content_jobs.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                record_failure(&state.health, &self.account, true, WriteFailure::QueueFull);
                return None;
            }
        };
        let pending = state.content_pending.get(&self.account)?.clone();
        pending.fetch_add(1, Ordering::Release);
        Some(ContentAdmission {
            _permit: permit,
            pending,
        })
    }

    pub(super) fn cache_tombstone(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid_next: Option<u32>,
        uid: u32,
    ) {
        let Some(uid_validity) = validity.filter(|value| *value > 0) else {
            return;
        };
        // A UID at or above this connection's observed UIDNEXT could be
        // allocated while this absence is waiting for the cache worker.
        let Some(uid_next) = uid_next.filter(|next| uid != 0 && uid < *next) else {
            return;
        };
        let Some(storage) = self.persistent.full_folder(&self.account, mailbox) else {
            return;
        };
        let mailbox = canonical_mailbox(mailbox).to_owned();
        let storage = storage.clone();
        // Fence offline content before returning a confirmed live miss. The
        // worker records the durable tombstone under the same account/epoch.
        let Some(state) = &self.persistent.0 else {
            return;
        };
        let reference = (
            (self.account.reference_namespace(), mailbox.clone()),
            uid_validity,
            uid,
        );
        if !current_generation(&state.generations, &reference.0, uid_validity) {
            return;
        }
        let generations = state.generations.clone();
        let pending = state.pending_tombstones.clone();
        let inserted = pending.lock().is_ok_and(|mut pending| {
            (pending.len() < CACHE_JOBS * BATCH_READ_ITEMS || pending.contains(&reference)) && {
                pending.insert(reference.clone());
                true
            }
        });
        if !inserted
            || !self
                .persistent
                .submit(&self.account, 4096 + mailbox.capacity(), true, {
                    let reference = reference.clone();
                    move |cache, account| {
                        let result = if current_generation(&generations, &reference.0, uid_validity)
                        {
                            cache.tombstone_observed(
                                CacheRef {
                                    account,
                                    mailbox: &mailbox,
                                    uid_validity,
                                    uid,
                                },
                                uid_next,
                            )
                        } else {
                            Ok(())
                        };
                        if result.is_ok() {
                            if let Ok(mut pending) = pending.lock() {
                                pending.remove(&reference);
                            }
                        } else {
                            cache.disable_offline();
                        }
                        result
                    }
                })
        {
            if current_generation(&state.generations, &reference.0, uid_validity) {
                storage.disable_offline();
            } else if let Ok(mut pending) = state.pending_tombstones.lock() {
                pending.remove(&reference);
            }
        }
    }

    pub(super) fn capture_body(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
        record: parts::cached_body::Record,
        permit: ContentAdmission,
    ) {
        let retained = (record, permit);
        let Some(uid_validity) = validity.filter(|value| *value > 0) else {
            return;
        };
        let mailbox = canonical_mailbox(mailbox).to_owned();
        self.persistent.submit(
            &self.account,
            4096 + mailbox.capacity(),
            true,
            move |cache, account| {
                let payload = retained;
                let reference = CacheRef {
                    account,
                    mailbox: &mailbox,
                    uid_validity,
                    uid,
                };
                write_body(cache, reference, &payload.0)
            },
        );
    }

    pub(super) fn capture_attachment(
        &self,
        mailbox: &str,
        uid_validity: u32,
        uid: u32,
        index: usize,
        metadata: attachments::staging::ReadyMetadata,
        capture: attachments::staging::Capture,
    ) {
        let mailbox = canonical_mailbox(mailbox).to_owned();
        self.persistent.submit(
            &self.account,
            4096 + mailbox.capacity(),
            true,
            move |cache, account| {
                use std::io::Write;
                let payload = capture;
                let bytes = &payload.bytes;
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                if bytes.len() != metadata.decoded_size || digest != metadata.sha256 {
                    return Err(CacheError::Corrupt);
                }
                let reference = CacheRef {
                    account,
                    mailbox: &mailbox,
                    uid_validity,
                    uid,
                };
                let kind = ArtifactKey::Attachment {
                    original_index: index as u32,
                    mime_version: 1,
                };
                let mut stage = cache.begin_blob(
                    reference,
                    kind,
                    Some(bytes.len() as u64),
                    crate::config::DEFAULT_MAX_MESSAGE as u64,
                )?;
                stage.write_all(bytes)?;
                let stored = stage.publish(&|| false)?;
                if stored.sha256 != digest {
                    return Err(CacheError::Corrupt);
                }
                Ok(())
            },
        );
    }

    pub(super) fn capture_raw(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
        raw: Vec<u8>,
        permit: ContentAdmission,
    ) {
        let retained = (raw, permit);
        let Some(uid_validity) = validity.filter(|value| *value > 0) else {
            return;
        };
        let mailbox = canonical_mailbox(mailbox).to_owned();
        self.persistent.submit(
            &self.account,
            4096 + mailbox.capacity(),
            true,
            move |cache, account| {
                use std::io::Write;
                let (_permit, raw) = (retained.1, retained.0);
                let reference = CacheRef {
                    account,
                    mailbox: &mailbox,
                    uid_validity,
                    uid,
                };
                let mut stage = cache.begin_blob(
                    reference,
                    ArtifactKey::Raw,
                    Some(raw.len() as u64),
                    crate::config::DEFAULT_MAX_MESSAGE as u64,
                )?;
                stage.write_all(&raw)?;
                let published = stage.publish(&|| false)?;
                let Ok(parsed) = mime::parse(&raw) else {
                    return Ok(());
                };
                let mut states = Vec::new();
                if let Ok(parts) = parts::from_raw(&parsed) {
                    for part in parts
                        .manifest
                        .parts
                        .iter()
                        .filter(|part| part.is_attachment && part.downloadable)
                    {
                        states.push((
                            ArtifactKey::Attachment {
                                original_index: part.attachment_index as u32,
                                mime_version: 1,
                            },
                            ArtifactState::Derivable { raw: published.id },
                        ));
                    }
                }
                let mut record = parts::cached_body::Record::default();
                let mut remaining = parts::cached_body::LIMIT;
                let decoded = parsed
                    .decoded_display_leaves(&mut remaining, |text, html, reserve| {
                        record.push(text, html, reserve);
                        Ok(())
                    })
                    .is_ok();
                drop(parsed);
                drop(raw);
                if decoded {
                    states.push((
                        parts::cached_body::KEY,
                        ArtifactState::Derivable { raw: published.id },
                    ));
                    cache.record_artifact_states(reference, &states)?;
                    write_body(cache, reference, &record)?;
                } else {
                    cache.record_artifact_states(reference, &states)?;
                }
                Ok(())
            },
        );
    }

    pub(super) fn cache_artifact_state(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
        kind: ArtifactKey,
        value: ArtifactState,
    ) {
        let Some(uid_validity) = validity.filter(|value| *value > 0) else {
            return;
        };
        if self
            .persistent
            .full_folder(&self.account, mailbox)
            .is_none()
        {
            return;
        }
        let mailbox = canonical_mailbox(mailbox).to_owned();
        self.persistent.submit(
            &self.account,
            4096 + mailbox.capacity(),
            true,
            move |cache, account| {
                cache.record_artifact_state(
                    CacheRef {
                        account,
                        mailbox: &mailbox,
                        uid_validity,
                        uid,
                    },
                    kind,
                    value,
                )
            },
        );
    }

    pub(super) fn observe_mailbox(&self, mailbox: &str, state: &async_imap::types::Mailbox) {
        if !self.persists(mailbox) {
            return;
        }
        let Some(validity) = state.uid_validity.filter(|value| *value > 0) else {
            return;
        };
        let high_uid = state.uid_next.and_then(|next| next.checked_sub(1));
        let Some(time) = checked_at_ms() else { return };
        let mailbox = canonical_mailbox(mailbox).to_owned();
        let Some(cache) = &self.persistent.0 else {
            return;
        };
        let key = (self.account.reference_namespace(), mailbox.clone());
        let Some(transition) = observe_epoch(&cache.generations, &key, validity) else {
            return;
        };
        let worker_key = key.clone();
        let generations = cache.generations.clone();
        if !self.persistent.submit(
            &self.account,
            4096 + 2 * mailbox.capacity(),
            true,
            move |cache, account| {
                let observed = cache.observe_folder(account, &mailbox, validity, high_uid, time);
                if transition && observed.is_err() {
                    block_generation(&generations, &worker_key, validity);
                }
                observed
            },
        ) && transition
        {
            block_generation(&cache.generations, &key, validity);
        }
    }

    pub(super) fn persist_headers(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        observations: Vec<HeaderObservation>,
    ) {
        let Some(validity) = validity.filter(|validity| *validity > 0) else {
            return;
        };
        if observations.is_empty() || !self.persists(mailbox) {
            return;
        }
        let mailbox = canonical_mailbox(mailbox).to_owned();
        let full = self
            .persistent
            .full_folder(&self.account, &mailbox)
            .is_some();
        let bytes = 4096
            + mailbox.capacity()
            + observations.capacity() * std::mem::size_of::<HeaderObservation>()
            + observations
                .iter()
                .map(HeaderObservation::owned_bytes)
                .sum::<usize>();
        self.persistent
            .submit(&self.account, bytes, true, move |cache, account| {
                for item in observations {
                    let reference = CacheRef {
                        account,
                        mailbox: &mailbox,
                        uid_validity: validity,
                        uid: item.uid,
                    };
                    cache.remember_headers(
                        reference,
                        item.complete,
                        &item.bytes,
                        item.index.as_deref(),
                    )?;
                    if full
                        && item.complete
                        && let Ok(headers) = mime::headers(&item.bytes)
                        && let Ok(ids) = conversation::thread_index_ids(&headers)
                    {
                        let ids = ids
                            .iter()
                            .map(|(role, id)| ThreadId { role: *role, id })
                            .collect::<Vec<_>>();
                        cache.remember_thread_ids(
                            reference,
                            conversation::THREAD_INDEX_VERSION,
                            &item.bytes,
                            &ids,
                        )?;
                    }
                }
                Ok(())
            });
    }

    pub(super) fn persist_manifest(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
        manifest: &parts::PartManifest,
    ) {
        let Some(validity) = validity.filter(|validity| *validity > 0) else {
            return;
        };
        if !self.persists(mailbox) {
            return;
        }
        let Some(bytes) = encode_manifest(manifest) else {
            return;
        };
        let mailbox = canonical_mailbox(mailbox).to_owned();
        let expected = self
            .persistent
            .full_folder(&self.account, &mailbox)
            .map(|_| {
                manifest
                    .parts
                    .iter()
                    .filter(|part| part.is_attachment)
                    .map(|part| {
                        (
                            ArtifactKey::Attachment {
                                original_index: part.attachment_index as u32,
                                mime_version: 1,
                            },
                            if part.downloadable {
                                ArtifactState::Missing
                            } else {
                                ArtifactState::Unsupported(Omission::UnsupportedEncoding)
                            },
                        )
                    })
                    .collect::<Vec<_>>()
            });
        let expected_bytes = expected.as_ref().map_or(0, |parts| {
            parts.capacity() * std::mem::size_of::<(ArtifactKey, ArtifactState)>()
        });
        self.persistent.submit(
            &self.account,
            4096 + mailbox.capacity() + bytes.capacity() + expected_bytes,
            true,
            move |cache, account| {
                let reference = CacheRef {
                    account,
                    mailbox: &mailbox,
                    uid_validity: validity,
                    uid,
                };
                if let Some(expected) = expected {
                    cache.remember_full_manifest(reference, &bytes, &expected)
                } else {
                    cache.remember_parts(reference, &bytes)
                }
            },
        );
    }

    pub(super) fn persist_structured_manifest(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
        manifest: &parts::PartManifest,
    ) {
        self.persist_manifest(mailbox, validity, uid, manifest);
        if self.persists(mailbox)
            && let Ok(headers) = mime::headers(manifest.root_headers())
        {
            self.persist_headers(
                mailbox,
                validity,
                vec![HeaderObservation::new(
                    uid,
                    manifest.root_headers(),
                    true,
                    &headers,
                )],
            );
        }
    }

    pub(super) fn persist_raw(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
        raw: &[u8],
        part: &mime::Part<'_>,
    ) {
        if !self.persists(mailbox) {
            return;
        }
        let header = &raw[..raw.len() - part.encoded_body().len()];
        self.persist_headers(
            mailbox,
            validity,
            vec![HeaderObservation::new(uid, header, true, &part.headers)],
        );
        if let Ok(parsed) = parts::from_raw(part) {
            self.persist_manifest(mailbox, validity, uid, &parsed.manifest);
        }
    }
}

fn write_body(
    cache: &Arc<Cache>,
    reference: CacheRef<'_>,
    record: &parts::cached_body::Record,
) -> std::result::Result<(), CacheError> {
    let mut stage = cache.begin_blob(
        reference,
        parts::cached_body::KEY,
        Some(record.bytes() as u64),
        parts::cached_body::LIMIT as u64,
    )?;
    let metadata = record.encode(&mut stage)?;
    let blob = stage.publish_body(&metadata, &|| false)?;
    if let Ok(rendered) = record.display(crate::html::HtmlDisplay::Legacy) {
        cache.remember_full_body_index(
            reference,
            parts::cached_body::KEY,
            blob.id,
            &rendered.text,
            &|| false,
        )?;
    }
    Ok(())
}

pub(super) fn coverage(
    cache: &Cache,
    account: &AccountKey,
    mailbox: &str,
    validity: u32,
) -> std::result::Result<Value, CacheError> {
    use crate::cache::ArtifactLayer;
    let mut value = json!({});
    for (name, layer) in [
        ("raw", ArtifactLayer::Raw),
        ("full_body", parts::cached_body::LAYER),
        (
            "attachments",
            ArtifactLayer::Attachments { mime_version: 1 },
        ),
    ] {
        let layer = cache.layer_coverage(account, mailbox, validity, layer)?;
        value[name] = json!({
            "materialized":layer.materialized,"materialized_bytes":layer.materialized_bytes,
            "derivable_from_raw":layer.derivable_from_raw,"missing":layer.missing,
            "omitted":layer.omitted,"unsupported":layer.unsupported
        });
    }
    Ok(value)
}

fn observe_epoch(generations: &Generations, key: &GenerationKey, validity: u32) -> Option<bool> {
    let mut epochs = generations.lock().ok()?;
    match epochs.entry(key.clone()) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Generation {
                current: validity,
                blocked: false,
            });
            Some(true)
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            let epoch = entry.get_mut();
            // A delayed EXAMINE can complete on another connection after a
            // newer generation has already been observed (RFC 3501 §2.3.1.1).
            if epoch.blocked || validity < epoch.current {
                return None;
            }
            let transition = validity > epoch.current;
            epoch.current = validity;
            Some(transition)
        }
    }
}

fn current_generation(generations: &Generations, key: &GenerationKey, validity: u32) -> bool {
    generations.lock().is_ok_and(|epochs| {
        epochs
            .get(key)
            .is_none_or(|epoch| !epoch.blocked && epoch.current == validity)
    })
}

fn block_generation(generations: &Generations, key: &GenerationKey, validity: u32) {
    if let Ok(mut epochs) = generations.lock()
        && let Some(epoch) = epochs.get_mut(key)
        && epoch.current == validity
    {
        epoch.blocked = true;
    }
}

pub(super) fn canonical_mailbox(mailbox: &str) -> &str {
    if mailbox.eq_ignore_ascii_case("INBOX") {
        "INBOX"
    } else {
        mailbox
    }
}

fn checked_at_ms() -> Option<i64> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()
}

pub(super) struct ListingObservation {
    pub(super) uid: u32,
    pub(super) flags: Option<Vec<String>>,
    pub(super) internal_date: Option<String>,
    pub(super) size: Option<u32>,
}

pub(super) fn index_text(headers: &[mailparse::MailHeader<'_>]) -> Option<String> {
    let mut text = String::new();
    for header in headers {
        if [b"Subject".as_slice(), b"From", b"To", b"Cc", b"Message-ID"]
            .iter()
            .any(|field| header.get_key_raw().eq_ignore_ascii_case(field))
        {
            let value = header.get_value();
            if text.len().saturating_add(value.len()) >= INDEX_BYTES {
                return None;
            }
            text.push_str(&value);
            text.push('\n');
        }
    }
    (!text.is_empty()).then_some(text)
}

pub(super) fn encode_manifest(manifest: &parts::PartManifest) -> Option<Vec<u8>> {
    use serde::Serialize;
    #[derive(Serialize)]
    struct Record<'a> {
        version: u8,
        parts: Vec<Part<'a>>,
    }
    #[derive(Serialize)]
    struct Part<'a> {
        section: &'a [u32],
        attachment_index: usize,
        attachment_ordinal: Option<usize>,
        is_attachment: bool,
        content_type: &'a str,
        transfer_encoding: &'a str,
        encoded_size: Option<u32>,
        disposition: Option<&'a str>,
        filename: Option<&'a str>,
        content_id: Option<&'a str>,
        downloadable: bool,
    }
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > PARTS_BYTES - self.0.len() {
                return Err(std::io::Error::other("Cache manifest limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let parts = manifest
        .parts
        .iter()
        .map(|part| Part {
            section: part.section.numbers(),
            attachment_index: part.attachment_index,
            attachment_ordinal: part.attachment_ordinal,
            is_attachment: part.is_attachment,
            content_type: &part.content_type,
            transfer_encoding: &part.transfer_encoding,
            encoded_size: part.encoded_size,
            disposition: part.disposition.as_deref(),
            filename: part.filename.as_deref(),
            content_id: part.content_id.as_deref(),
            downloadable: part.downloadable,
        })
        .collect();
    let mut output = Bounded(Vec::new());
    serde_json::to_writer(&mut output, &Record { version: 1, parts }).ok()?;
    Some(output.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn full_cache_fixture(
        root: &std::path::Path,
    ) -> (
        MailRuntime,
        PreparedCache,
        Arc<crate::accounts::AccountCatalog>,
        crate::accounts::CacheRun,
    ) {
        use crate::{accounts, config};
        let mut config = (*config::fixture()).clone();
        config.account_registry = Some(root.join("accounts.sqlite"));
        let set = config::ValidatedAccountSet::legacy(Arc::new(config));
        let path = root.join("cache.json");
        std::fs::write(&path, br#"{"version":1,"mode":"full","directory":"cache","max_bytes":8388608,"accounts":[{"account_id":"default","mailboxes":["INBOX"],"max_bytes":8388608}]}"#).unwrap();
        let settings = config::load_cache_file(&path, &set).unwrap();
        let catalog = accounts::open_catalog(set).unwrap();
        let witness = accounts::CacheRun::begin(catalog.config()).unwrap();
        let cache =
            MailRuntime::prepare_cache_for_run(&catalog, settings, Some(witness.identity()))
                .unwrap();
        let runtime = MailRuntime::with_cache(
            catalog.clone(),
            Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap()),
            cache.clone(),
        )
        .unwrap();
        (runtime, cache, catalog, witness)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_sent_lookup_holds_write_and_cache_permits_until_worker_exits() {
        use crate::mail::scheduler::WorkClass;
        use std::time::Duration;

        let root = tempfile::tempdir().unwrap();
        let (runtime, cache, catalog, _witness) = full_cache_fixture(root.path());
        let account = catalog.write_account("owner").unwrap();
        let service = runtime.service_for(&account.key).unwrap();
        let (unblock, blocked) = std::sync::mpsc::channel();
        let (started, entering) = oneshot::channel();
        runtime
            .run_write(&account, async {
                let result = service
                    .lookup_sent(move || {
                        let _ = started.send(());
                        blocked.recv().unwrap();
                        Ok(Vec::new())
                    })
                    .await;
                assert!(result.is_none());
                Ok(())
            })
            .await
            .unwrap();
        entering.await.unwrap();
        let cache_reads = &cache.0.as_ref().unwrap().cache_read_jobs;
        assert_eq!(cache_reads.available_permits(), EXECUTION_SLOTS - 1);
        let namespace = account.key.reference_namespace();
        let token = &account.cancellation;
        let mut permits = Vec::new();
        for _ in 1..EXECUTION_SLOTS {
            permits.push(
                runtime
                    .scheduler
                    .acquire(namespace, WorkClass::Foreground, token)
                    .await
                    .unwrap(),
            );
        }
        let waiting = runtime
            .scheduler
            .acquire(namespace, WorkClass::Foreground, token);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), waiting)
                .await
                .is_err()
        );
        unblock.send(()).unwrap();
        let recovered = tokio::time::timeout(
            Duration::from_secs(2),
            runtime
                .scheduler
                .acquire(namespace, WorkClass::Foreground, token),
        )
        .await
        .unwrap()
        .unwrap();
        drop(recovered);
        drop(permits);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cache_reads.available_permits() != EXECUTION_SLOTS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        runtime.finish_cache(cache).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delayed_old_generation_absence_preserves_new_offline_copy() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, cache, catalog, _witness) = full_cache_fixture(root.path());
        let account = catalog.read_account("owner", None).unwrap();
        let service = runtime.service_for(&account.key).unwrap();
        let observe = |validity| {
            service.observe_mailbox(
                "INBOX",
                &async_imap::types::Mailbox {
                    uid_validity: Some(validity),
                    uid_next: Some(8),
                    ..Default::default()
                },
            )
        };
        observe(9);
        assert_eq!(
            cache.status(&account.key, "default").await["storage"]["folders"][0]["uid_validity"],
            9
        );

        let (unblock, blocked) = std::sync::mpsc::channel();
        assert!(cache.submit(&account.key, 4096, false, move |_, _| {
            blocked.recv().unwrap();
            Ok(())
        }));
        let missing = || MailError {
            code: "NOT_FOUND",
            message: "The referenced email is no longer available.",
            retryable: false,
            details: None,
        };
        service.observe_fetch_error("INBOX", Some(9), Some(8), 7, missing());
        observe(10);
        service.observe_fetch_error("INBOX", Some(9), Some(8), 7, missing());
        unblock.send(()).unwrap();

        let header = b"Subject: new generation\r\n\r\n";
        let parsed = mime::headers(header).unwrap();
        service.persist_headers(
            "INBOX",
            Some(10),
            vec![HeaderObservation::new(7, header, true, &parsed)],
        );
        assert_eq!(
            cache.status(&account.key, "default").await["storage"]["folders"][0]["uid_validity"],
            10
        );
        let storage = cache.full_storage(&account.key).unwrap();
        // The UIDVALIDITY transition itself leaves the scope dirty until
        // reconciliation. Simulate that verified step for the new generation.
        let stamp = storage.dirty_stamp(&account.key, "INBOX").unwrap();
        storage.clear_dirty_if_verified(&stamp, 10).unwrap();
        let reference = CacheRef {
            account: &account.key,
            mailbox: "INBOX",
            uid_validity: 10,
            uid: 7,
        };
        assert_eq!(
            storage
                .cached_message(reference)
                .unwrap()
                .unwrap()
                .header
                .as_deref(),
            Some(header.as_slice())
        );
        assert!(matches!(
            storage.cached_message(CacheRef {
                uid_validity: 9,
                ..reference
            }),
            Err(CacheError::NeedsVerification)
        ));
        assert!(
            cache
                .0
                .as_ref()
                .unwrap()
                .pending_tombstones
                .lock()
                .unwrap()
                .is_empty()
        );
        runtime.finish_cache(cache).await;
    }

    #[test]
    fn first_observations_arriving_out_of_order_keep_the_newest_generation() {
        let generations = Generations::default();
        let key = ([1; 32], "INBOX".into());
        let (old_received, wait_old) = std::sync::mpsc::sync_channel(0);
        let (new_applied, wait_new) = std::sync::mpsc::sync_channel(0);
        std::thread::scope(|threads| {
            let old_generations = &generations;
            let old_key = &key;
            threads.spawn(move || {
                old_received.send(()).unwrap();
                wait_new.recv().unwrap();
                assert_eq!(observe_epoch(old_generations, old_key, 9), None);
            });
            wait_old.recv().unwrap();
            let newest = observe_epoch(&generations, &key, 10);
            new_applied.send(()).unwrap();
            assert_eq!(newest, Some(true));
        });
        assert_eq!(observe_epoch(&generations, &key, 10), Some(false));
        assert_eq!(generations.lock().unwrap()[&key].current, 10);
        assert!(!current_generation(&generations, &key, 9));
        assert!(current_generation(&generations, &key, 10));
    }
}
