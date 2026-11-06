//! Principal-aware account routing over one journal and one backend scheduler.
use super::*;
use crate::accounts::{AccountCatalog, AccountLease};
use scheduler::{AdmissionError, BackendPermit, Scheduler, WorkClass};
use std::{collections::HashMap, future::Future};

pub struct MailRuntime {
    config: Arc<Config>,
    journal: Arc<Journal>,
    pub(super) catalog: Arc<AccountCatalog>,
    pub(super) scheduler: Arc<Scheduler>,
    write_guards: Arc<cache_write_guard::WriteGuardPool>,
    services: HashMap<AccountKey, MailService>,
    pub attachments: Arc<attachments::AttachmentStore>,
    conversations: conversation::CursorStore,
    sync: sync::worker::Supervisor,
}

tokio::task_local! {
    pub(super) static READ_ADMISSION: Arc<BackendPermit>;
    pub(super) static WRITE_THROUGH_REVISION: u64;
}

pub(super) fn read_admission() -> Option<Arc<BackendPermit>> {
    READ_ADMISSION.try_with(Arc::clone).ok()
}

pub(super) fn write_through_revision() -> u64 {
    WRITE_THROUGH_REVISION
        .try_with(|revision| *revision)
        .unwrap_or(0)
}

impl MailRuntime {
    pub fn new(catalog: Arc<AccountCatalog>, journal: Arc<Journal>) -> anyhow::Result<Self> {
        Self::with_cache(catalog, journal, PreparedCache::default())
    }

    #[cfg(test)]
    pub fn prepare_cache(
        catalog: &AccountCatalog,
        settings: Option<crate::config::CacheSettings>,
    ) -> anyhow::Result<PreparedCache> {
        Self::prepare_cache_for_run(catalog, settings, None)
    }

    pub fn prepare_cache_for_run(
        catalog: &AccountCatalog,
        settings: Option<crate::config::CacheSettings>,
        run: Option<crate::accounts::CacheRunIdentity>,
    ) -> anyhow::Result<PreparedCache> {
        match settings {
            Some(settings) => PreparedCache::open(catalog, settings, run),
            None => Ok(PreparedCache::default()),
        }
    }

    pub fn with_cache(
        catalog: Arc<AccountCatalog>,
        journal: Arc<Journal>,
        cache: PreparedCache,
    ) -> anyhow::Result<Self> {
        let config = catalog.config().clone();
        let attachments = Arc::new(attachments::AttachmentStore::new(config.clone()));
        let metadata_cache = Arc::new(std::sync::Mutex::new(insights::MetadataCache::default()));
        let write_guards = Arc::new(cache_write_guard::WriteGuardPool::default());
        let mut services = HashMap::new();
        for lease in catalog.registered_accounts() {
            let mut service = MailService::for_account(
                lease.config,
                journal.clone(),
                lease.key.clone(),
                lease.alias,
                lease.is_legacy,
                attachments.clone(),
                metadata_cache.clone(),
            )?;
            service.persistent = cache.clone();
            service.write_guards = write_guards.clone();
            services.insert(lease.key, service);
        }
        Ok(Self {
            config,
            journal,
            catalog,
            scheduler: Scheduler::new(),
            write_guards,
            services,
            attachments,
            conversations: cache.cursor_store(),
            sync: sync::worker::Supervisor::new(cache),
        })
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    pub(crate) fn shutdown_budget(&self) -> std::time::Duration {
        // A queued draft/reply/forward can still use separate full IMAP and
        // SMTP budgets, two cache fences and QUIT after admission.
        scheduler::QUEUE_WAIT
            + self.config.timeout * 2
            + persistence::WRITE_FENCE_WAIT * 2
            + crate::smtp::QUIT_TIMEOUT
            + std::time::Duration::from_secs(20) // journal, local work and cache finalization
    }

    pub(crate) fn begin_shutdown(&self) {
        self.write_guards.seal();
        self.sync.stop();
    }

    pub(crate) async fn drain_shutdown(&self, deadline: tokio::time::Instant) -> bool {
        let (writes_finished, sync_finished) =
            tokio::join!(self.write_guards.drain_until(deadline), self.sync.finish());
        writes_finished && sync_finished
    }

    pub(crate) async fn finish_cache(
        &self,
        cache: PreparedCache,
        deadline: tokio::time::Instant,
    ) -> bool {
        let (reads_finished, transfers_finished) = tokio::join!(
            self.scheduler.stop_and_drain(deadline),
            self.attachments.drain_transfers(deadline),
        );
        reads_finished
            && transfers_finished
            && tokio::time::Instant::now() < deadline
            && cache.finish_before(deadline).await
    }

    pub fn start_sync(self: &Arc<Self>) {
        self.sync.start(self);
    }

    pub fn has_read_access(&self, principal: &str) -> bool {
        !self.catalog.visible_accounts(principal).is_empty()
    }

    pub fn legacy_mode_for(&self, principal: &str) -> Option<String> {
        self.catalog
            .write_account(principal)
            .ok()
            .map(|lease| lease.config.mode.clone())
    }

    pub fn list_accounts(&self, principal: &str) -> Value {
        let visible = self.catalog.visible_accounts(principal);
        let default = self.catalog.default_read_account_id();
        let default = visible
            .iter()
            .any(|lease| lease.alias == default)
            .then_some(default);
        let writable = self.legacy_mode_for(principal);
        let accounts = visible
            .into_iter()
            .filter_map(|lease| {
                let service = self.services.get(&lease.key)?;
                let mut value = service.account_discovery()["accounts"][0].clone();
                value["id"] = json!(lease.alias);
                value["display_name"] = json!(lease.display_name);
                value["default"] = json!(default == Some(lease.alias.as_str()));
                value["mode"] = json!(if lease.is_legacy {
                    writable.as_deref().unwrap_or("read")
                } else {
                    "read"
                });
                if service.persistent.full_storage(&lease.key).is_some() {
                    value["capabilities"]["cache_modes"] = json!(["off", "metadata", "full"]);
                } else if service.persistent.configured(&lease.key) {
                    value["capabilities"]["cache_modes"] = json!(["off", "metadata"]);
                }
                Some(value)
            })
            .collect::<Vec<_>>();
        json!({"accounts":accounts,"default_account_id":default})
    }

    pub async fn execute(&self, name: &str, args: &Value, principal: &str) -> Result<Value> {
        if !self
            .config
            .subjects
            .iter()
            .any(|subject| subject == principal)
        {
            return Err(MailError::account_unavailable());
        }
        // Historical send outcomes remain principal-scoped and independent of
        // mailbox ACL, active account generations, backoff and backend slots.
        if name == "get_operation" {
            let key = text(args, "operation_key")?;
            if key.len() > 128 {
                return Err(MailError::input());
            }
            return Ok(json!({"operation":self.journal.get(principal,key)?}));
        }
        if name == "list_accounts" {
            return Ok(self.list_accounts(principal));
        }
        if account_read(name) {
            let requested = args
                .get("account_id")
                .map(|value| value.as_str().ok_or_else(MailError::input))
                .transpose()?;
            let lease = self.catalog.read_account(principal, requested)?;
            let service = self.service_for(&lease.key)?;
            if name == "get_cache_status" {
                return self
                    .run_cached(&lease, async {
                        let mut value = service.persistent.status(&lease.key, &lease.alias).await;
                        self.sync.add_health(&lease.key, &mut value);
                        Ok(value)
                    })
                    .await;
            }
            let mut selected = args.clone();
            selected
                .as_object_mut()
                .ok_or_else(MailError::input)?
                .insert("account_id".into(), json!(lease.alias));
            let offline = if name == "read_conversation" {
                conversation::cached_request(&selected)?
            } else {
                read_source::Source::parse(name, &selected)? == read_source::Source::Cache
            };
            let result = if offline {
                self.run_cached(
                    &lease,
                    Box::pin(async {
                        if name == "read_conversation" {
                            Box::pin(
                                service.cached_legacy_conversation(&selected, &lease.cancellation),
                            )
                            .await
                        } else {
                            Box::pin(service.cached_operation(name, &selected, principal)).await
                        }
                    }),
                )
                .await?
            } else {
                self.run_read(
                    &lease,
                    Box::pin(service.execute(name, &selected, principal)),
                )
                .await?
            };
            Ok(account_context(name, result, &selected, &lease.alias))
        } else {
            let lease = self.catalog.write_account(principal)?;
            if !write_allowed(name, &lease.config.mode) {
                return Err(MailError::account_unavailable());
            }
            let service = self.service_for(&lease.key)?;
            let _write = self
                .write_guards
                .admit()
                .ok_or_else(|| admission_error(AdmissionError::Capacity))?;
            self.run_write(&lease, Box::pin(service.execute(name, args, principal)))
                .await
        }
    }

    pub(super) fn service_for(&self, key: &AccountKey) -> Result<&MailService> {
        self.services
            .get(key)
            .ok_or_else(MailError::account_unavailable)
    }

    pub(super) async fn run_write<T>(
        &self,
        lease: &AccountLease,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let permit = self
            .scheduler
            .acquire(
                lease.key.reference_namespace(),
                WorkClass::Foreground,
                &lease.cancellation,
            )
            .await
            .map_err(admission_error)?;
        // Admission can be shared with blocking preflight work. After the
        // write starts, ACL cancellation never cuts its outcome handling.
        let revision = self
            .service_for(&lease.key)?
            .persistent
            .write_through_revision();
        READ_ADMISSION
            .scope(
                Arc::new(permit),
                WRITE_THROUGH_REVISION.scope(revision, operation),
            )
            .await
    }

    pub(crate) async fn cross_conversation(
        &self,
        args: &Value,
        principal: &str,
        fits: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        // Resolve the explicit/default read account before touching cursor state.
        let requested = args
            .get("account_id")
            .map(|value| value.as_str().ok_or_else(MailError::input))
            .transpose()?;
        let lease = self.catalog.read_account(principal, requested)?;
        let service = self.service_for(&lease.key)?;
        let operation = Box::pin(service.cross_conversation(
            &self.conversations,
            args,
            principal,
            &lease.cancellation,
            fits,
        ));
        if conversation::cached_request(args)? {
            self.run_cached(&lease, operation).await
        } else {
            self.run_read(&lease, operation).await
        }
    }

    pub(super) async fn run_read<T>(
        &self,
        lease: &AccountLease,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let service = self.service_for(&lease.key)?;
        service.backend_ready()?;
        self.run_cached(lease, async {
            service.backend_ready()?;
            WRITE_THROUGH_REVISION
                .scope(service.persistent.write_through_revision(), operation)
                .await
        })
        .await
    }

    pub(super) async fn run_cached<T>(
        &self,
        lease: &AccountLease,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let permit = self
            .scheduler
            .acquire(
                lease.key.reference_namespace(),
                WorkClass::Foreground,
                &lease.cancellation,
            )
            .await
            .map_err(admission_error)?;
        READ_ADMISSION
            .scope(Arc::new(permit), async {
                tokio::select! {
                    biased;
                    _ = lease.cancellation.cancelled() => Err(MailError::account_unavailable()),
                    result = operation => result,
                }
            })
            .await
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_test_smtp(mut self, port: u16) -> Self {
        self.services
            .values_mut()
            .find(|service| service.smtp.is_some())
            .expect("legacy SMTP service")
            .smtp = Some(SmtpSender::fixture(port, self.config.timeout));
        self
    }
    #[cfg(test)]
    pub(crate) fn with_test_imap_roots(mut self, roots: RootCertStore) -> Self {
        for service in self.services.values_mut() {
            service.test_imap_roots = Some(roots.clone());
        }
        self
    }
}

pub(super) fn admission_error(error: AdmissionError) -> MailError {
    match error {
        AdmissionError::AccountCancelled => MailError::account_unavailable(),
        AdmissionError::Capacity => MailError {
            code: "BUSY",
            message: "Mailbox queue is full. Retry this read later; inspect write outcomes first.",
            retryable: true,
            details: None,
        },
        AdmissionError::QueueTimeout => MailError {
            code: "BUSY",
            message: "Mailbox concurrency limit reached.",
            retryable: true,
            details: None,
        },
    }
}

pub(super) fn account_read(name: &str) -> bool {
    matches!(
        name,
        "list_mailboxes"
            | "list_emails"
            | "search_emails"
            | "get_email"
            | "get_email_status"
            | "get_emails"
            | "download_attachment"
            | "list_attachments"
            | "prepare_attachment_download"
            | "read_conversation"
            | "locate_message"
            | "get_mailbox_statistics"
            | "get_cache_status"
            | "diagnose_connectivity"
    )
}
fn write_allowed(name: &str, mode: &str) -> bool {
    match name {
        "save_draft" => mode != "read",
        "send_email" | "send_draft" | "reply_email" | "forward_email" => {
            matches!(mode, "send" | "full")
        }
        "mark_email" | "move_email" | "delete_email" | "mark_emails" | "move_emails"
        | "delete_emails" | "create_mailbox" | "rename_mailbox" | "delete_mailbox" => {
            mode == "full"
        }
        _ => false,
    }
}

fn account_context(name: &str, mut value: Value, args: &Value, alias: &str) -> Value {
    value["account_id"] = json!(alias);
    let mailbox = args["mailbox"].as_str().unwrap_or("INBOX");
    if !matches!(
        name,
        "list_mailboxes" | "locate_message" | "diagnose_connectivity"
    ) {
        value["mailbox"] = json!(mailbox);
    }
    match name {
        "list_emails" | "search_emails" => {
            let validity = value["uid_validity"].clone();
            if let Some(items) = value["emails"].as_array_mut() {
                for item in items {
                    item["account_id"] = json!(alias);
                    item["mailbox"] = json!(mailbox);
                    item["uid_validity"] = validity.clone();
                }
            }
        }
        "get_emails" => {
            if let Some(items) = value["items"].as_array_mut() {
                for item in items {
                    if let Some(email) = item.get_mut("email") {
                        email["account_id"] = json!(alias);
                        email["mailbox"] = json!(mailbox);
                    }
                }
            }
        }
        "read_conversation" => {
            if let Some(items) = value["messages"].as_array_mut() {
                for item in items {
                    item["account_id"] = json!(alias);
                    item["mailbox"] = json!(mailbox);
                }
            }
        }
        "locate_message" => {
            for field in ["matches", "scopes"] {
                if let Some(items) = value[field].as_array_mut() {
                    for item in items {
                        item["account_id"] = json!(alias);
                    }
                }
            }
        }
        _ => {}
    }
    value
}
