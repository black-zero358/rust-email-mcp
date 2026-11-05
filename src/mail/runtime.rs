//! Principal-aware account routing over one journal and one backend scheduler.
use super::*;
use crate::accounts::{AccountCatalog, AccountLease};
use scheduler::{AdmissionError, Scheduler, WorkClass};
use std::{collections::HashMap, future::Future};

pub struct MailRuntime {
    config: Arc<Config>,
    journal: Arc<Journal>,
    pub(super) catalog: Arc<AccountCatalog>,
    pub(super) scheduler: Arc<Scheduler>,
    services: HashMap<AccountKey, MailService>,
    pub attachments: Arc<attachments::AttachmentStore>,
    conversations: conversation::CursorStore,
}

impl MailRuntime {
    pub fn new(catalog: Arc<AccountCatalog>, journal: Arc<Journal>) -> anyhow::Result<Self> {
        let config = catalog.config().clone();
        let attachments = Arc::new(attachments::AttachmentStore::new(config.clone()));
        let metadata_cache = Arc::new(std::sync::Mutex::new(insights::MetadataCache::default()));
        let mut services = HashMap::new();
        for lease in catalog.registered_accounts() {
            services.insert(
                lease.key.clone(),
                MailService::for_account(
                    lease.config,
                    journal.clone(),
                    lease.key,
                    lease.alias,
                    lease.is_legacy,
                    attachments.clone(),
                    metadata_cache.clone(),
                )?,
            );
        }
        Ok(Self {
            config,
            journal,
            catalog,
            scheduler: Scheduler::new(),
            services,
            attachments,
            conversations: Default::default(),
        })
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.config
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
            let mut selected = args.clone();
            selected
                .as_object_mut()
                .ok_or_else(MailError::input)?
                .insert("account_id".into(), json!(lease.alias));
            let result = self
                .run_read(&lease, service.execute(name, &selected, principal))
                .await?;
            Ok(account_context(name, result, &selected, &lease.alias))
        } else {
            let lease = self.catalog.write_account(principal)?;
            if !write_allowed(name, &lease.config.mode) {
                return Err(MailError::account_unavailable());
            }
            let service = self.service_for(&lease.key)?;
            let _permit = self
                .scheduler
                .acquire(
                    lease.key.reference_namespace(),
                    WorkClass::Foreground,
                    &lease.cancellation,
                )
                .await
                .map_err(admission_error)?;
            // Preserve SMTP's journal-before-send and known/unknown outcome
            // handling: an already started write is never cancelled by ACL.
            service.execute(name, args, principal).await
        }
    }

    pub(super) fn service_for(&self, key: &AccountKey) -> Result<&MailService> {
        self.services
            .get(key)
            .ok_or_else(MailError::account_unavailable)
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
        self.run_read(
            &lease,
            Box::pin(service.cross_conversation(
                &self.conversations,
                args,
                principal,
                &lease.cancellation,
                fits,
            )),
        )
        .await
    }

    pub(super) async fn run_read<T>(
        &self,
        lease: &AccountLease,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let service = self.service_for(&lease.key)?;
        service.backend_ready()?;
        self.scheduler
            .run(
                lease.key.reference_namespace(),
                WorkClass::Foreground,
                &lease.cancellation,
                || async {
                    service.backend_ready()?;
                    operation.await
                },
            )
            .await
            .map_err(admission_error)?
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
