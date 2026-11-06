//! Admission and provenance for write-through metadata observations.
use super::*;
use crate::{
    cache::{
        AccountScope, ArtifactKey, ArtifactState, Cache, CacheError, ListingEvidence, ListingStamp,
        MailboxEvidence, Omission, Ref as CacheRef, ThreadId,
    },
    config::{CacheMode, CacheSettings, SyncPolicy, SyncProtocol},
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
pub(super) const WRITE_FENCE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
// Two retained payloads plus one 32 MiB conversion buffer on the single FIFO
// worker. The worker releases raw before constructing the full-text index.
const CONTENT_PAYLOADS: usize = 2;

#[derive(Clone, Default)]
pub struct PreparedCache(Option<Arc<Persistent>>);

struct Persistent {
    mode: CacheMode,
    sync: SyncPolicy,
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
    sync_absence: Mutex<HashMap<GenerationKey, u32>>,
    sync_epochs: Mutex<HashMap<GenerationKey, u32>>,
    delta_invalidations: Mutex<HashMap<GenerationKey, (u32, DeltaInvalidation)>>,
    qresync_fences: Arc<Mutex<QresyncFences>>,
}

#[derive(Default)]
struct QresyncFences {
    revision: u64,
    scopes: HashMap<GenerationKey, QresyncScope>,
}

struct QresyncScope {
    revision: u64,
    recovery: Option<QresyncRecovery>,
}

#[derive(Clone, Copy)]
pub(super) struct QresyncRecovery {
    pub(super) validity: u32,
    pub(super) auditing: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum DeltaInvalidation {
    Seed,
    All,
}

type CacheJob = Box<dyn FnOnce(&Arc<Cache>) + Send>;
enum CacheMessage {
    Work(CacheJob),
    Barrier(oneshot::Sender<()>),
    Finish(
        Arc<Persistent>,
        Option<tokio::time::Instant>,
        oneshot::Sender<bool>,
    ),
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
            sync,
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
                                CacheMessage::Barrier(done) => {
                                    let _ = done.send(());
                                }
                                CacheMessage::Finish(state, deadline, done) => {
                                    let publish = || {
                                        !done.is_closed()
                                            && deadline.is_none_or(|deadline| {
                                                tokio::time::Instant::now() < deadline
                                            })
                                    };
                                    let finished = publish()
                                        && (!state.can_mark_clean()
                                            || worker.mark_clean_if(&publish).is_ok());
                                    drop(state);
                                    drop(worker);
                                    let _ = done.send(finished);
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
            sync,
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
            sync_absence: Mutex::default(),
            sync_epochs: Mutex::default(),
            delta_invalidations: Mutex::default(),
            qresync_fences: Arc::default(),
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
        self.sync_current(account, mailbox, validity, uid)
            && self.0.as_ref().is_some_and(|state| {
                let key = (
                    account.reference_namespace(),
                    canonical_mailbox(mailbox).into(),
                );
                state
                    .sync_absence
                    .lock()
                    .is_ok_and(|fences| fences.get(&key) != Some(&validity))
                    && state.qresync_fences.lock().is_ok_and(|fences| {
                        fences
                            .scopes
                            .get(&key)
                            .and_then(|scope| scope.recovery)
                            .is_none_or(|proof| proof.validity < validity)
                    })
            })
    }

    pub(super) fn sync_current(
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

    pub(super) fn sync_policy(&self) -> Option<SyncPolicy> {
        let state = self.0.as_ref()?;
        (state.mode == CacheMode::Full).then_some(state.sync)
    }

    pub(super) fn sync_observe(&self, account: &AccountKey, mailbox: &str, validity: u32) -> bool {
        let Some(state) = &self.0 else { return false };
        if !state.enabled(account, mailbox) {
            return false;
        }
        let key = (account.reference_namespace(), mailbox.into());
        let Some(transition) = observe_epoch(&state.generations, &key, validity) else {
            return false;
        };
        !transition
            || state
                .sync_epochs
                .lock()
                .map(|mut pending| {
                    let epoch = pending.entry(key).or_insert(validity);
                    *epoch = (*epoch).max(validity);
                })
                .is_ok()
    }

    pub(super) fn sync_commit_epoch(&self, account: &AccountKey, mailbox: &str, validity: u32) {
        let Some(state) = &self.0 else { return };
        let key = (account.reference_namespace(), mailbox.into());
        if let Ok(mut pending) = state.sync_epochs.lock()
            && pending.get(&key).is_some_and(|epoch| *epoch <= validity)
        {
            pending.remove(&key);
        }
        if let Ok(mut absences) = state.sync_absence.lock()
            && absences.get(&key).is_some_and(|epoch| *epoch < validity)
        {
            absences.remove(&key);
        }
        if let Ok(mut invalidations) = state.delta_invalidations.lock()
            && invalidations
                .get(&key)
                .is_some_and(|(epoch, _)| *epoch < validity)
        {
            invalidations.remove(&key);
        }
        if let Ok(mut fences) = state.qresync_fences.lock()
            && let Some(scope) = fences.scopes.get_mut(&key)
            && scope
                .recovery
                .is_some_and(|proof| proof.validity < validity)
        {
            scope.recovery = None;
        }
    }

    pub(super) fn write_through_revision(&self) -> u64 {
        self.0
            .as_ref()
            .and_then(|state| state.qresync_fences.lock().ok())
            .map_or(0, |fences| fences.revision)
    }

    pub(super) async fn sync_write_barrier(&self) -> std::result::Result<(), CacheError> {
        let queue = self
            .0
            .as_ref()
            .and_then(|state| state.queue.as_ref())
            .ok_or(CacheError::Storage)?;
        let (done, wait) = oneshot::channel();
        queue
            .send(CacheMessage::Barrier(done))
            .await
            .map_err(|_| CacheError::Storage)?;
        wait.await.map_err(|_| CacheError::Storage)
    }

    pub(super) fn qresync_recovery(
        &self,
        account: &AccountKey,
        mailbox: &str,
    ) -> std::result::Result<Option<QresyncRecovery>, CacheError> {
        let Some(state) = &self.0 else {
            return Ok(None);
        };
        let key = (account.reference_namespace(), mailbox.into());
        Ok(state
            .qresync_fences
            .lock()
            .map_err(|_| CacheError::Storage)?
            .scopes
            .get(&key)
            .and_then(|scope| scope.recovery))
    }

    pub(super) fn fence_qresync_deletion(
        &self,
        account: &AccountKey,
        mailbox: &str,
        validity: u32,
    ) -> bool {
        let Some(state) = &self.0 else { return false };
        let key = (account.reference_namespace(), mailbox.into());
        current_generation(&state.generations, &key, validity)
            && state.qresync_fences.lock().is_ok_and(|mut fences| {
                if fences
                    .scopes
                    .get(&key)
                    .and_then(|scope| scope.recovery)
                    .is_some_and(|proof| proof.validity > validity)
                {
                    return false;
                }
                fences.revision += 1;
                let revision = fences.revision;
                fences.scopes.insert(
                    key,
                    QresyncScope {
                        revision,
                        recovery: Some(QresyncRecovery {
                            validity,
                            auditing: false,
                        }),
                    },
                );
                true
            })
    }

    pub(super) fn start_qresync_recovery(
        &self,
        account: &AccountKey,
        mailbox: &str,
        validity: u32,
    ) -> std::result::Result<(), CacheError> {
        let state = self.0.as_ref().ok_or(CacheError::Storage)?;
        let key = (account.reference_namespace(), mailbox.into());
        let mut fences = state
            .qresync_fences
            .lock()
            .map_err(|_| CacheError::Storage)?;
        if let Some(proof) = fences
            .scopes
            .get_mut(&key)
            .and_then(|scope| scope.recovery.as_mut())
            .filter(|proof| proof.validity == validity)
        {
            proof.auditing = true;
        }
        Ok(())
    }

    pub(super) fn finish_qresync_recovery(
        &self,
        account: &AccountKey,
        mailbox: &str,
        validity: u32,
        auditing: bool,
    ) {
        if let Some(state) = &self.0
            && let Ok(mut fences) = state.qresync_fences.lock()
        {
            let key = (account.reference_namespace(), mailbox.into());
            if let Some(scope) = fences.scopes.get_mut(&key)
                && scope
                    .recovery
                    .is_some_and(|proof| proof.validity == validity && proof.auditing == auditing)
            {
                scope.recovery = None;
            }
        }
    }

    pub(super) fn delta_invalidation_pending(
        &self,
        account: &AccountKey,
        mailbox: &str,
        validity: u32,
    ) -> Option<DeltaInvalidation> {
        let Some(state) = &self.0 else {
            return Some(DeltaInvalidation::All);
        };
        let key = (account.reference_namespace(), mailbox.into());
        state
            .delta_invalidations
            .lock()
            .map(|pending| {
                pending
                    .get(&key)
                    .filter(|(epoch, _)| *epoch >= validity)
                    .map(|(_, invalidation)| *invalidation)
            })
            .unwrap_or(Some(DeltaInvalidation::All))
    }

    pub(super) fn fence_delta_invalidation(
        &self,
        account: &AccountKey,
        mailbox: &str,
        validity: u32,
        invalidation: DeltaInvalidation,
    ) -> bool {
        let Some(state) = &self.0 else { return false };
        let key = (account.reference_namespace(), mailbox.into());
        current_generation(&state.generations, &key, validity)
            && state.delta_invalidations.lock().is_ok_and(|mut pending| {
                let proof = pending.entry(key).or_insert((validity, invalidation));
                *proof = (*proof).max((validity, invalidation));
                proof.0 == validity
            })
    }

    pub(super) fn commit_delta_invalidation(
        &self,
        account: &AccountKey,
        mailbox: &str,
        validity: u32,
        applied: DeltaInvalidation,
    ) {
        if let Some(state) = &self.0
            && let Ok(mut invalidations) = state.delta_invalidations.lock()
        {
            let key = (account.reference_namespace(), mailbox.into());
            if invalidations
                .get(&key)
                .is_some_and(|proof| *proof <= (validity, applied))
            {
                invalidations.remove(&key);
            }
        }
    }

    // Keep the whole scope offline while an authenticated absence proof is
    // waiting for SQLite. A failed commit preserves the fence until a complete
    // reconciliation has applied the evidence for that generation.
    pub(super) fn sync_fence_absence(
        &self,
        account: &AccountKey,
        mailbox: &str,
        validity: u32,
    ) -> Option<bool> {
        let state = self.0.as_ref()?;
        let key = (account.reference_namespace(), mailbox.into());
        let mut fences = state.sync_absence.lock().ok()?;
        match fences.get(&key).copied() {
            Some(previous) if previous > validity => None,
            Some(previous) if previous == validity => Some(false),
            _ => {
                fences.insert(key, validity);
                Some(true)
            }
        }
    }

    pub(super) fn sync_finish_reconcile(&self, account: &AccountKey, mailbox: &str, validity: u32) {
        if let Some(state) = &self.0
            && let Ok(mut fences) = state.sync_absence.lock()
        {
            let key = (account.reference_namespace(), mailbox.into());
            if fences.get(&key).is_some_and(|fence| *fence <= validity) {
                fences.remove(&key);
            }
        }
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
        self.finish_with_deadline(None).await;
    }

    pub(super) async fn finish_before(self, deadline: tokio::time::Instant) -> bool {
        tokio::time::timeout_at(deadline, self.finish_with_deadline(Some(deadline)))
            .await
            .unwrap_or(false)
    }

    async fn finish_with_deadline(self, deadline: Option<tokio::time::Instant>) -> bool {
        let Some((state, queue)) = self
            .0
            .as_ref()
            .and_then(|state| state.queue.as_ref().map(|queue| (state, queue)))
        else {
            return true;
        };
        state.cache_read_jobs.close();
        let (done, wait) = oneshot::channel();
        queue
            .send(CacheMessage::Finish(state.clone(), deadline, done))
            .await
            .is_ok()
            && wait.await.unwrap_or(false)
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
        let sync = state.sync;
        let admission = runtime::read_admission();
        let (done, wait) = oneshot::channel();
        let submitted = self.submit(account, 4096, false, move |cache, account| {
            let _admission = admission;
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
                    let sync_folders = if full {
                        status
                            .folders
                            .iter()
                            .map(|folder| {
                                sync_folder_status(cache, account, &folder.mailbox, sync.protocol)
                            })
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    let _ =
                        done.send((status, folders, read_health(&health, account), sync_folders));
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
            result.filter(|(status, _, _, _)| state.account_current(account, &status.folders));
        let mut sync_folders = None;
        let mut value = match result {
            Some((status, folders, health, mut progress)) => {
                for (folder, progress) in status.folders.iter().zip(&mut progress) {
                    if folder.uid_validity.is_some_and(|validity| {
                        self.delta_invalidation_pending(account, &folder.mailbox, validity)
                            == Some(DeltaInvalidation::All)
                    }) {
                        progress["condstore"] = Value::Null;
                        progress["qresync"] = Value::Null;
                    }
                    if folder.uid_validity.is_some_and(|validity| {
                        !self.offline_current(account, &folder.mailbox, validity, None)
                    }) {
                        progress["progress"] = Value::Null;
                        progress["condstore"] = Value::Null;
                        progress["qresync"] = Value::Null;
                        progress["error"] = json!("verification_pending");
                    }
                }
                sync_folders = Some(progress);
                json!({
                "account_id":alias,"mode":mode,"status":"ready",
                "write_health":health,
                "storage":{
                    "max_bytes":status.max_bytes, "used_bytes":status.used_bytes,
                    "global_max_bytes":status.global_max_bytes, "startup_unclean":status.startup_unclean,
                    "folders":folders
                }
                })
            }
            None => {
                json!({"account_id":alias,"mode":mode,"status":"unavailable","storage":null,
                    "write_health":read_health(&state.health, account)})
            }
        };
        if full {
            value["sync"] = json!({
                "enabled":sync.enabled,
                "interval_seconds":sync.interval_seconds,
                "reconcile_interval_seconds":sync.reconcile_interval_seconds,
                "protocol":sync.protocol,
                "folders":sync_folders
            });
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

    fn submit_evidence(
        &self,
        account: &AccountKey,
        mailbox: String,
        bytes: usize,
        operation: impl FnOnce(&Arc<Cache>, &AccountKey) -> std::result::Result<(), CacheError>
        + Send
        + 'static,
    ) {
        let Some(state) = &self.0 else { return };
        let fences = state.qresync_fences.clone();
        let key = (account.reference_namespace(), mailbox);
        let revision = runtime::write_through_revision();
        let current = move || {
            fences.lock().is_ok_and(|fences| {
                fences
                    .scopes
                    .get(&key)
                    .is_none_or(|scope| revision >= scope.revision)
            })
        };
        if current() {
            self.submit(account, bytes, true, move |cache, account| {
                if current() {
                    operation(cache, account)
                } else {
                    Ok(())
                }
            });
        }
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

fn sync_folder_status(
    cache: &Cache,
    account: &AccountKey,
    mailbox: &str,
    protocol: SyncProtocol,
) -> Value {
    use crate::cache::sync_state::{DeltaLane, SyncError};
    let status = cache
        .sync_scope_status(account, mailbox)
        .and_then(|progress| {
            let (flags, deletions) = if protocol == SyncProtocol::Auto {
                (
                    cache.delta_status(account, mailbox, DeltaLane::Flags)?,
                    cache.delta_status(account, mailbox, DeltaLane::FlagsAndExpunges)?,
                )
            } else {
                (None, None)
            };
            Ok((progress, flags, deletions))
        });
    match status {
        Ok((progress, flags, deletions)) => {
            json!({"mailbox":mailbox,"progress":progress,"condstore":flags,"qresync":deletions,"error":null})
        }
        Err(error) => {
            let error = if matches!(error, SyncError::Cache(CacheError::Quota)) {
                "quota"
            } else {
                "storage"
            };
            json!({"mailbox":mailbox,"progress":null,"condstore":null,"qresync":null,"error":error})
        }
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
                .delta_invalidations
                .lock()
                .is_ok_and(|scopes| scopes.is_empty())
            && self
                .qresync_fences
                .lock()
                .is_ok_and(|fences| fences.scopes.values().all(|scope| scope.recovery.is_none()))
            && self
                .sync_absence
                .lock()
                .is_ok_and(|fences| fences.is_empty())
            && self
                .sync_epochs
                .lock()
                .is_ok_and(|epochs| epochs.is_empty())
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
            .read_cache_evidence(move || storage.confirmed_sent(&account))
            .await
        else {
            cache.disable_offline();
            return None;
        };
        let folders = folders.iter().map(String::as_str).collect::<Vec<_>>();
        self.cache_before_write(&folders).await
    }

    async fn read_cache_evidence<T: Send + 'static>(
        &self,
        lookup: impl FnOnce() -> std::result::Result<T, CacheError> + Send + 'static,
    ) -> Option<T> {
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

    pub(super) async fn capture_listing_stamp(
        &self,
        mailbox: &str,
        validity: Option<u32>,
    ) -> Option<ListingStamp> {
        let uid_validity = validity.filter(|value| *value > 0)?;
        let mailbox = canonical_mailbox(mailbox);
        let cache = self.persistent.full_folder(&self.account, mailbox)?.clone();
        let (account, mailbox) = (self.account.clone(), mailbox.to_owned());
        self.read_cache_evidence(move || cache.listing_stamp(&account, &mailbox, uid_validity))
            .await
    }

    pub(super) fn persist_listing(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        items: Vec<ListingObservation>,
        stamp: Option<ListingStamp>,
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
        self.persistent.submit_evidence(
            &self.account,
            mailbox.clone(),
            bytes,
            move |cache, account| {
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
                        stamp.as_ref(),
                    )?;
                }
                Ok(())
            },
        );
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
        self.persistent.submit_evidence(
            &self.account,
            mailbox.clone(),
            4096 + mailbox.capacity(),
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
        self.persistent.submit_evidence(
            &self.account,
            mailbox.clone(),
            4096 + mailbox.capacity(),
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
        self.persistent.submit_evidence(
            &self.account,
            mailbox.clone(),
            4096 + mailbox.capacity(),
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
        self.persistent.submit_evidence(
            &self.account,
            mailbox.clone(),
            4096 + mailbox.capacity(),
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
        self.persistent.submit_evidence(
            &self.account,
            mailbox.clone(),
            bytes,
            move |cache, account| {
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
            },
        );
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
        self.persistent.submit_evidence(
            &self.account,
            mailbox.clone(),
            4096 + mailbox.capacity() + bytes.capacity() + expected_bytes,
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

pub(super) fn checked_at_ms() -> Option<i64> {
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

    async fn summary_flags(
        service: &MailService,
        flags: &str,
        pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    ) -> Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (client, server) = tokio::io::duplex(2048);
        let flags = flags.to_owned();
        let peer = tokio::spawn(async move {
            let mut stream = BufReader::new(server);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert!(command.starts_with("LOGIN "));
            stream
                .write_all(format!("{tag} OK login\r\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert_eq!(
                command,
                "UID FETCH 7 (UID FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER]<0.1048576>)"
            );
            if let Some((entered, release)) = pause {
                entered.send(()).unwrap();
                release.await.unwrap();
            }
            let header = "Subject: synthetic\r\n\r\n";
            stream.write_all(format!(
                "* 1 FETCH (UID 7 FLAGS ({flags}) RFC822.SIZE 100 INTERNALDATE \" 7-Feb-2025 09:00:00 +0000\" BODY[HEADER]<0> {{{}}}\r\n{header})\r\n{tag} OK done\r\n",
                header.len(),
            ).as_bytes()).await.unwrap();
        });
        let mut session = async_imap::Client::new(client)
            .login("synthetic", "test-only")
            .await
            .unwrap();
        let rows = service
            .fetch_summaries(&mut session, "INBOX", Some(9), Some(8), &[7])
            .await
            .unwrap();
        peer.await.unwrap();
        rows[0]["flags"].clone()
    }

    fn verify_unseen(cache: &Cache, reference: CacheRef<'_>) {
        use crate::cache::sync_state::{MailboxObservation, ReconciledFlags};

        // Reuse the previous observation timestamp so the regression also
        // exercises queued evidence that ties or outranks the verified write.
        let checked_at_ms = cache
            .cached_message(reference)
            .unwrap()
            .unwrap()
            .flags_checked_at_ms
            .unwrap();
        let account = reference.account;
        let mailbox = reference.mailbox;
        let ticket = cache.begin_dirty(account, &[mailbox]).unwrap();
        assert!(matches!(
            cache.listing_stamp(account, mailbox, reference.uid_validity),
            Err(CacheError::NeedsVerification)
        ));
        cache.finish_dirty(ticket).unwrap();
        let fence = cache.sync_reconcile_fence(account, mailbox).unwrap();
        let observed = MailboxObservation {
            uid_validity: reference.uid_validity,
            uid_next: reference.uid + 1,
            checked_at_ms,
        };
        let scan = cache
            .start_sync_reconcile(account, mailbox, observed, &fence)
            .unwrap();
        let page = cache.sync_known_page(&scan.stamp, 200).unwrap();
        let flags = [ReconciledFlags {
            uid: reference.uid,
            flags: &[],
        }];
        assert!(
            cache
                .commit_sync_reconcile(&scan.stamp, &page, &flags, checked_at_ms, &|| false)
                .unwrap()
                .complete
        );
    }

    #[tokio::test]
    async fn old_summary_flags_cannot_reappear_after_verified_mutation() {
        use crate::{
            accounts::{AccountCatalog, CacheRunIdentity},
            cache::sync_state::{MailboxObservation, ScanKind},
            config::{self, ValidatedAccountSet},
        };

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.json");
        std::fs::write(
            &path,
            br#"{"version":1,"mode":"full","directory":"cache","max_bytes":33554432,"accounts":[{"account_id":"default","mailboxes":["INBOX"],"max_bytes":8388608}]}"#,
        ).unwrap();
        let config = config::fixture();
        let settings = ValidatedAccountSet::legacy(config);
        let catalog = AccountCatalog::from_test_accounts(settings.clone()).unwrap();
        let account = catalog.registered_accounts().pop().unwrap().key;
        let open = |run| {
            let cache_settings = config::load_cache_file(&path, &settings).unwrap();
            MailRuntime::prepare_cache_for_run(&catalog, cache_settings, Some(run)).unwrap()
        };
        let run = CacheRunIdentity {
            instance: uuid::Uuid::new_v4(),
            previous: None,
            current: uuid::Uuid::new_v4(),
        };
        let persistent = open(run);
        let mail = MailRuntime::with_cache(
            catalog.clone(),
            Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap()),
            persistent.clone(),
        )
        .unwrap();
        let service = mail.service_for(&account).unwrap();
        let cache = persistent.full_storage(&account).unwrap();
        let reference = CacheRef {
            account: &account,
            mailbox: "INBOX",
            uid_validity: 9,
            uid: 7,
        };
        let flags = |cache: &Cache| {
            cache
                .cached_message(reference)
                .unwrap()
                .unwrap()
                .flags
                .unwrap()
        };
        let initial_stamp = cache.listing_stamp(&account, "INBOX", 9).unwrap();
        let observed = MailboxObservation {
            uid_validity: 9,
            uid_next: 8,
            checked_at_ms: 1,
        };
        let history = cache
            .start_sync(&account, "INBOX", ScanKind::History, observed)
            .unwrap();
        cache
            .record_sync_search(&history.stamp, 1, 7, &[7], &|| false)
            .unwrap();
        cache
            .remember_listing(
                reference,
                ListingEvidence {
                    flags: Some(&[]),
                    internal_date: None,
                    size: None,
                    observed_at_ms: 1,
                },
                Some(&initial_stamp),
            )
            .unwrap();
        assert_eq!(
            summary_flags(service, "\\Seen", None).await,
            json!(["Seen"])
        );
        persistent.sync_write_barrier().await.unwrap();
        assert_eq!(flags(cache), ["Seen"]);
        let previous = cache.cached_message(reference).unwrap().unwrap();
        let size_only = ListingEvidence {
            flags: None,
            internal_date: None,
            size: Some(101),
            observed_at_ms: previous.observed_at_ms.unwrap(),
        };
        assert!(matches!(
            cache.remember_listing(
                reference,
                ListingEvidence {
                    flags: Some(&[]),
                    ..size_only
                },
                None,
            ),
            Err(CacheError::NeedsVerification)
        ));
        let current = cache.cached_message(reference).unwrap().unwrap();
        assert_eq!(
            (current.size, current.flags),
            (previous.size, previous.flags)
        );
        cache.remember_listing(reference, size_only, None).unwrap();
        let current = cache.cached_message(reference).unwrap().unwrap();
        assert_eq!(current.size, Some(101));
        assert_eq!(
            current.flags.as_deref(),
            Some(["Seen".to_owned()].as_slice())
        );

        let (entered, wait_entered) = oneshot::channel();
        let (release, wait_release) = oneshot::channel();
        let mut old = Box::pin(summary_flags(
            service,
            "\\Seen",
            Some((entered, wait_release)),
        ));
        tokio::select! {
            entered = wait_entered => entered.unwrap(),
            _ = &mut old => panic!("the old FETCH did not wait"),
        }
        verify_unseen(cache, reference);
        assert!(flags(cache).is_empty());
        release.send(()).unwrap();
        assert_eq!(old.await, json!(["Seen"]));
        persistent.sync_write_barrier().await.unwrap();
        let after_delayed = flags(cache);

        assert_eq!(
            summary_flags(service, "\\Flagged", None).await,
            json!(["Flagged"])
        );
        persistent.sync_write_barrier().await.unwrap();
        assert_eq!(flags(cache), ["Flagged"]);

        let (entered, wait_entered) = oneshot::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        assert!(persistent.submit(&account, 1, false, move |_, _| {
            entered.send(()).unwrap();
            wait_release
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            Ok(())
        }));
        wait_entered.await.unwrap();
        assert_eq!(
            summary_flags(service, "\\Seen", None).await,
            json!(["Seen"])
        );
        verify_unseen(cache, reference);
        assert!(flags(cache).is_empty());
        release.send(()).unwrap();
        persistent.sync_write_barrier().await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        mail.begin_shutdown();
        assert!(
            tokio::time::timeout_at(deadline, mail.drain_shutdown(deadline))
                .await
                .unwrap()
        );
        assert!(mail.finish_cache(persistent, deadline).await);
        drop(mail);

        let next = open(CacheRunIdentity {
            previous: Some(run.current),
            current: uuid::Uuid::new_v4(),
            ..run
        });
        let cache = next.full_storage(&account).unwrap();
        assert!(!cache.status(&account).unwrap().startup_unclean);
        let after_reopen = flags(cache);
        next.finish().await;
        assert_eq!(
            (after_delayed, after_reopen),
            (Vec::<String>::new(), Vec::<String>::new())
        );
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
