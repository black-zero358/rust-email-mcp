//! Local read routing and proof checks. These paths never open IMAP.
use super::{parts::cached_body, persistence::canonical_mailbox, *};
use crate::cache::{
    Access, ArtifactKey, ArtifactState, Cache, CacheError, CachedMessage, Ref as CacheRef,
    ScopeSnapshot,
};
use std::io::Read;
use tokio::sync::OwnedSemaphorePermit;

pub(super) fn cache_error(error: CacheError) -> MailError {
    let (code, message, retryable) = match error {
        CacheError::NeedsVerification => (
            "CACHE_NEEDS_VERIFICATION",
            "The cached scope needs a live verification.",
            true,
        ),
        CacheError::OutOfScope => (
            "CACHE_UNAVAILABLE",
            "The requested cache scope is unavailable.",
            false,
        ),
        CacheError::Quota => (
            "LIMIT_EXCEEDED",
            "The cache byte quota has been reached.",
            false,
        ),
        CacheError::Storage | CacheError::Corrupt => (
            "STORAGE_UNAVAILABLE",
            "The cached data is unavailable.",
            true,
        ),
    };
    MailError {
        code,
        message,
        retryable,
        details: None,
    }
}

pub(super) fn cache_miss() -> MailError {
    MailError {
        code: "CACHE_MISS",
        message: "The requested content has no complete local copy.",
        retryable: false,
        details: None,
    }
}

pub(super) fn cache_context(scopes: Vec<Value>) -> Value {
    json!({"stale":true,"remote_generation_unverified":true,"scopes":scopes})
}

pub(super) fn scope_info(scope: &ScopeSnapshot) -> Value {
    json!({
        "mailbox":scope.mailbox,
        "uid_validity":scope.uid_validity,
        "high_uid":scope.high_uid,
        "checked_at_ms":scope.checked_at_ms,
        "coverage":{
            "known_messages":scope.known_messages,
            "complete_headers":scope.complete_headers,
            "indexed_threads":scope.indexed_threads,
            "remote_complete":false
        }
    })
}

#[derive(Clone)]
pub(super) struct CachedReader {
    pub(super) cache: Arc<Cache>,
    pub(super) account: AccountKey,
    pub(super) alias: String,
    pub(super) config: Arc<Config>,
    pub(super) persistent: PreparedCache,
}

impl CachedReader {
    pub(super) fn scope_info_with_layers(&self, scope: &ScopeSnapshot) -> Result<Value> {
        let mut value = scope_info(scope);
        value["coverage"]["layers"] = persistence::coverage(
            &self.cache,
            &self.account,
            &scope.mailbox,
            scope.uid_validity,
        )
        .map_err(cache_error)?;
        Ok(value)
    }
    pub(super) fn reference<'a>(
        &'a self,
        mailbox: &'a str,
        validity: u32,
        uid: u32,
    ) -> CacheRef<'a> {
        CacheRef {
            account: &self.account,
            mailbox: canonical_mailbox(mailbox),
            uid_validity: validity,
            uid,
        }
    }

    pub(super) fn verify(&self, scope: &ScopeSnapshot, uid: Option<u32>) -> Result<()> {
        if !self
            .persistent
            .offline_current(&self.account, &scope.mailbox, scope.uid_validity, uid)
        {
            return Err(cache_error(CacheError::NeedsVerification));
        }
        self.cache
            .verify_scope(&self.account, scope)
            .map_err(cache_error)
    }

    pub(super) fn freeze(&self, mailbox: &str, args: &Value) -> Result<ScopeSnapshot> {
        let scope = self
            .cache
            .freeze_scope(&self.account, canonical_mailbox(mailbox))
            .map_err(cache_error)?;
        check_validity(args, Some(scope.uid_validity))?;
        self.verify(&scope, None)?;
        Ok(scope)
    }

    fn body(
        &self,
        reference: CacheRef<'_>,
        access: Access,
        args: &Value,
        limit: usize,
    ) -> Result<Value> {
        if !self.persistent.offline_current(
            reference.account,
            reference.mailbox,
            reference.uid_validity,
            Some(reference.uid),
        ) {
            return Err(cache_error(CacheError::NeedsVerification));
        }
        let message = if access == Access::Offline {
            self.cache.cached_message(reference)
        } else {
            self.cache.cached_message_live_verified(reference)
        }
        .map_err(cache_error)?
        .ok_or_else(cache_miss)?;
        let header = message
            .header
            .as_deref()
            .filter(|_| message.header_complete)
            .ok_or_else(cache_miss)?;
        let state = self
            .cache
            .artifact_state(reference, cached_body::KEY)
            .map_err(cache_error)?;
        let state = if state == ArtifactState::Missing
            && let ArtifactState::Materialized(raw) = self
                .cache
                .artifact_state(reference, ArtifactKey::Raw)
                .map_err(cache_error)?
        {
            ArtifactState::Derivable { raw: raw.id }
        } else {
            state
        };
        let (mut value, lease) = match state {
            ArtifactState::Materialized(info) => {
                let mut lease = self
                    .cache
                    .open_blob(reference, cached_body::KEY, access, Some(info.id))
                    .map_err(cache_error)?;
                let body = cached_body::Record::decode(&mut lease).map_err(cache_error)?;
                let parts = if access == Access::Offline {
                    self.cache
                        .cached_parts(reference)
                        .map_err(cache_error)?
                        .map(|(parts, _)| parts)
                } else {
                    self.cache.parts(reference).map_err(cache_error)?
                }
                .ok_or_else(cache_miss)?;
                let manifest = manifest(&parts)?;
                let headers = mime::headers(header)?;
                let attachments = manifest["parts"]
                    .as_array()
                    .ok_or_else(cache_miss)?
                    .iter()
                    .filter(|part| part["is_attachment"] == true)
                    .map(|part| {
                        json!({
                            "attachment_index":part["attachment_index"],
                            "content_type":part["content_type"], "filename":part["filename"]
                        })
                    })
                    .collect::<Vec<_>>();
                let value = insights::with_displayed(
                    json!({"uid":reference.uid,"uid_validity":reference.uid_validity,
                        "subject":headers.get_first_value("Subject"),"from":headers.get_first_value("From"),
                        "to":headers.get_first_value("To"),"attachments":attachments,"content_trust":"untrusted"}),
                    &headers,
                    body.display(insights::display_format(args)?)?,
                    limit,
                    args,
                )?;
                lease.revalidate().map_err(cache_error)?;
                (value, lease)
            }
            ArtifactState::Derivable { raw } => {
                let mut lease = self
                    .cache
                    .open_blob(reference, ArtifactKey::Raw, access, Some(raw))
                    .map_err(cache_error)?;
                let bytes = blob_bytes(&mut lease, self.config.max_message)?;
                let value = insights::parsed_with_options(
                    &bytes,
                    reference.uid,
                    Some(reference.uid_validity),
                    limit,
                    args,
                )?;
                lease.revalidate().map_err(cache_error)?;
                (value, lease)
            }
            ArtifactState::Missing => return Err(cache_miss()),
            ArtifactState::Omitted(_) => {
                return Err(MailError::limit_kind(LimitKind::DecodedBodyBytes));
            }
            ArtifactState::Unsupported(_) => return Err(crate::html::unsupported()),
        };
        let current = || {
            self.persistent.offline_current(
                reference.account,
                reference.mailbox,
                reference.uid_validity,
                Some(reference.uid),
            )
        };
        if !current() {
            return Err(cache_error(CacheError::NeedsVerification));
        }
        lease.touch();
        lease.revalidate().map_err(cache_error)?;
        if !current() {
            return Err(cache_error(CacheError::NeedsVerification));
        }
        value["read_diagnostics"] =
            json!({"source":"cache","fallback_reason":null,"limit_kind":null});
        if access == Access::Offline {
            value["account_id"] = json!(self.alias);
            value["mailbox"] = json!(reference.mailbox);
        }
        Ok(value)
    }

    fn read_bodies(&self, name: &str, args: &Value) -> Result<Value> {
        let mailbox = folder(args, InputLimits::Read)?;
        let scope = self.freeze(mailbox, args)?;
        let mut value = if name == "get_email" {
            self.body(
                self.reference(mailbox, scope.uid_validity, uid(args)?),
                Access::Offline,
                args,
                (self.config.max_output / 2).min(crate::config::MAX_DISPLAY_BODY),
            )?
        } else {
            let ids = args["uids"]
                .as_array()
                .filter(|ids| !ids.is_empty() && ids.len() <= BATCH_READ_ITEMS)
                .ok_or_else(MailError::input)?;
            let mut items = Vec::with_capacity(ids.len());
            for id in ids {
                let id = id
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok())
                    .filter(|id| *id > 0)
                    .ok_or_else(MailError::input)?;
                let result = self.body(
                    self.reference(mailbox, scope.uid_validity, id),
                    Access::Offline,
                    args,
                    64 * 1024,
                );
                items.push(match result {
                    Ok(email) => json!({"uid":id,"ok":true,"email":email}),
                    Err(error) => json!({"uid":id,"ok":false,"error":error}),
                });
            }
            json!({"items":items,"requested":ids.len(),"completed":ids.len(),"uid_validity":scope.uid_validity})
        };
        let coverage = self.scope_info_with_layers(&scope)?;
        self.verify(&scope, None)?;
        value["read_source"] = json!("cache");
        value["cache"] = cache_context(vec![coverage]);
        Ok(value)
    }

    fn mailboxes(&self) -> Result<Value> {
        let mut mailboxes = Vec::new();
        let mut scopes = Vec::new();
        for row in self
            .cache
            .cached_mailboxes(&self.account)
            .map_err(cache_error)?
        {
            if let Some(evidence) = row.evidence {
                mailboxes.push(evidence);
            }
            scopes.push(
                json!({"mailbox":row.name,"checked_at_ms":row.checked_at_ms,"coverage":{
                    "list_observed":row.checked_at_ms.is_some(),"remote_complete":false
                }}),
            );
        }
        Ok(json!({"mailboxes":mailboxes,"read_source":"cache","cache":cache_context(scopes)}))
    }
}

pub(super) fn manifest(bytes: &[u8]) -> Result<Value> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| cache_error(CacheError::Corrupt))?;
    if value["version"] != 1
        || value["parts"].as_array().is_none_or(|parts| {
            parts.len() > mime::MAX_PARTS
                || parts
                    .iter()
                    .enumerate()
                    .any(|(i, part)| part["attachment_index"] != i)
        })
    {
        return Err(cache_error(CacheError::Corrupt));
    }
    Ok(value)
}

pub(super) fn blob_bytes(lease: &mut crate::cache::BlobLease, limit: usize) -> Result<Vec<u8>> {
    let size = usize::try_from(lease.info().bytes)
        .ok()
        .filter(|bytes| *bytes <= limit)
        .ok_or_else(|| MailError::limit_kind(LimitKind::RawMessageBytes))?;
    let mut bytes = vec![0; size];
    lease
        .read_exact(&mut bytes)
        .map_err(|_| cache_error(CacheError::Storage))?;
    lease.revalidate().map_err(cache_error)?;
    Ok(bytes)
}

impl MailService {
    pub(super) fn cached_reader(&self) -> Result<CachedReader> {
        let cache = self
            .persistent
            .full_storage(&self.account)
            .ok_or_else(|| cache_error(CacheError::OutOfScope))?;
        Ok(CachedReader {
            cache: cache.clone(),
            account: self.account.clone(),
            alias: self.alias.clone(),
            config: self.config.clone(),
            persistent: self.persistent.clone(),
        })
    }

    pub(super) async fn cache_job<T: Send + 'static>(
        &self,
        operation: impl FnOnce(CachedReader) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let reader = self.cached_reader()?;
        let permit = self
            .persistent
            .cache_read_permit()
            .ok_or_else(|| runtime::admission_error(scheduler::AdmissionError::Capacity))?;
        cache_job(reader, permit, operation).await
    }

    pub(super) async fn cached_body_for_conversation(
        &self,
        mailbox: &str,
        validity: u32,
        uid: u32,
        args: &Value,
        limit: usize,
    ) -> Result<Value> {
        let (mailbox, args) = (mailbox.to_owned(), args.clone());
        self.cache_job(move |reader| {
            let scope = reader.freeze(&mailbox, &json!({"uid_validity":validity}))?;
            let result = reader.body(
                reader.reference(&mailbox, validity, uid),
                Access::Offline,
                &args,
                limit,
            )?;
            reader.verify(&scope, Some(uid))?;
            Ok(result)
        })
        .await
    }

    pub(super) async fn cached_mailboxes(&self) -> Result<Value> {
        self.cache_job(|reader| reader.mailboxes()).await
    }

    pub(super) async fn cached_live_body(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
        limit: usize,
        args: &Value,
    ) -> Option<Value> {
        let validity = validity?;
        self.persistent.full_folder(&self.account, mailbox)?;
        let (mailbox, args) = (mailbox.to_owned(), args.clone());
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            self.cache_job(move |reader| {
                reader.body(
                    reader.reference(&mailbox, validity, uid),
                    Access::LiveVerified,
                    &args,
                    limit,
                )
            }),
        )
        .await
        .ok()?
        .ok()
    }

    pub(super) async fn cached_operation(
        &self,
        name: &str,
        args: &Value,
        principal: &str,
    ) -> Result<Value> {
        match name {
            "list_mailboxes" => self.cached_mailboxes().await,
            "get_email" | "get_emails" => {
                let (name, args) = (name.to_owned(), args.clone());
                self.cache_job(move |reader| reader.read_bodies(&name, &args))
                    .await
            }
            "list_emails" | "search_emails" | "locate_message" => {
                let (name, args) = (name.to_owned(), args.clone());
                self.cache_job(move |reader| reader.cached_query(&name, &args))
                    .await
            }
            "list_attachments" | "download_attachment" | "prepare_attachment_download" => {
                self.cached_attachment(name, args, principal).await
            }
            _ => Err(MailError::input()),
        }
    }
}

async fn cache_job<T: Send + 'static>(
    reader: CachedReader,
    permit: OwnedSemaphorePermit,
    operation: impl FnOnce(CachedReader) -> Result<T> + Send + 'static,
) -> Result<T> {
    let admission = runtime::read_admission();
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _admission = admission;
        operation(reader)
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .map_err(|_| MailError::timeout())?
        .map_err(|_| cache_error(CacheError::Storage))?
}

mod query;
