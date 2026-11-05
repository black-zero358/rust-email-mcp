//! Admission and provenance for write-through metadata observations.
use super::*;
use crate::{
    cache::{AccountScope, Cache, CacheError, Ref as CacheRef},
    config::CacheSettings,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Semaphore, mpsc, oneshot};

const CACHE_JOBS: usize = EXECUTION_SLOTS * 4;
const CACHE_JOB_BYTES: usize = 8 * 1024 * 1024;
const INDEX_BYTES: usize = 8 * 1024;
const PARTS_BYTES: usize = 256 * 1024;

#[derive(Clone, Default)]
pub struct PreparedCache(Option<Arc<Persistent>>);

struct Persistent {
    storage: Option<Arc<Cache>>,
    queue: Option<mpsc::Sender<CacheMessage>>,
    folders: HashMap<AccountKey, HashSet<String>>,
    health: Arc<WriteHealthByAccount>,
    generations: Arc<Generations>,
    job_count: Arc<Semaphore>,
    job_bytes: Arc<Semaphore>,
}

type CacheJob = Box<dyn FnOnce(&Cache) + Send>;
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
    ) -> anyhow::Result<Self> {
        let CacheSettings {
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
        let (storage, queue) = Cache::open(&directory, max_bytes, scopes, &reserved)
            .ok()
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
            tracing::warn!(event = "cache_unavailable");
        }
        Ok(Self(Some(Arc::new(Persistent {
            storage,
            queue,
            folders,
            health: Arc::new(Mutex::new(HashMap::new())),
            generations: Arc::new(Mutex::new(HashMap::new())),
            job_count: Arc::new(Semaphore::new(CACHE_JOBS)),
            job_bytes: Arc::new(Semaphore::new(CACHE_JOB_BYTES)),
        }))))
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
        let (done, wait) = oneshot::channel();
        let submitted = self.submit(account, 4096, false, move |cache, account| {
            match cache.status(account) {
                Ok(status) => {
                    let _ = done.send((status, read_health(&health, account)));
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
        let result = result.filter(|(status, _)| state.account_current(account, &status.folders));
        match result {
            Some((status, health)) => json!({
                "account_id":alias,"mode":"metadata","status":"ready",
                "write_health":health,
                "storage":{
                    "max_bytes":status.max_bytes, "used_bytes":status.used_bytes,
                    "global_max_bytes":status.global_max_bytes, "startup_unclean":status.startup_unclean,
                    "folders":status.folders
                }
            }),
            None => {
                json!({"account_id":alias,"mode":"metadata","status":"unavailable","storage":null,
                    "write_health":read_health(&state.health, account)})
            }
        }
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
        operation: impl FnOnce(&Cache, &AccountKey) -> std::result::Result<(), CacheError>
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
    pub(super) fn persists(&self, mailbox: &str) -> bool {
        self.persistent.enabled_for_folder(&self.account, mailbox)
    }

    pub(super) fn observe_mailbox(&self, mailbox: &str, state: &async_imap::types::Mailbox) {
        if !self.persists(mailbox) {
            return;
        }
        let Some(validity) = state.uid_validity.filter(|value| *value > 0) else {
            return;
        };
        let high_uid = state.uid_next.and_then(|next| next.checked_sub(1));
        let Ok(time) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return;
        };
        let Ok(time) = i64::try_from(time.as_millis()) else {
            return;
        };
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
        self.persistent.submit(
            &self.account,
            4096 + mailbox.capacity() + bytes.capacity(),
            true,
            move |cache, account| {
                cache.remember_parts(
                    CacheRef {
                        account,
                        mailbox: &mailbox,
                        uid_validity: validity,
                        uid,
                    },
                    &bytes,
                )
            },
        );
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

fn block_generation(generations: &Generations, key: &GenerationKey, validity: u32) {
    if let Ok(mut epochs) = generations.lock()
        && let Some(epoch) = epochs.get_mut(key)
        && epoch.current == validity
    {
        epoch.blocked = true;
    }
}

fn canonical_mailbox(mailbox: &str) -> &str {
    if mailbox.eq_ignore_ascii_case("INBOX") {
        "INBOX"
    } else {
        mailbox
    }
}

fn index_text(headers: &[mailparse::MailHeader<'_>]) -> Option<String> {
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

fn encode_manifest(manifest: &parts::PartManifest) -> Option<Vec<u8>> {
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
    }
}
