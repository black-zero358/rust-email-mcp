use crate::{
    accounts::AccountKey,
    config::Config,
    error::{ConnectionBudgetExceeded, LimitKind, MailError, Result},
    imap_text::{display_name, mailbox_name, uid_field_search},
    input_limits::{InputLimits, ReadInputLimits},
    journal::{Journal, Operation},
    mime,
    smtp::SmtpSender,
};
pub mod attachments;
mod cache_read;
mod cache_write_guard;
mod classification;
pub(crate) mod conversation;
pub mod insights;
mod listing;
mod parts;
mod persistence;
pub(crate) mod read_source;
mod runtime;
pub(crate) mod scheduler;
mod sync;
use futures::TryStreamExt;
use lettre::{address::Envelope, message::Mailbox};
use mailparse::MailHeaderMap;
use parts::map_read_imap_error;
pub use persistence::PreparedCache;
pub use runtime::MailRuntime;
use scheduler::{ADMISSION_REQUESTS, EXECUTION_SLOTS};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};

/// Limits cumulative transport bytes; the parser separately bounds allocation.
#[derive(Debug)]
pub struct BudgetStream<T> {
    inner: T,
    remaining: Arc<AtomicUsize>,
}

impl<T> BudgetStream<T> {
    pub fn new(inner: T, budget: usize) -> (Self, Arc<AtomicUsize>) {
        let remaining = Arc::new(AtomicUsize::new(budget));
        (Self::with_remaining(inner, remaining.clone()), remaining)
    }

    fn with_remaining(inner: T, remaining: Arc<AtomicUsize>) -> Self {
        Self { inner, remaining }
    }
}
impl<T: AsyncRead + Unpin> AsyncRead for BudgetStream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let limit = self.remaining.load(Ordering::Relaxed).min(buf.remaining());
        if limit == 0 {
            return Poll::Ready(Err(std::io::Error::other(ConnectionBudgetExceeded)));
        }
        let mut limited = ReadBuf::new(buf.initialize_unfilled_to(limit));
        match Pin::new(&mut self.inner).poll_read(cx, &mut limited) {
            Poll::Ready(Ok(())) => {
                let n = limited.filled().len();
                self.remaining.fetch_sub(n, Ordering::Relaxed);
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for BudgetStream<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
type Session = async_imap::Session<BudgetStream<TlsStream<TcpStream>>>;
const BACKEND_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
const BATCH_READ_ITEMS: usize = 10;
const LIST_PAGE_ITEMS: usize = 50;
const SCAN_UID_WINDOW: u32 = 500;

pub struct MailService {
    pub config: Arc<Config>,
    pub journal: Arc<Journal>,
    pub(crate) account: AccountKey,
    pub(crate) alias: String,
    smtp: Option<SmtpSender>,
    pub attachments: Arc<attachments::AttachmentStore>,
    backend_backoff: std::sync::Mutex<Option<std::time::Instant>>,
    metadata_cache: Arc<std::sync::Mutex<insights::MetadataCache>>,
    persistent: PreparedCache,
    write_guards: Arc<cache_write_guard::WriteGuardPool>,
    #[cfg(test)]
    test_imap_roots: Option<RootCertStore>,
}
impl MailService {
    #[cfg(test)]
    pub fn new(config: Arc<Config>, journal: Arc<Journal>) -> anyhow::Result<Self> {
        Self::with_account(config, journal, AccountKey::synthetic())
    }

    pub fn with_account(
        config: Arc<Config>,
        journal: Arc<Journal>,
        account: AccountKey,
    ) -> anyhow::Result<Self> {
        let store = Arc::new(attachments::AttachmentStore::new(config.clone()));
        Self::for_account(
            config,
            journal,
            account,
            "default".into(),
            true,
            store,
            Default::default(),
        )
    }

    fn for_account(
        config: Arc<Config>,
        journal: Arc<Journal>,
        account: AccountKey,
        alias: String,
        is_legacy: bool,
        attachments: Arc<attachments::AttachmentStore>,
        metadata_cache: Arc<std::sync::Mutex<insights::MetadataCache>>,
    ) -> anyhow::Result<Self> {
        let smtp = if is_legacy && !config.smtp_host.is_empty() {
            Some(SmtpSender::new(&config)?)
        } else {
            None
        };
        Ok(Self {
            attachments,
            metadata_cache,
            persistent: PreparedCache::default(),
            write_guards: Arc::default(),
            config,
            journal,
            account,
            alias,
            smtp,
            backend_backoff: Default::default(),
            #[cfg(test)]
            test_imap_roots: None,
        })
    }

    fn imap_roots(&self) -> RootCertStore {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        #[cfg(test)]
        let roots = self.test_imap_roots.clone().unwrap_or(roots);
        roots
    }

    fn backend_ready(&self) -> Result<()> {
        let mut deadline = self
            .backend_backoff
            .lock()
            .map_err(|_| MailError::backend())?;
        if deadline.is_some_and(|until| until > std::time::Instant::now()) {
            return Err(MailError {
                code: "BUSY",
                message: "The mailbox connection is cooling down after a backend failure. Retry this read later.",
                retryable: true,
                details: None,
            });
        }
        *deadline = None;
        Ok(())
    }
    fn record_backend_failure(&self) {
        if let Ok(mut deadline) = self.backend_backoff.lock() {
            *deadline = Some(std::time::Instant::now() + BACKEND_BACKOFF);
        }
    }
    fn backend_timeout(&self) -> MailError {
        self.record_backend_failure();
        MailError::timeout()
    }

    async fn connect(&self) -> Result<Session> {
        self.connect_for(false, None).await
    }
    async fn connect_read(&self) -> Result<Session> {
        self.connect_for(true, None).await
    }
    async fn connect_for(
        &self,
        read: bool,
        remaining: Option<Arc<AtomicUsize>>,
    ) -> Result<Session> {
        let result = self.connect_inner(read, remaining).await;
        if result
            .as_ref()
            .is_err_and(|error| error.code == "MAIL_BACKEND")
        {
            self.record_backend_failure();
        }
        result
    }

    async fn connect_inner(
        &self,
        read: bool,
        remaining: Option<Arc<AtomicUsize>>,
    ) -> Result<Session> {
        let remaining =
            remaining.unwrap_or_else(|| Arc::new(AtomicUsize::new(self.config.max_message * 4)));
        let budget = remaining.load(Ordering::Relaxed);
        if budget == 0 {
            return Err(MailError::limit_kind(LimitKind::ConnectionBytes));
        }
        let tls = ClientConfig::builder()
            .with_root_certificates(self.imap_roots())
            .with_no_client_auth();
        let tcp = TcpStream::connect((&*self.config.imap_host, self.config.imap_port))
            .await
            .map_err(|_| MailError::backend())?;
        // IMAP commands are sent as several small writes before waiting for
        // the tagged reply. Avoid a Nagle delay on each sequential command.
        tcp.set_nodelay(true).map_err(|_| MailError::backend())?;
        let name =
            ServerName::try_from(self.config.imap_host.clone()).map_err(|_| MailError::input())?;
        let stream = TlsConnector::from(Arc::new(tls))
            .connect(name, tcp)
            .await
            .map_err(|_| MailError::backend())?;
        let stream = BudgetStream::with_remaining(stream, remaining);
        let mut client = async_imap::Client::with_buffer_limit(stream, budget);
        client.read_response().await.map_err(|error| {
            if read {
                map_read_imap_error(error.into())
            } else {
                MailError::backend()
            }
        })?;
        client
            .login(&self.config.address, &self.config.password)
            .await
            .map_err(|(error, _)| {
                if read {
                    map_read_imap_error(error)
                } else {
                    MailError::backend()
                }
            })
    }

    fn account_discovery(&self) -> Value {
        let mut discovery = json!({
            "default_account_id":"default",
            "accounts":[{
                "id":"default", "address":self.config.address, "mode":self.config.mode, "default":true,
                "capabilities":{
                    "attachment_delivery_modes":["inline_base64","authenticated_http"],
                    "part_fetch":true, "cross_mailbox_conversation":true, "cache_modes":["off"]
                },
                "limits":{
                    "mailbox_characters":ReadInputLimits::MAILBOX_CHARACTERS,
                    "query_characters":ReadInputLimits::QUERY_CHARACTERS,
                    "text_utf8_bytes":ReadInputLimits::TEXT_UTF8_BYTES,
                    "mailbox_modified_utf7_bytes":ReadInputLimits::MAILBOX_WIRE_BYTES,
                    "raw_message_bytes":self.config.max_message,
                    "connection_bytes":self.config.max_message * 4,
                    "wire_response_bytes":self.config.max_output,
                    "inline_attachment_decoded_bytes":self.config.max_output / 4,
                    "batch_read_items":BATCH_READ_ITEMS, "list_page_items":LIST_PAGE_ITEMS,
                    "scan_uid_window":SCAN_UID_WINDOW, "execution_slots":EXECUTION_SLOTS,
                    "admission_requests":ADMISSION_REQUESTS,
                    "per_account_base_slots":scheduler::ACCOUNT_BASE_SLOTS,
                    "background_slots":scheduler::BACKGROUND_SLOTS,
                    "queue_wait_seconds":scheduler::QUEUE_WAIT.as_secs(),
                    "backend_backoff_seconds":BACKEND_BACKOFF.as_secs(),
                    "operation_timeout_seconds":self.config.timeout.as_secs()
                }
            }]
        });
        if let (Some(limits), Some(conversation)) = (
            discovery["accounts"][0]["limits"].as_object_mut(),
            conversation::public_limits(&self.config).as_object_mut(),
        ) {
            limits.append(conversation);
        }
        discovery
    }
    pub async fn execute(&self, name: &str, args: &Value, principal: &str) -> Result<Value> {
        if !["send_email", "send_draft", "reply_email", "forward_email"].contains(&name) {
            validate_legacy_write_inputs(name, args, &self.config)?;
        }
        if name == "list_accounts" {
            return Ok(self.account_discovery());
        }
        if name == "get_operation" {
            return Ok(
                json!({"operation":self.journal.get(principal,text(args,"operation_key")?)?}),
            );
        }
        // Sending has its own timeout and durable outcome handling. Never cancel it at the HTTP boundary.
        if ["send_email", "send_draft", "reply_email", "forward_email"].contains(&name) {
            return self.send(name, args, principal).await;
        }
        if ["prepare_attachment_download", "list_attachments"].contains(&name) {
            return tokio::time::timeout(
                self.config.timeout,
                self.prepare_attachment(name, args, principal),
            )
            .await
            .map_err(|_| self.backend_timeout())?;
        }
        if name == "diagnose_connectivity" {
            return self.diagnose_connectivity().await;
        }
        if ["mark_emails", "move_emails", "delete_emails"].contains(&name) {
            let ids = batch_ids(args)?;
            folder(args, InputLimits::LegacyWrite)?;
            if name == "mark_emails"
                && (!["seen", "flagged", "answered"].contains(&text(args, "flag")?)
                    || !args["value"].is_boolean())
            {
                return Err(MailError::input());
            }
            if name == "move_emails" {
                quote(text(args, "destination")?)?;
            }
            if args.get("permanent").is_some_and(|v| !v.is_boolean()) {
                return Err(MailError::input());
            }
            if name == "delete_emails" && args["confirm"] != true {
                return Err(MailError::input());
            }
            let deadline = tokio::time::Instant::now() + self.config.timeout;
            let connection = tokio::time::timeout_at(deadline, self.connect()).await;
            return match connection {
                Ok(Ok(mut s)) => self.batch(&mut s, name, args, deadline).await,
                _ => Ok(batch_unprocessed(&ids)),
            };
        }
        if matches!(name, "list_emails" | "search_emails") {
            let deadline = tokio::time::Instant::now() + self.config.timeout;
            return tokio::time::timeout_at(deadline, self.list_operation(name, args, deadline))
                .await
                .map_err(|_| self.backend_timeout())?;
        }
        tokio::time::timeout(self.config.timeout, self.imap_operation(name, args))
            .await
            .map_err(|_| self.backend_timeout())?
    }
    async fn imap_operation(&self, name: &str, a: &Value) -> Result<Value> {
        if matches!(name, "get_email" | "get_emails" | "read_conversation") {
            return Box::pin(self.body_operation(name, a)).await;
        }
        let mut s = if runtime::account_read(name) {
            self.connect_read().await?
        } else {
            self.connect().await?
        };
        let result = self.with_session(&mut s, name, a).await;
        // Dropping the connection avoids CLOSE/EXPUNGE side effects and bounds logout latency.
        result
    }
    async fn with_session<S>(
        &self,
        s: &mut async_imap::Session<S>,
        name: &str,
        a: &Value,
    ) -> Result<Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        Box::pin(self.with_session_guarded(s, name, a, &mut None)).await
    }

    async fn with_session_guarded<S>(
        &self,
        s: &mut async_imap::Session<S>,
        name: &str,
        a: &Value,
        dirty: &mut Option<Option<cache_write_guard::DirtyWriteGuard>>,
    ) -> Result<Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        validate_legacy_write_inputs(name, a, &self.config)?;
        let imap_error = |error| {
            if runtime::account_read(name) {
                map_read_imap_error(error)
            } else {
                MailError::backend()
            }
        };
        match name {
            "locate_message" | "get_mailbox_statistics" => self.insight_operation(s, name, a).await,
            "list_mailboxes" => {
                let list = s
                    .list(Some(""), Some("*"))
                    .await
                    .map_err(imap_error)?
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(imap_error)?;
                if list.len() > 200 {
                    return Err(MailError::limit_with(LimitKind::ScanItems, 200, list.len()));
                }
                let mailboxes = list
                    .iter()
                    .map(|m| mailbox_metadata(m, &self.config))
                    .collect::<Result<Vec<_>>>()?;
                self.persist_mailboxes(&mailboxes);
                Ok(json!({"mailboxes":mailboxes}))
            }
            "get_email_status" | "download_attachment" => {
                let folder = folder(a, InputLimits::Read)?;
                let uid = uid(a)?;
                let state = s
                    .examine(mailbox_name(folder, InputLimits::Read)?)
                    .await
                    .map_err(imap_error)?;
                self.observe_mailbox(folder, &state);
                check_validity(a, state.uid_validity)?;
                let observe = |error| {
                    self.observe_fetch_error(folder, state.uid_validity, state.uid_next, uid, error)
                };
                if name == "get_email_status" {
                    return email_status(s, uid, state.uid_validity)
                        .await
                        .map_err(observe);
                }
                let raw = self.raw_attachment(s, uid).await.map_err(observe)?;
                let value = attachments::inline_download(
                    &raw,
                    a,
                    &self.account,
                    folder,
                    uid,
                    state.uid_validity.ok_or_else(MailError::backend)?,
                    self.config.max_output / 4,
                );
                if let Some(permit) = self.content_capture(folder) {
                    if let Ok(part) = mime::parse(&raw) {
                        self.persist_raw(folder, state.uid_validity, uid, &raw, &part);
                    }
                    self.capture_raw(folder, state.uid_validity, uid, raw, permit);
                }
                value
            }
            "save_draft" => save_draft(s, &self.config, a, Some(self)).await,
            "create_mailbox" => {
                let name = text(a, "name")?;
                let wire = mailbox_name(name, InputLimits::LegacyWrite)?;
                let _dirty = self.cache_before_write(&[name]).await;
                s.create(wire).await.map_err(imap_error)?;
                Ok(json!({"status":"created"}))
            }
            "rename_mailbox" => {
                let source = mailbox_name(text(a, "name")?, InputLimits::LegacyWrite)?;
                let destination = mailbox_name(text(a, "new_name")?, InputLimits::LegacyWrite)?;
                let _dirty = self.cache_before_folder_tree_write().await;
                s.rename(source, destination).await.map_err(imap_error)?;
                Ok(json!({"status":"renamed"}))
            }
            "delete_mailbox" => {
                if a["confirm"] != true {
                    return Err(MailError::input());
                }
                let wire = mailbox_name(text(a, "name")?, InputLimits::LegacyWrite)?;
                let _dirty = self.cache_before_folder_tree_write().await;
                s.delete(wire).await.map_err(imap_error)?;
                Ok(json!({"status":"deleted"}))
            }
            "move_email" | "delete_email" | "mark_email" => {
                let source = folder(a, InputLimits::LegacyWrite)?;
                let state = s
                    .select(mailbox_name(source, InputLimits::LegacyWrite)?)
                    .await
                    .map_err(imap_error)?;
                self.observe_mailbox(source, &state);
                check_validity(a, state.uid_validity)?;
                let id = uid(a)?;
                let uid = id.to_string();
                let mut fetched = s.uid_fetch(&uid, "(UID)").await.map_err(imap_error)?;
                let mut found = false;
                let mut has_fetch = false;
                while let Some(row) = fetched.try_next().await.map_err(imap_error)? {
                    has_fetch = true;
                    found |= row.uid == Some(id);
                }
                drop(fetched);
                if !found {
                    if !has_fetch {
                        self.cache_tombstone(source, state.uid_validity, state.uid_next, id);
                    }
                    return Err(fetch_attribute_error(true, false));
                }
                let confirmed = match name {
                    "mark_email" => {
                        let flag = match text(a, "flag")? {
                            "seen" => "\\Seen",
                            "flagged" => "\\Flagged",
                            "answered" => "\\Answered",
                            _ => return Err(MailError::input()),
                        };
                        let wanted = a["value"].as_bool().ok_or_else(MailError::input)?;
                        let op = if wanted { "+FLAGS" } else { "-FLAGS" };
                        if dirty.is_none() {
                            *dirty = Some(self.cache_before_write(&[source]).await);
                        }
                        let mut stored = s
                            .uid_store(&uid, format!("{op} ({flag})"))
                            .await
                            .map_err(imap_error)?;
                        let flag = async_imap::types::Flag::from(flag);
                        let mut observed = None;
                        while let Some(row) = stored.try_next().await.map_err(imap_error)? {
                            if row.uid == Some(id) && row.has_flags() {
                                observed = Some(row.flags().any(|returned| returned == flag));
                            }
                        }
                        observed == Some(wanted)
                    }
                    "delete_email" if a["permanent"] == true => {
                        if a["confirm"] != true {
                            return Err(MailError::input());
                        }
                        let caps = s.capabilities().await.map_err(imap_error)?;
                        if !caps.has_str("UIDPLUS") {
                            return Err(MailError {
                                code: "UNSUPPORTED",
                                message: "Permanent deletion requires UIDPLUS.",
                                retryable: false,
                                details: None,
                            });
                        }
                        if dirty.is_none() {
                            *dirty = Some(self.cache_before_write(&[source]).await);
                        }
                        let mut stored = s
                            .uid_store(&uid, "+FLAGS.SILENT (\\Deleted)")
                            .await
                            .map_err(imap_error)?;
                        while stored.try_next().await.map_err(imap_error)?.is_some() {}
                        drop(stored);
                        s.uid_expunge(&uid)
                            .await
                            .map_err(imap_error)?
                            .try_for_each(|_| std::future::ready(Ok(())))
                            .await
                            .map_err(imap_error)?;
                        // EXPUNGE reports sequence numbers and can include concurrent removals.
                        false
                    }
                    _ => {
                        let dest = if name == "delete_email" {
                            &self.config.trash
                        } else {
                            text(a, "destination")?
                        };
                        let caps = s.capabilities().await.map_err(imap_error)?;
                        if !caps.has_str("MOVE") {
                            return Err(MailError {
                                code: "UNSUPPORTED",
                                message: "Safe movement requires the server MOVE capability.",
                                retryable: false,
                                details: None,
                            });
                        }
                        let wire = quoted(&mailbox_name(dest, InputLimits::LegacyWrite)?);
                        if dirty.is_none() {
                            *dirty = Some(self.cache_before_write(&[source, dest]).await);
                        }
                        uid_move_confirmed(s, id, &wire).await?
                    }
                };
                if confirmed {
                    Ok(json!({"status":"updated"}))
                } else {
                    Ok(json!({"status":"accepted","retry_allowed":false}))
                }
            }
            _ => Err(MailError::input()),
        }
    }
    async fn batch(
        &self,
        s: &mut Session,
        name: &str,
        a: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<Value> {
        let ids = batch_ids(a)?;
        let single = match name {
            "mark_emails" => "mark_email",
            "move_emails" => "move_email",
            "delete_emails" => "delete_email",
            _ => return Err(MailError::input()),
        };
        let mut executor = SessionBatch {
            service: self,
            session: s,
            name: single,
            args: a,
            dirty: None,
        };
        Ok(run_batch(&mut executor, &ids, deadline).await)
    }

    async fn raw_with_limits<S>(
        &self,
        s: &mut async_imap::Session<S>,
        uid: u32,
        limits: InputLimits,
    ) -> Result<Vec<u8>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        self.raw_with_absence(s, uid, limits, false).await
    }

    async fn raw_attachment<S>(&self, s: &mut async_imap::Session<S>, uid: u32) -> Result<Vec<u8>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        self.raw_with_absence(s, uid, InputLimits::Read, true).await
    }

    async fn raw_with_absence<S>(
        &self,
        s: &mut async_imap::Session<S>,
        uid: u32,
        limits: InputLimits,
        report_absence: bool,
    ) -> Result<Vec<u8>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let imap_error = |error| match limits {
            InputLimits::Read => map_read_imap_error(error),
            InputLimits::LegacyWrite => MailError::backend(),
        };
        let metadata = s
            .uid_fetch(uid.to_string(), "(UID RFC822.SIZE)")
            .await
            .map_err(imap_error)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(imap_error)?;
        let size = metadata
            .iter()
            .filter(|f| f.uid == Some(uid))
            .find_map(|f| f.size)
            .ok_or_else(|| {
                fetch_attribute_error(report_absence, metadata.iter().any(|f| f.uid == Some(uid)))
            })? as usize;
        if size > self.config.max_message {
            return Err(raw_limit(limits, self.config.max_message, size));
        }
        let f = s
            .uid_fetch(
                uid.to_string(),
                format!("(UID BODY.PEEK[]<0.{}>)", self.config.max_message + 1),
            )
            .await
            .map_err(imap_error)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(imap_error)?;
        let body = f
            .iter()
            .filter(|f| f.uid == Some(uid))
            .find_map(|f| f.body())
            .ok_or_else(|| {
                fetch_attribute_error(report_absence, f.iter().any(|f| f.uid == Some(uid)))
            })?;
        if body.len() > self.config.max_message {
            return Err(raw_limit(limits, self.config.max_message, body.len()));
        }
        // Attachment delivery requires the complete RFC 2822 message, even when
        // the server successfully completes a syntactically valid short literal.
        if report_absence && body.len() != size {
            return Err(MailError::backend());
        }
        Ok(body.to_vec())
    }
    async fn send(&self, name: &str, a: &Value, principal: &str) -> Result<Value> {
        let key = text(a, "operation_key")?;
        if key.len() > 128 {
            return Err(MailError::input());
        }
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!([name, a])).map_err(|_| MailError::input())?)
        );
        if let Some(old) = self.journal.get(principal, key)? {
            // claim verifies the fingerprint even on retries after the draft was removed.
            return Ok(
                json!({"operation":self.journal.claim(principal,key,&fingerprint,&old.message_id)?.ok_or_else(MailError::storage)?}),
            );
        }
        let smtp = self
            .smtp
            .as_ref()
            .ok_or_else(MailError::account_unavailable)?;
        validate_legacy_write_inputs(name, a, &self.config)?;
        let message_id = format!("<{}@email-mcp.local>", uuid::Uuid::new_v4());
        let (raw, envelope) = if name == "send_draft" {
            let raw = tokio::time::timeout(self.config.timeout, async {
                let mut s = self.connect().await?;
                let state = s
                    .examine(mailbox_name(&self.config.drafts, InputLimits::LegacyWrite)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                self.raw_with_limits(&mut s, uid(a)?, InputLimits::LegacyWrite)
                    .await
            })
            .await
            .map_err(|_| MailError::timeout())??;
            prepare_draft(&raw, &self.config.address, &message_id)?
        } else if name == "reply_email" || name == "forward_email" {
            let original = tokio::time::timeout(self.config.timeout, async {
                let mut s = self.connect().await?;
                let state = s
                    .examine(mailbox_name(
                        folder(a, InputLimits::LegacyWrite)?,
                        InputLimits::LegacyWrite,
                    )?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                self.raw_with_limits(&mut s, uid(a)?, InputLimits::LegacyWrite)
                    .await
            })
            .await
            .map_err(|_| MailError::timeout())??;
            let headers = mime::headers(&original)?;
            let mut args = a.clone();
            let prefix = if name == "reply_email" { "Re:" } else { "Fwd:" };
            args["subject"] = json!(format!(
                "{prefix} {}",
                headers.get_first_value("Subject").unwrap_or_default()
            ));
            if name == "reply_email" {
                args["in_reply_to"] =
                    json!(headers.get_first_value("Message-ID").unwrap_or_default());
            }
            if name == "forward_email" {
                args["body"] = json!(forward_body(&original, text(a, "body")?)?);
            }
            compose(&args, &self.config.address, Some(&message_id))?
        } else {
            compose(a, &self.config.address, Some(&message_id))?
        };
        if let Some(old) = self.claim_send(principal, key, &fingerprint, &message_id, &raw)? {
            return Ok(json!({"operation":old}));
        }
        let _dirty = self.cache_before_send().await;
        let submission = smtp.submit(&envelope, &raw).await;
        let operation = record_send(
            &self.journal,
            principal,
            key,
            submission.status,
            message_id,
            name == "send_draft",
        );
        submission.cleanup().await;
        Ok(json!({"operation":operation,"retry_allowed":false}))
    }

    fn claim_send(
        &self,
        principal: &str,
        key: &str,
        fingerprint: &str,
        message_id: &str,
        raw: &[u8],
    ) -> Result<Option<Operation>> {
        if raw.len() > self.config.max_message {
            return Err(MailError::limit());
        }
        crate::smtp::validate_data(raw)?;
        self.journal.claim(principal, key, fingerprint, message_id)
    }
}

trait BatchExecutor {
    fn apply(&mut self, uid: u32) -> impl std::future::Future<Output = Result<Value>> + Send;
}
struct SessionBatch<'a> {
    service: &'a MailService,
    session: &'a mut Session,
    name: &'a str,
    args: &'a Value,
    dirty: Option<Option<cache_write_guard::DirtyWriteGuard>>,
}
impl BatchExecutor for SessionBatch<'_> {
    async fn apply(&mut self, id: u32) -> Result<Value> {
        let mut args = self.args.clone();
        args["uid"] = json!(id);
        args.as_object_mut().unwrap().remove("uids");
        match Box::pin(self.service.with_session_guarded(
            self.session,
            self.name,
            &args,
            &mut self.dirty,
        ))
        .await
        {
            Ok(mut result) => {
                result["uid"] = json!(id);
                Ok(result)
            }
            Err(error) if error.code == "NOT_FOUND" => Ok(json!({"uid":id,"status":"not_found"})),
            Err(error) => Err(error),
        }
    }
}
async fn run_batch(
    executor: &mut impl BatchExecutor,
    ids: &[u32],
    deadline: tokio::time::Instant,
) -> Value {
    let mut items = Vec::new();
    let mut stopped = false;
    for id in ids {
        if tokio::time::Instant::now() >= deadline {
            stopped = true;
        }
        if stopped {
            items.push(json!({"uid":id,"status":"unprocessed","retry_allowed":false}));
            continue;
        }
        match tokio::time::timeout_at(deadline, executor.apply(*id)).await {
            Ok(Ok(item)) => items.push(item),
            Ok(Err(error)) => {
                items
                    .push(json!({"uid":id,"status":"unknown","error":error,"retry_allowed":false}));
                stopped = true;
            }
            Err(_) => {
                items.push(json!({"uid":id,"status":"unknown","error":MailError::timeout(),"retry_allowed":false}));
                stopped = true;
            }
        }
    }
    json!({"items":items,"requested":ids.len(),"retry_allowed":false})
}

fn page_cursor(
    start: u32,
    ids: &[u32],
    limit: usize,
    more: bool,
    postfilter: bool,
    last_examined: Option<u32>,
) -> Option<u32> {
    if postfilter && ids.len() == limit {
        last_examined
    } else if more {
        ids.last().copied()
    } else if start > 1 {
        Some(start)
    } else {
        None
    }
}
fn quote(value: &str) -> Result<String> {
    if value.len() > 998 || value.contains(['\r', '\n', '\0']) {
        return Err(MailError::input());
    }
    Ok(quoted(value))
}
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
fn imap_date(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return Err(MailError::input());
    }
    let year: u32 = value[..4].parse().map_err(|_| MailError::input())?;
    let month: usize = value[5..7].parse().map_err(|_| MailError::input())?;
    let day: u32 = value[8..].parse().map_err(|_| MailError::input())?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if year == 0 || !(1..=12).contains(&month) || day == 0 || day > days[month - 1] {
        return Err(MailError::input());
    }
    Ok(format!(
        "{day}-{}-{year:04}",
        [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
        ][month - 1]
    ))
}
fn search_query(a: &Value, start: u32, end: u32, search: bool) -> Result<String> {
    let mut keys = Vec::new();
    keys.push(format!("UID {start}:{end}"));
    if search {
        for (field, key) in [
            ("query", "TEXT"),
            ("from", "FROM"),
            ("to", "TO"),
            ("subject", "SUBJECT"),
        ] {
            if a.get(field).is_some() {
                keys.push(format!("{key} {}", quote(text(a, field)?)?));
            }
        }
        for (field, key) in [("since", "SINCE"), ("before", "BEFORE")] {
            if a.get(field).is_some() {
                keys.push(format!("{key} {}", imap_date(text(a, field)?)?));
            }
        }
        if let (Some(since), Some(before)) = (a["since"].as_str(), a["before"].as_str())
            && since >= before
        {
            return Err(MailError::input());
        }
        for (field, yes, no) in [
            ("seen", "SEEN", "UNSEEN"),
            ("flagged", "FLAGGED", "UNFLAGGED"),
            ("answered", "ANSWERED", "UNANSWERED"),
        ] {
            if let Some(v) = a.get(field) {
                keys.push(
                    if v.as_bool().ok_or_else(MailError::input)? {
                        yes
                    } else {
                        no
                    }
                    .into(),
                );
            }
        }
        let min = a
            .get("min_size")
            .map(|_| number(a, "min_size", 0, 0, u32::MAX as u64))
            .transpose()?;
        let max = a
            .get("max_size")
            .map(|_| number(a, "max_size", 0, 0, u32::MAX as u64))
            .transpose()?;
        if min.zip(max).is_some_and(|(min, max)| min > max) {
            return Err(MailError::input());
        }
        if let Some(n) = min
            && n > 0
        {
            keys.push(format!("LARGER {}", n - 1));
        }
        if let Some(n) = max
            && n < u32::MAX as u64
        {
            keys.push(format!("SMALLER {}", n + 1));
        }
    }
    Ok(keys.join(" "))
}
fn batch_ids(a: &Value) -> Result<Vec<u32>> {
    number(a, "uid_validity", 0, 1, u32::MAX as u64)?;
    let list = a["uids"].as_array().ok_or_else(MailError::input)?;
    if list.is_empty() || list.len() > 50 {
        return Err(MailError::input());
    }
    let mut ids = Vec::new();
    for id in list {
        let id = id
            .as_u64()
            .filter(|n| *n > 0 && *n <= u32::MAX as u64)
            .ok_or_else(MailError::input)? as u32;
        if ids.contains(&id) {
            return Err(MailError::input());
        }
        ids.push(id);
    }
    Ok(ids)
}
fn batch_unprocessed(ids: &[u32]) -> Value {
    json!({"items":ids.iter().map(|id|json!({"uid":id,"status":"unprocessed","retry_allowed":false})).collect::<Vec<_>>(),"requested":ids.len(),"error":MailError::backend(),"retry_allowed":false})
}
fn record_send(
    journal: &Journal,
    principal: &str,
    key: &str,
    status: &str,
    message_id: String,
    is_draft: bool,
) -> Operation {
    let mut warnings = Vec::new();
    if status == "accepted" && is_draft {
        warnings.push("draft_retained".into());
    }
    if journal.finish(principal, key, status, &warnings).is_err() {
        warnings.push("journal_update_failed".into());
        tracing::error!(event = "send_journal_failed", outcome = status);
    }
    Operation {
        status: status.into(),
        message_id,
        warnings,
    }
}

pub fn text<'a>(a: &'a Value, key: &str) -> Result<&'a str> {
    let s = a
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(MailError::input)?;
    if s.len() > 65536 || s.contains('\0') {
        return Err(MailError::input());
    }
    Ok(s)
}
fn folder(a: &Value, limits: InputLimits) -> Result<&str> {
    let f = a.get("mailbox").and_then(Value::as_str).unwrap_or("INBOX");
    limits.mailbox(f, "/mailbox")?;
    Ok(f)
}

fn mailbox_metadata(mailbox: &async_imap::types::Name, config: &Config) -> Result<Value> {
    use async_imap::types::NameAttribute;
    let name = display_name(mailbox.name())?;
    let mut flags = Vec::new();
    let mut roles = Vec::new();
    for attribute in mailbox.attributes() {
        let (flag, role) = match attribute {
            NameAttribute::NoInferiors => ("\\NoInferiors", None),
            NameAttribute::NoSelect => ("\\NoSelect", None),
            NameAttribute::Marked => ("\\Marked", None),
            NameAttribute::Unmarked => ("\\Unmarked", None),
            NameAttribute::All => ("\\All", Some("all")),
            NameAttribute::Archive => ("\\Archive", Some("archive")),
            NameAttribute::Drafts => ("\\Drafts", Some("drafts")),
            NameAttribute::Flagged => ("\\Flagged", Some("flagged")),
            NameAttribute::Junk => ("\\Junk", Some("junk")),
            NameAttribute::Sent => ("\\Sent", Some("sent")),
            NameAttribute::Trash => ("\\Trash", Some("trash")),
            NameAttribute::Extension(value) => (value.as_ref(), None),
            _ => continue,
        };
        if !flags.contains(&flag) {
            flags.push(flag);
        }
        if let Some(role) = role
            && !roles.contains(&role)
        {
            roles.push(role);
        }
    }
    let source = if !roles.is_empty() {
        Some("server")
    } else if name.eq_ignore_ascii_case("INBOX") {
        roles.push("inbox");
        Some("protocol")
    } else {
        if name == config.drafts {
            roles.push("drafts");
        }
        if name == config.trash {
            roles.push("trash");
        }
        if roles.is_empty() {
            None
        } else {
            Some("configuration")
        }
    };
    let candidates = if roles.is_empty() {
        match name.to_lowercase().as_str() {
            "sent" | "sent items" | "sent mail" | "已发送" => vec!["sent"],
            "archive" | "archives" | "归档" => vec!["archive"],
            "drafts" | "草稿" | "草稿箱" => vec!["drafts"],
            "trash" | "deleted items" | "废纸篓" | "已删除" => vec!["trash"],
            "junk" | "spam" | "垃圾邮件" | "垃圾箱" => vec!["junk"],
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    Ok(json!({
        "name":name, "attributes":format!("{:?}",mailbox.attributes()),
        "attribute_flags":flags, "delimiter":mailbox.delimiter(),
        "selectable":!mailbox.attributes().contains(&NameAttribute::NoSelect),
        "special_use":roles, "role_source":source, "role_candidates":candidates
    }))
}
fn raw_limit(limits: InputLimits, limit: usize, actual: usize) -> MailError {
    match limits {
        InputLimits::Read => MailError::limit_with(LimitKind::RawMessageBytes, limit, actual),
        InputLimits::LegacyWrite => MailError::limit(),
    }
}

fn fetch_attribute_error(report_absence: bool, target_present: bool) -> MailError {
    if report_absence && !target_present {
        MailError {
            code: "NOT_FOUND",
            message: "The requested email is no longer available.",
            retryable: false,
            details: None,
        }
    } else {
        MailError::backend()
    }
}
/// Write operations keep their original input budget even when they first read mail.
fn validate_legacy_write_inputs(name: &str, a: &Value, config: &Config) -> Result<()> {
    let legacy = InputLimits::LegacyWrite;
    match name {
        "create_mailbox" | "delete_mailbox" | "rename_mailbox" => {
            mailbox_name(text(a, "name")?, legacy)?;
            if name == "rename_mailbox" {
                mailbox_name(text(a, "new_name")?, legacy)?;
            }
        }
        "mark_email" | "mark_emails" | "move_email" | "move_emails" | "delete_email"
        | "delete_emails" | "reply_email" | "forward_email" => {
            mailbox_name(folder(a, legacy)?, legacy)?;
            if matches!(name, "move_email" | "move_emails") {
                mailbox_name(text(a, "destination")?, legacy)?;
            }
            if matches!(name, "delete_email" | "delete_emails") && a["permanent"] != true {
                mailbox_name(&config.trash, legacy)?;
            }
        }
        "save_draft" | "send_draft" => {
            mailbox_name(&config.drafts, legacy)?;
        }
        _ => {}
    }
    Ok(())
}
fn uid(a: &Value) -> Result<u32> {
    Ok(number(a, "uid", 0, 1, u32::MAX as u64)? as u32)
}
fn number(a: &Value, key: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let n = match a.get(key) {
        None => default,
        Some(v) => v.as_u64().ok_or_else(MailError::input)?,
    };
    if n < min || n > max {
        Err(MailError::input())
    } else {
        Ok(n)
    }
}
fn check_validity(a: &Value, current: Option<u32>) -> Result<()> {
    if let Some(v) = a.get("uid_validity")
        && v.as_u64() != current.map(u64::from)
    {
        return Err(MailError {
            code: "STALE_UID",
            message: "Mailbox UIDVALIDITY changed. List the mailbox again.",
            retryable: false,
            details: None,
        });
    }
    Ok(())
}
fn addresses(a: &Value, key: &str) -> Result<Vec<Mailbox>> {
    match a.get(key) {
        None => Ok(vec![]),
        Some(v) => {
            let list = v.as_array().ok_or_else(MailError::input)?;
            if list.len() > 50 {
                return Err(MailError::input());
            }
            list.iter()
                .map(|v| {
                    v.as_str()
                        .ok_or_else(MailError::input)?
                        .parse()
                        .map_err(|_| MailError::input())
                })
                .collect()
        }
    }
}
async fn uid_move_confirmed<S>(s: &mut async_imap::Session<S>, uid: u32, wire: &str) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    use imap_proto::{Response, ResponseCode, Status, rfc4315::UidSetMember};
    fn singleton(set: &[UidSetMember]) -> Option<u32> {
        match set {
            [UidSetMember::Uid(uid)] => Some(*uid),
            [UidSetMember::UidRange(range)] if range.start() == range.end() => Some(*range.start()),
            _ => None,
        }
        .filter(|uid| *uid > 0)
    }
    let matches = |response: &Response<'_>| {
        let outcome = match response {
            Response::Data {
                status: Status::Ok,
                outcome,
            }
            | Response::Done {
                status: Status::Ok,
                outcome,
                ..
            } => outcome,
            _ => return false,
        };
        matches!(&outcome.code, Some(ResponseCode::CopyUid(validity, source, dest))
            if *validity > 0 && singleton(source) == Some(uid) && singleton(dest).is_some())
    };
    let mut confirmed = false;
    let completion =
        parts::reader::command(s, format!("UID MOVE {uid} {wire}"), None, |response| {
            confirmed |= matches(response.parsed());
            Ok(())
        })
        .await
        .map_err(|_| MailError::backend())?;
    Ok(confirmed || matches(completion.parsed()))
}

async fn email_status<S>(
    s: &mut async_imap::Session<S>,
    uid: u32,
    validity: Option<u32>,
) -> Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let mut fetched = s
        .uid_fetch(uid.to_string(), "(UID FLAGS)")
        .await
        .map_err(map_read_imap_error)?;
    let mut target_seen = false;
    let mut last_flags = None;
    // Keep the last response with explicit FLAGS while awaiting tagged completion.
    while let Some(fetch) = fetched.try_next().await.map_err(map_read_imap_error)? {
        if fetch.uid == Some(uid) {
            target_seen = true;
            if fetch.has_flags() {
                last_flags = Some(fetch);
            }
        }
    }
    let target = last_flags.ok_or_else(|| {
        if target_seen {
            MailError::backend()
        } else {
            MailError {
                code: "NOT_FOUND",
                message: "The requested email UID was not returned by the mailbox.",
                retryable: false,
                details: None,
            }
        }
    })?;
    Ok(
        json!({"uid":uid,"uid_validity":validity,"flags":target.flags().map(|f|format!("{f:?}")).collect::<Vec<_>>()}),
    )
}

async fn save_draft<S>(
    s: &mut async_imap::Session<S>,
    config: &Config,
    a: &Value,
    service: Option<&MailService>,
) -> Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let (raw, _) = compose(a, &config.address, None)?;
    if raw.len() > config.max_message {
        return Err(MailError::limit());
    }
    let mailbox = mailbox_name(&config.drafts, InputLimits::LegacyWrite)?;
    let _dirty = if let Some(service) = service {
        service.cache_before_write(&[&config.drafts]).await
    } else {
        None
    };
    // async-imap serializes flags verbatim; RFC 3501 APPEND requires a flag list.
    s.append(mailbox, Some("(\\Draft)"), None, &raw)
        .await
        .map_err(|_| MailError::backend())?;
    Ok(json!({"status":"saved","mailbox":config.drafts}))
}

pub fn compose(a: &Value, from: &str, id: Option<&str>) -> Result<(Vec<u8>, Envelope)> {
    if a.get("attachments").is_some() {
        return Err(MailError::input());
    }
    let mut b = lettre::Message::builder()
        .from(from.parse().map_err(|_| MailError::input())?)
        .subject(text(a, "subject")?);
    if id.is_none() {
        b = b.keep_bcc();
    }
    if let Some(id) = id {
        b = b.message_id(Some(id.into()));
    }
    if let Some(id) = a.get("in_reply_to").and_then(Value::as_str)
        && !id.is_empty()
    {
        b = b.in_reply_to(id.into());
    }
    for m in addresses(a, "to")? {
        b = b.to(m);
    }
    for m in addresses(a, "cc")? {
        b = b.cc(m);
    }
    for m in addresses(a, "bcc")? {
        b = b.bcc(m);
    }
    let message = b
        .header(lettre::message::header::ContentType::TEXT_PLAIN)
        .body(text(a, "body")?.to_owned())
        .map_err(|_| MailError::input())?;
    Ok((message.formatted(), message.envelope().clone()))
}
// Both envelope extraction and outbound filtering must see identical field names.
// Normalize supported obsolete whitespace syntax; reject ambiguous/malformed
// fields before constructing an envelope or claiming a send operation.
fn canonical_draft_headers(raw: &[u8]) -> Result<(Vec<u8>, usize)> {
    mime::headers(raw)?;
    let mut canonical = Vec::new();
    let mut offset = 0;
    let mut have_field = false;
    for line in raw.split_inclusive(|b| *b == b'\n') {
        offset += line.len();
        let line = line.strip_suffix(b"\n").ok_or_else(MailError::input)?;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            canonical.extend_from_slice(b"\r\n");
            return Ok((canonical, offset));
        }
        if line.iter().any(|b| (*b < 32 && *b != b'\t') || *b == 127) {
            return Err(MailError::input());
        }
        if line.starts_with(b" ") || line.starts_with(b"\t") {
            if !have_field {
                return Err(MailError::input());
            }
            canonical.extend_from_slice(line);
        } else {
            let colon = line
                .iter()
                .position(|b| *b == b':')
                .ok_or_else(MailError::input)?;
            let name = line[..colon].trim_ascii_end();
            if name.is_empty() || name.iter().any(|b| !(33..=126).contains(b) || *b == b':') {
                return Err(MailError::input());
            }
            // Resent blocks have distinct destination semantics. Refuse them
            // until that model is supported rather than silently dropping a
            // hidden recipient or choosing the wrong delivery envelope.
            if name.len() >= 7 && name[..7].eq_ignore_ascii_case(b"resent-") {
                return Err(MailError {
                    code: "UNSUPPORTED_DRAFT_HEADERS",
                    message: "Resent header blocks are not supported for draft sending.",
                    retryable: false,
                    details: None,
                });
            }
            canonical.extend(name.iter().map(u8::to_ascii_lowercase));
            canonical.extend_from_slice(&line[colon..]);
            have_field = true;
        }
        canonical.extend_from_slice(b"\r\n");
    }
    Err(MailError::input())
}

pub fn prepare_draft(raw: &[u8], from: &str, id: &str) -> Result<(Vec<u8>, Envelope)> {
    let (canonical, body_start) = canonical_draft_headers(raw)?;
    let headers = mime::headers(&canonical)?;
    let mut recipients = Vec::new();
    for key in ["To", "Cc", "Bcc"] {
        for header in headers.get_all_headers(key) {
            let list = mailparse::addrparse_header(header).map_err(|_| MailError::input())?;
            for a in list.iter() {
                match a {
                    mailparse::MailAddr::Single(s) => {
                        recipients.push(s.addr.parse().map_err(|_| MailError::input())?)
                    }
                    mailparse::MailAddr::Group(g) => {
                        for s in &g.addrs {
                            recipients.push(s.addr.parse().map_err(|_| MailError::input())?);
                        }
                    }
                }
            }
        }
    }
    if recipients.is_empty() || recipients.len() > 100 {
        return Err(MailError::input());
    }
    let envelope = Envelope::new(
        Some(from.parse().map_err(|_| MailError::input())?),
        recipients,
    )
    .map_err(|_| MailError::input())?;
    // Preserve MIME body byte-for-byte; strip Bcc and old identity headers including folded continuations.
    let headers =
        std::str::from_utf8(&canonical[..canonical.len() - 2]).map_err(|_| MailError::input())?;
    let mut clean = Vec::new();
    let mut skip = false;
    for line in headers.lines() {
        if !line.starts_with([' ', '\t']) {
            let key = line.split(':').next().unwrap_or("");
            skip = [
                "bcc",
                "resent-bcc",
                "from",
                "sender",
                "message-id",
                "return-path",
            ]
            .iter()
            .any(|k| key.eq_ignore_ascii_case(k));
        }
        if !skip {
            clean.extend_from_slice(line.trim_end_matches('\r').as_bytes());
            clean.extend_from_slice(b"\r\n");
        }
    }
    clean.extend_from_slice(format!("From: {from}\r\nMessage-ID: {id}\r\n\r\n").as_bytes());
    clean.extend_from_slice(&raw[body_start..]);
    crate::smtp::validate_data(&clean)?;
    Ok((clean, envelope))
}
fn forward_body(raw: &[u8], note: &str) -> Result<String> {
    Ok(format!(
        "{note}\n\n---------- Forwarded message ----------\n{}",
        mime::parse(raw)?.plain_body()?
    ))
}

fn parsed_metadata(p: &mime::Part<'_>, uid: u32, validity: Option<u32>) -> Result<Value> {
    let manifest = parts::from_raw(p)?;
    Ok(insights::manifest_metadata(
        &p.headers,
        &manifest.manifest,
        uid,
        validity,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) async fn fetch_script(
        script: Vec<(String, String, &'static str)>,
    ) -> (
        async_imap::Session<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (client, server) = tokio::io::duplex(32768);
        let backend = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            let tag = line.split_whitespace().next().unwrap().to_owned();
            server
                .write_all(format!("{tag} OK login\r\n").as_bytes())
                .await
                .unwrap();
            for (expected, rows, status) in script {
                line.clear();
                server.read_line(&mut line).await.unwrap();
                let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
                assert_eq!(command, expected);
                let completion = match status {
                    "EOF" => String::new(),
                    "WRONG_TAG" => "unrelated OK completed\r\n".to_owned(),
                    status => format!("{tag} {status} completed\r\n"),
                };
                server
                    .write_all(format!("{rows}{completion}").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let session = async_imap::Client::new(client)
            .login("synthetic", "test-only")
            .await
            .unwrap();
        (session, backend)
    }

    fn fetch_fixture() -> (tempfile::TempDir, MailService) {
        let dir = tempfile::tempdir().unwrap();
        let service = MailService::new(
            crate::config::fixture(),
            Arc::new(Journal::open(&dir.path().join("db")).unwrap()),
        )
        .unwrap();
        (dir, service)
    }

    #[test]
    fn mixed_inline_body_preserves_leaf_boundaries_in_forward_cache_and_preview() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=m\r\n\r\n",
            "--m\r\n\r\nAmount: 1\r\n",
            "--m\r\n\r\n\r\n",
            "--m\r\nContent-Type: multipart/alternative; boundary=a\r\n\r\n",
            "--a\r\nContent-Type: text/html\r\n\r\n<b>000</b>\r\n",
            "--a\r\nContent-Type: text/html\r\n\r\nDiscard<svg></svg>\r\n",
            "--a--\r\n",
            "--m\r\n\r\n\r\nsignature\r\n",
            "--m--\r\n",
        )
        .as_bytes();
        let expected = "Amount: 1\n000\r\nsignature";
        assert_eq!(
            forward_body(raw, "Note").unwrap(),
            format!("Note\n\n---------- Forwarded message ----------\n{expected}")
        );
        let part = mime::parse(raw).unwrap();
        let mut record = parts::cached_body::Record::default();
        let mut remaining = parts::cached_body::LIMIT;
        part.decoded_display_leaves(&mut remaining, |text, html, reserve| {
            record.push(text, html, reserve);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            record
                .display(crate::html::HtmlDisplay::Legacy)
                .unwrap()
                .text,
            expected
        );
        assert_eq!(
            parts::preview::from_raw(raw, &mut (8 * 1024))
                .map_err(|fault| fault.error)
                .unwrap(),
            parts::preview::Preview::Text {
                text: "Amount: 1 000 signature".into(),
                truncated: false,
            }
        );
    }

    #[tokio::test]
    async fn single_writes_distinguish_missing_targets_and_confirmed_updates() {
        let (_dir, service) = fetch_fixture();
        let select = ("SELECT \"INBOX\"", "* OK [UIDVALIDITY 9] valid\r\n", "OK");
        let target = ("UID FETCH 7 (UID)", "* 1 FETCH (UID 7)\r\n", "OK");
        let missing = ("UID FETCH 7 (UID)", "", "OK");
        let move_cap = ("CAPABILITY", "* CAPABILITY IMAP4rev1 MOVE\r\n", "OK");
        let delete_cap = ("CAPABILITY", "* CAPABILITY IMAP4rev1 UIDPLUS\r\n", "OK");
        let mark = "UID STORE 7 +FLAGS (\\Seen)";
        let mv = "UID MOVE 7 \"Archive\"";
        let del = "UID STORE 7 +FLAGS.SILENT (\\Deleted)";
        let expunge = "UID EXPUNGE 7";
        let cases = [
            ("mark_email", vec![missing], Err("NOT_FOUND")),
            ("move_email", vec![missing], Err("NOT_FOUND")),
            (
                "delete_email",
                vec![
                    target,
                    delete_cap,
                    (del, "", "OK"),
                    (expunge, "* 1 EXPUNGE\r\n", "OK"),
                    select,
                    missing,
                ],
                Ok("accepted"),
            ),
            (
                "mark_email",
                vec![target, (mark, "* 2 FETCH (UID 8 FLAGS (\\Seen))\r\n", "OK")],
                Ok("accepted"),
            ),
            (
                "mark_email",
                vec![target, (mark, "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n", "OK")],
                Ok("updated"),
            ),
            (
                "move_email",
                vec![target, move_cap, (mv, "* 1 EXPUNGE\r\n", "OK")],
                Ok("accepted"),
            ),
            (
                "move_email",
                vec![
                    target,
                    move_cap,
                    (mv, "* OK [COPYUID 9 7 42] copied\r\n", "OK"),
                ],
                Ok("updated"),
            ),
            (
                "move_email",
                vec![target, move_cap, (mv, "", "OK [COPYUID 9 7:7 42:42]")],
                Ok("updated"),
            ),
            (
                "move_email",
                vec![
                    target,
                    move_cap,
                    (mv, "", "OK [COPYUID 9 1:4294967295 1:4294967295]"),
                ],
                Ok("accepted"),
            ),
            (
                "move_email",
                vec![
                    target,
                    move_cap,
                    (mv, "* OK [COPYUID 9 7 42] copied\r\n", "NO"),
                ],
                Err("MAIL_BACKEND"),
            ),
            (
                "mark_email",
                vec![
                    target,
                    (mark, "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n", "BAD"),
                ],
                Err("MAIL_BACKEND"),
            ),
            (
                "mark_email",
                vec![("UID FETCH 7 (UID)", "* 1 FETCH (UID 7)\r\n", "NO")],
                Err("MAIL_BACKEND"),
            ),
        ];
        for (name, commands, expected) in cases {
            let script = [select]
                .into_iter()
                .chain(commands)
                .map(|(command, rows, status)| (command.into(), rows.into(), status))
                .collect();
            let (mut session, backend) = fetch_script(script).await;
            let args = json!({"mailbox":"INBOX","uid":7,"uid_validity":9,"flag":"seen","value":true,
                "destination":"Archive","permanent":true,"confirm":true});
            let result = service.with_session(&mut session, name, &args).await;
            let outcome = result
                .as_ref()
                .map(|value| value["status"].as_str().unwrap())
                .map_err(|error| error.code);
            assert_eq!(outcome, expected, "{name}");
            if outcome == Ok("accepted") {
                assert_eq!(result.unwrap()["retry_allowed"], false);
            }
            if name == "delete_email" {
                let repeated = service.with_session(&mut session, name, &args).await;
                assert_eq!(repeated.unwrap_err().code, "NOT_FOUND");
            }
            backend.await.unwrap();
        }
    }

    #[tokio::test]
    async fn email_status_uses_last_target_flags_after_matching_completion() {
        let (_dir, service) = fetch_fixture();
        let updates = "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n* 1 FETCH (UID 7 FLAGS (\\Flagged))\r\n";
        let size = "* 1 FETCH (UID 7 RFC822.SIZE 100)\r\n";
        let unrelated = "* 2 FETCH (UID 8 FLAGS (\\Deleted))\r\n* 3 FETCH (FLAGS (\\Answered))\r\n";
        for (rows, completion, expected) in [
            (format!("{updates}{size}"), "OK", Ok(json!(["Flagged"]))),
            (
                format!("{updates}* 1 FETCH (UID 7 FLAGS ())\r\n{size}"),
                "OK",
                Ok(json!([])),
            ),
            (size.to_owned(), "OK", Err("MAIL_BACKEND")),
            (String::new(), "OK", Err("NOT_FOUND")),
            (updates.to_owned(), "NO", Err("MAIL_BACKEND")),
            (updates.to_owned(), "BAD", Err("MAIL_BACKEND")),
            (updates.to_owned(), "EOF", Err("MAIL_BACKEND")),
        ] {
            let (mut session, backend) = fetch_script(vec![
                (
                    "EXAMINE \"INBOX\"".into(),
                    "* 3 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n".into(),
                    "OK",
                ),
                (
                    "UID FETCH 7 (UID FLAGS)".into(),
                    format!("{rows}{unrelated}"),
                    completion,
                ),
            ])
            .await;
            let result = service
                .with_session(
                    &mut session,
                    "get_email_status",
                    &json!({"uid":7,"uid_validity":9}),
                )
                .await
                .map_err(|error| error.code);
            backend.await.unwrap();
            let expected = expected.map(|flags| json!({"uid":7,"uid_validity":9,"flags":flags}));
            assert_eq!(result, expected, "{completion} with {rows:?}");
        }
    }

    #[tokio::test]
    async fn raw_fetch_ignores_interleaved_flags_and_requires_target_attributes() {
        let (_dir, service) = fetch_fixture();
        let raw = "Subject: synthetic\r\n\r\nbody";
        let size_command = "UID FETCH 7 (UID RFC822.SIZE)".to_owned();
        let body_command = format!(
            "UID FETCH 7 (UID BODY.PEEK[]<0.{}>)",
            service.config.max_message + 1
        );
        let flags = "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n";
        let size = format!("* 1 FETCH (UID 7 RFC822.SIZE {})\r\n", raw.len());
        let body = format!("* 1 FETCH (UID 7 BODY[] {{{}}}\r\n{raw})\r\n", raw.len());
        for size_first in [true, false] {
            for body_first in [true, false] {
                let size_rows = if size_first {
                    format!("{size}{flags}")
                } else {
                    format!("{flags}{size}")
                };
                let body_rows = if body_first {
                    format!("{body}{flags}")
                } else {
                    format!("{flags}{body}")
                };
                let unrelated = "* 2 FETCH (UID 99 RFC822.SIZE 99999999)\r\n* 3 FETCH (RFC822.SIZE 99999999)\r\n";
                let (mut session, backend) = fetch_script(vec![
                    (size_command.clone(), format!("{unrelated}{size_rows}"), "OK"),
                    (body_command.clone(), format!("* 2 FETCH (UID 99 BODY[] {{5}}\r\nother)\r\n* 3 FETCH (BODY[] {{5}}\r\nother)\r\n{body_rows}"), "OK"),
                ]).await;
                assert_eq!(
                    service
                        .raw_with_limits(&mut session, 7, InputLimits::Read)
                        .await
                        .unwrap(),
                    raw.as_bytes()
                );
                backend.await.unwrap();
            }
        }
        for missing_body in [true, false] {
            let mut script = vec![(
                size_command.clone(),
                if missing_body {
                    size.clone()
                } else {
                    flags.to_owned()
                },
                "OK",
            )];
            if missing_body {
                script.push((body_command.clone(), flags.to_owned(), "OK"));
            }
            let (mut session, backend) = fetch_script(script).await;
            assert_eq!(
                service
                    .raw_with_limits(&mut session, 7, InputLimits::Read)
                    .await
                    .unwrap_err()
                    .code,
                "MAIL_BACKEND"
            );
            backend.await.unwrap();
        }
        for status in ["NO", "BAD", "EOF", "WRONG_TAG"] {
            for body_failure in [true, false] {
                let mut script = vec![(
                    size_command.clone(),
                    size.clone(),
                    if body_failure { "OK" } else { status },
                )];
                if body_failure {
                    script.push((body_command.clone(), body.clone(), status));
                }
                let (mut session, backend) = fetch_script(script).await;
                assert_eq!(
                    service
                        .raw_with_limits(&mut session, 7, InputLimits::Read)
                        .await
                        .unwrap_err()
                        .code,
                    "MAIL_BACKEND"
                );
                backend.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn list_fetch_merges_uid_attributes_and_only_actual_flag_updates() {
        let (_dir, service) = fetch_fixture();
        let header = "Subject: synthetic\r\nFrom: sender@example.invalid\r\n\r\n";
        let full = format!(
            "* 1 FETCH (UID 7 FLAGS (\\Seen) RFC822.SIZE 100 BODY[HEADER]<0> {{{}}}\r\n{header})\r\n",
            header.len()
        );
        for tool in ["list_emails", "search_emails"] {
            for update in ["\\fLaGgEd", ""] {
                let rows = format!(
                    "* 2 FETCH (UID 99 FLAGS (\\Deleted))\r\n* 3 FETCH (FLAGS (\\Answered))\r\n* 1 FETCH (UID 7 FLAGS (\\Answered))\r\n{full}* 1 FETCH (UID 7 FLAGS ({update}))\r\n* 1 FETCH (UID 7 RFC822.SIZE 101)\r\n* 1 FETCH (UID 7)\r\n"
                );
                let (mut session, backend) = fetch_script(vec![
                    (
                        "EXAMINE \"INBOX\"".into(),
                        "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n"
                            .into(),
                        "OK",
                    ),
                    ("UID SEARCH UID 1:7".into(), "* SEARCH 7\r\n".into(), "OK"),
                    (
                        "UID FETCH 7 (UID FLAGS RFC822.SIZE BODY.PEEK[HEADER]<0.1048576>)".into(),
                        rows,
                        "OK",
                    ),
                ])
                .await;
                let emails = list_summaries(&service, &mut session, 1, tool == "search_emails")
                    .await
                    .unwrap();
                backend.await.unwrap();
                assert_eq!(emails.len(), 1);
                assert_eq!(emails[0]["uid"], 7);
                assert_eq!(emails[0]["subject"], "synthetic");
                assert_eq!(emails[0]["header_complete"], true);
                assert_eq!(emails[0]["size"], 101);
                assert_eq!(
                    emails[0]["flags"],
                    if update.is_empty() {
                        json!([])
                    } else {
                        json!(["Flagged"])
                    }
                );
            }
        }
    }

    fn padded_header(prefix: &str, tail: &str, bytes: usize) -> String {
        const LINE_BYTES: usize = 998;
        let mut header = format!("{prefix}X-Pad:\r\n");
        let remaining = bytes - header.len() - tail.len();
        let folds = (remaining - 3) / LINE_BYTES;
        let fold = format!(" {}\r\n", "x".repeat(LINE_BYTES - 3));
        header.push_str(&fold.repeat(folds));
        header.push(' ');
        header.push_str(&"x".repeat(remaining - folds * LINE_BYTES - 3));
        header.push_str("\r\n");
        header.push_str(tail);
        header
    }

    #[tokio::test]
    async fn listing_reports_header_coverage_without_exposing_a_clipped_field() {
        const SAMPLE: usize = 1024 * 1024;
        const FROM: &str = "From: sender@example.invalid\r\n";
        let (_dir, service) = fetch_fixture();
        for (tail, expected_complete) in [
            ("Subject: clipped", false),
            ("Subject: may continue\r\n", false),
            ("Subject: =?UTF-8?Q?known?=\r\n =?UTF-8?Q?clip", false),
            ("\r\n", true),
        ] {
            let header = padded_header(FROM, tail, SAMPLE);
            let rows = format!(
                "* 1 FETCH (UID 7 FLAGS () BODY[HEADER]<0> {{{}}}\r\n{header})\r\n",
                header.len(),
            );
            let (mut session, backend) = fetch_script(vec![
                (
                    "EXAMINE \"INBOX\"".into(),
                    "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n".into(),
                    "OK",
                ),
                ("UID SEARCH UID 1:7".into(), "* SEARCH 7\r\n".into(), "OK"),
                (
                    format!("UID FETCH 7 (UID FLAGS RFC822.SIZE BODY.PEEK[HEADER]<0.{SAMPLE}>)"),
                    rows,
                    "OK",
                ),
            ])
            .await;
            let emails = list_summaries(&service, &mut session, 1, false)
                .await
                .unwrap();
            backend.await.unwrap();
            assert_eq!(emails[0]["subject"], Value::Null, "{tail:?}");
            assert_eq!(emails[0]["from"], "sender@example.invalid", "{tail:?}");
            assert_eq!(emails[0]["header_complete"], expected_complete, "{tail:?}");
            let classification = if expected_complete {
                "complete"
            } else {
                "partial"
            };
            assert_eq!(
                emails[0]["classification_status"], classification,
                "{tail:?}"
            );
        }
    }

    #[tokio::test]
    async fn cold_locate_accepts_complete_header_only_literals_within_budget() {
        const LIMIT: usize = 128 * 1024;
        const ID: &str = "<bare@example.test>";
        const HEADER: &str = "Subject: Header-only\r\nFrom: sender@example.invalid\r\nMessage-ID: <bare@example.test>\r\n";
        let boundary = padded_header(HEADER, "", LIMIT);
        let oversized = padded_header(HEADER, "", LIMIT + 1);
        let blank_line = format!("{HEADER}\r\n");
        for (header, case, expected) in [
            (HEADER, "OK", Ok(7)),
            (blank_line.as_str(), "OK", Ok(7)),
            (boundary.as_str(), "OK", Ok(7)),
            (oversized.as_str(), "OK", Err("LIMIT_EXCEEDED")),
            (HEADER, "NO", Err("MAIL_BACKEND")),
            (HEADER, "BAD", Err("MAIL_BACKEND")),
            (HEADER, "EOF", Err("MAIL_BACKEND")),
            (HEADER, "WRONG_TAG", Err("MAIL_BACKEND")),
            (HEADER, "OTHER_UID", Err("MAIL_BACKEND")),
            (HEADER, "OTHER_SECTION", Err("MAIL_BACKEND")),
            (HEADER, "OTHER_ORIGIN", Err("MAIL_BACKEND")),
        ] {
            let (_dir, service) = fetch_fixture();
            let (attributes, status) = match case {
                "OTHER_UID" => ("UID 8 BODY[HEADER]<0>", "OK"),
                "OTHER_SECTION" => ("UID 7 BODY[TEXT]<0>", "OK"),
                "OTHER_ORIGIN" => ("UID 7 BODY[HEADER]<1>", "OK"),
                status => ("UID 7 BODY[HEADER]<0>", status),
            };
            let rows = format!(
                "* 1 FETCH ({attributes} {{{}}}\r\n{header})\r\n",
                header.len()
            );
            let (mut session, backend) = fetch_script(vec![
                (
                    "EXAMINE \"INBOX\"".into(),
                    "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n".into(),
                    "OK",
                ),
                (
                    format!("UID SEARCH UID 1:7 HEADER Message-ID \"{ID}\""),
                    "* SEARCH 7\r\n".into(),
                    "OK",
                ),
                (
                    format!("UID FETCH 7 (UID BODY.PEEK[HEADER]<0.{}>)", LIMIT + 1),
                    rows,
                    status,
                ),
            ])
            .await;
            let result = service
                .insight_operation(
                    &mut session,
                    "locate_message",
                    &json!({"message_id":ID,"mailboxes":["INBOX"]}),
                )
                .await
                .map(|value| value["matches"][0]["uid"].as_u64().unwrap())
                .map_err(|error| error.code);
            backend.await.unwrap();
            assert_eq!(result, expected, "{case} with {} bytes", header.len());
        }
    }

    async fn list_summaries<S>(
        service: &MailService,
        session: &mut async_imap::Session<S>,
        limit: usize,
        search: bool,
    ) -> Result<Vec<Value>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let page = list_test_page(service, session, &json!({"limit":limit}), search).await?;
        Ok(page["emails"]
            .as_array()
            .expect("fixture summaries")
            .clone())
    }

    async fn list_test_page<S>(
        service: &MailService,
        session: &mut async_imap::Session<S>,
        args: &Value,
        search: bool,
    ) -> Result<Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let mailbox = folder(args, InputLimits::Read)?;
        let wire_mailbox = mailbox_name(mailbox, InputLimits::Read)?;
        let state = listing::select_list_mailbox(session, &wire_mailbox).await?;
        check_validity(args, state.uid_validity)?;
        let Some(scan) = listing::scan_list(session, args, search, &state).await? else {
            return Ok(
                json!({"emails":[],"next_before_uid":null,"uid_validity":state.uid_validity}),
            );
        };
        let (ids, next) = scan.unfiltered();
        let emails = service
            .fetch_summaries(session, mailbox, state.uid_validity, state.uid_next, &ids)
            .await?;
        Ok(json!({"emails":emails,"next_before_uid":next,"uid_validity":state.uid_validity}))
    }

    #[test]
    fn unsupported_data_is_rejected_before_journal_claim() {
        let dir = tempfile::tempdir().unwrap();
        let service = MailService::new(
            crate::config::fixture(),
            Arc::new(Journal::open(&dir.path().join("db")).unwrap()),
        )
        .unwrap();
        for body in [
            b"ordinary\nsecond line".to_vec(),
            b"ordinary\rsecond line".to_vec(),
            b"ordinary\0binary".to_vec(),
            vec![b'a'; 999],
        ] {
            let error = service
                .claim_send("owner", "key", "hash", "id", &body)
                .unwrap_err();
            assert_eq!(error.code, "UNSUPPORTED_DATA_FORMAT");
            assert!(service.journal.get("owner", "key").unwrap().is_none());
        }
        assert!(
            service
                .claim_send("owner", "key", "hash", "id", b"ordinary\r\nsecond line")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            service.journal.get("owner", "key").unwrap().unwrap().status,
            "sending"
        );
    }

    #[tokio::test]
    async fn rev1_mailbox_names_round_trip_through_tools() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let (client, server) = tokio::io::duplex(8192);
            let backend = tokio::spawn(async move {
                let mut server = BufReader::new(server);
                // Every mailbox argument must be the rev1 ASCII wire name.
                for (expected, rows) in [
                    ("LOGIN \"synthetic\" \"test-only\"", ""),
                    ("LIST \"\" *", "* LIST () \"/\" \"&mHl27g-&-\"\r\n"),
                    ("EXAMINE \"&mHl27g-&-\"", "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n"),
                    ("UID FETCH 7 (UID FLAGS)", "* 1 FETCH (UID 7 FLAGS ())\r\n"),
                    ("RENAME \"&mHl27g-&-\" \"&ZeVnLIqe-\"", ""),
                    ("CREATE \"&mHl27g-&-\"", ""),
                    ("DELETE \"&mHl27g-&-\"", ""),
                    ("SELECT \"&mHl27g-&-\"", "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n"),
                    ("UID FETCH 7 (UID)", "* 1 FETCH (UID 7)\r\n"),
                    ("CAPABILITY", "* CAPABILITY IMAP4rev1 MOVE\r\n"),
                    ("UID MOVE 7 \"&ZeVnLIqe-\"", ""),
                    ("SELECT \"&mHl27g-&-\"", "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n"),
                    ("UID FETCH 7 (UID)", "* 1 FETCH (UID 7)\r\n"),
                    ("CAPABILITY", "* CAPABILITY IMAP4rev1 MOVE\r\n"),
                    ("UID MOVE 7 \"&mHl27g-&-\"", ""),
                    ("EXAMINE \"&mHl27g-&-\"", "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n"),
                ] {
                    let mut line = String::new();
                    server.read_line(&mut line).await.unwrap();
                    let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
                    assert_eq!(command, expected);
                    server.write_all(format!("{rows}{tag} OK completed\r\n").as_bytes()).await.unwrap();
                }
                let mut line = String::new();
                server.read_line(&mut line).await.unwrap();
                let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
                assert_eq!(command, "UID SEARCH CHARSET UTF-8 UID 1:7 TEXT {6}");
                let tag = tag.to_owned();
                server.write_all(b"+ ready\r\n").await.unwrap();
                let mut query = [0; 8];
                server.read_exact(&mut query).await.unwrap();
                assert_eq!(&query, "发票\r\n".as_bytes());
                server.write_all(format!("* SEARCH\r\n{tag} OK searched\r\n").as_bytes()).await.unwrap();
                line.clear();
                server.read_line(&mut line).await.unwrap();
                let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
                assert!(command.starts_with("APPEND \"&mHl27g-&-\" (\\Draft) {"));
                let size = command.rsplit_once('{').unwrap().1.strip_suffix('}').unwrap().parse::<usize>().unwrap();
                let tag = tag.to_owned();
                server.write_all(b"+ ready\r\n").await.unwrap();
                let mut literal = vec![0; size + 2];
                server.read_exact(&mut literal).await.unwrap();
                assert!(literal.ends_with(b"\r\n"));
                server.write_all(format!("{tag} OK appended\r\n").as_bytes()).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let mut config = (*crate::config::fixture()).clone();
            config.drafts = "项目&".into();
            config.trash = "项目&".into();
            let service = MailService::new(Arc::new(config), Arc::new(Journal::open(&dir.path().join("db")).unwrap())).unwrap();
            let mut session = async_imap::Client::new(client).login("synthetic", "test-only").await.unwrap();
            let names = service.with_session(&mut session, "list_mailboxes", &json!({})).await.unwrap();
            let name = names["mailboxes"][0]["name"].as_str().unwrap();
            assert_eq!(name, "项目&");
            service.with_session(&mut session, "get_email_status", &json!({"mailbox":name,"uid":7})).await.unwrap();
            service.with_session(&mut session, "rename_mailbox", &json!({"name":name,"new_name":"日本語"})).await.unwrap();
            service.with_session(&mut session, "create_mailbox", &json!({"name":name})).await.unwrap();
            service.with_session(&mut session, "delete_mailbox", &json!({"name":name,"confirm":true})).await.unwrap();
            service.with_session(&mut session, "move_email", &json!({"mailbox":name,"uid":7,"destination":"日本語"})).await.unwrap();
            service.with_session(&mut session, "delete_email", &json!({"mailbox":name,"uid":7})).await.unwrap();
            let found = list_test_page(&service, &mut session, &json!({"mailbox":name,"query":"发票"}), true).await.unwrap();
            assert_eq!(found["emails"], json!([]));
            service.with_session(&mut session, "save_draft", &json!({"to":["recipient@example.com"],"subject":"synthetic","body":"synthetic"})).await.unwrap();
            backend.await.unwrap();
        }).await.unwrap();
    }
    #[test]
    fn draft_envelope_and_privacy_filter_share_canonical_fields() {
        let mut raw = concat!(
            "From: old@example.com\n",
            "Sender: old@example.com\n",
            "Message-ID: <old@example.com>\n",
            "Return-Path: <old@example.com>\n",
            "To: =?UTF-8?Q?review=40example.net=2C_release=3A_=3CLead=3E?= <visible@example.com>\n",
            "To: 张三 <unicode@example.com>\n",
            "Cc: Copy group: =?UTF-8?B?6YOo572yIDx0ZWFtPiwgKGNvcHkp?= <copy@example.com>, reviewer@example.com;\n",
            "Bcc: hidden@example.com,\n second@example.com\n",
            "bCc: third@example.com\n",
            "Subject: preserved\n",
            "Content-Type: application/octet-stream\n",
            "Content-Transfer-Encoding: quoted-printable\n\n",
        )
        .as_bytes()
        .to_vec();
        let body = b"binary=00=FF\r\n\r\n--unchanged";
        raw.extend_from_slice(body);
        let (clean, envelope) = prepare_draft(&raw, "me@example.com", "<new@example.com>").unwrap();
        assert_eq!(
            envelope
                .to()
                .iter()
                .map(AsRef::<str>::as_ref)
                .collect::<Vec<_>>(),
            [
                "visible@example.com",
                "unicode@example.com",
                "copy@example.com",
                "reviewer@example.com",
                "hidden@example.com",
                "second@example.com",
                "third@example.com",
            ]
        );
        let (headers, body_start) = mailparse::parse_headers(&clean).unwrap();
        assert!(headers.get_all_values("Bcc").is_empty());
        assert!(headers.get_all_values("Sender").is_empty());
        assert!(headers.get_all_values("Return-Path").is_empty());
        assert_eq!(headers.get_all_values("From"), ["me@example.com"]);
        assert_eq!(headers.get_all_values("Message-ID"), ["<new@example.com>"]);
        assert_eq!(&clean[body_start..], body);
        assert!(!String::from_utf8_lossy(&clean).contains("hidden@example.com"));
        assert!(!String::from_utf8_lossy(&clean).contains("second@example.com"));
        assert!(!String::from_utf8_lossy(&clean).contains("third@example.com"));

        let ambiguous = b"To: =?UTF-8?Q?address=40example.com?=\r\n\r\nbody";
        assert!(prepare_draft(ambiguous, "me@example.com", "<new@example.com>").is_err());
    }

    async fn smtp_fixture(disconnect_after_data: bool) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("db")).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let backend_journal = journal.clone();
        let backend = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            socket
                .get_mut()
                .write_all(b"220 test ESMTP\r\n")
                .await
                .unwrap();
            let mut data = false;
            let mut quit_seen = false;
            let mut payload = String::new();
            loop {
                let mut line = String::new();
                if socket.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                if data {
                    if line == ".\r\n" {
                        if disconnect_after_data {
                            break;
                        }
                        socket
                            .get_mut()
                            .write_all(b"250 accepted\r\n")
                            .await
                            .unwrap();
                        data = false;
                    } else {
                        payload.push_str(&line);
                    }
                    continue;
                }
                if line.starts_with("EHLO") || line.starts_with("HELO") {
                    socket.get_mut().write_all(b"250 test\r\n").await.unwrap();
                } else if line.starts_with("MAIL") || line.starts_with("RCPT") {
                    socket.get_mut().write_all(b"250 OK\r\n").await.unwrap();
                } else if line.starts_with("DATA") {
                    socket
                        .get_mut()
                        .write_all(b"354 send data\r\n")
                        .await
                        .unwrap();
                    data = true;
                } else if line.starts_with("QUIT") {
                    quit_seen = true;
                    assert_eq!(
                        backend_journal
                            .get("owner", "test-send")
                            .unwrap()
                            .unwrap()
                            .status,
                        "accepted"
                    );
                    // Leave QUIT unanswered until the client closes the connection.
                    assert_eq!(socket.read_line(&mut String::new()).await.unwrap(), 0);
                    break;
                } else {
                    panic!("Unexpected mock SMTP command");
                }
            }
            assert_eq!(quit_seen, !disconnect_after_data);
            assert!(payload.contains("Subject: synthetic"));
            assert!(!payload.contains("hidden@example.com"));
            listener
        });
        let mut service = MailService::new(crate::config::fixture(), journal.clone()).unwrap();
        service.smtp = Some(SmtpSender::fixture(
            port,
            std::time::Duration::from_millis(300),
        ));
        let args = json!({"to":["receiver@example.com"],"bcc":["hidden@example.com"],"subject":"synthetic","body":"test","operation_key":"test-send"});
        let result = service.send("send_email", &args, "owner").await.unwrap();
        assert_eq!(
            result["operation"]["status"],
            if disconnect_after_data {
                "unknown"
            } else {
                "accepted"
            }
        );
        assert_eq!(result["retry_allowed"], false);
        let listener = backend.await.unwrap().into_std().unwrap();
        let repeat = service.send("send_email", &args, "owner").await.unwrap();
        assert_eq!(
            repeat["operation"]["message_id"],
            result["operation"]["message_id"]
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[tokio::test]
    async fn smtp_lost_reply_does_not_resend() {
        smtp_fixture(true).await;
    }
    #[tokio::test]
    async fn smtp_accepted_is_persisted_before_unresponsive_quit() {
        smtp_fixture(false).await;
    }

    #[tokio::test]
    async fn draft_without_required_8bitmime_is_durably_rejected_before_mail() {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::TcpListener,
        };
        use tokio_rustls::{TlsAcceptor, rustls::pki_types::PrivateKeyDer};

        let imap = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let smtp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = (*crate::config::fixture()).clone();
        config.imap_host = "localhost".into();
        config.imap_port = imap.local_addr().unwrap().port();
        let smtp_port = smtp.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&dir.path().join("db")).unwrap());
        let body_fetch = format!(
            "UID FETCH 7 (UID BODY.PEEK[]<0.{}>)",
            config.max_message + 1
        );
        let mut service = MailService::new(Arc::new(config), journal.clone()).unwrap();
        service.smtp = Some(SmtpSender::fixture(smtp_port, service.config.timeout));

        let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = signed.cert.der().clone();
        let tls = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                PrivateKeyDer::Pkcs8(signed.signing_key.serialize_der().into()),
            )
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();
        service.test_imap_roots = Some(roots);

        let imap_task = tokio::spawn(async move {
            let (socket, _) = imap.accept().await.unwrap();
            let stream = TlsAcceptor::from(Arc::new(tls))
                .accept(socket)
                .await
                .unwrap();
            let mut stream = BufReader::new(stream);
            stream.write_all(b"* OK fixture\r\n").await.unwrap();
            let raw = concat!(
                "To: receiver@example.com\r\n",
                "Content-Type: text/plain; charset=utf-8\r\n",
                "Content-Transfer-Encoding: 8bit\r\n\r\n",
                "合成正文\r\n",
            );
            for (expected, reply) in [
                ("LOGIN ", String::new()),
                (
                    "EXAMINE \"Drafts\"",
                    "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n".into(),
                ),
                (
                    "UID FETCH 7 (UID RFC822.SIZE)",
                    format!("* 1 FETCH (UID 7 RFC822.SIZE {})\r\n", raw.len()),
                ),
                (
                    &body_fetch,
                    format!("* 1 FETCH (UID 7 BODY[]<0> {{{}}}\r\n{raw})\r\n", raw.len()),
                ),
            ] {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let (tag, command) = line.trim_end().split_once(' ').unwrap();
                assert!(command.starts_with(expected), "{command}");
                stream
                    .write_all(format!("{reply}{tag} OK complete\r\n").as_bytes())
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
            }
        });
        let smtp_task = tokio::spawn(async move {
            let (socket, _) = smtp.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            socket.write_all(b"220 test ESMTP\r\n").await.unwrap();
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("EHLO "));
            socket.write_all(b"250 test\r\n").await.unwrap();
            line.clear();
            socket.read_line(&mut line).await.unwrap();
            assert_eq!(line, "QUIT\r\n");
            assert_eq!(
                journal.get("owner", "test-8bit").unwrap().unwrap().status,
                "rejected"
            );
            socket.write_all(b"221 bye\r\n").await.unwrap();
            smtp
        });

        let args = json!({"uid":7,"uid_validity":9,"operation_key":"test-8bit"});
        let sent = service.execute("send_draft", &args, "owner").await.unwrap();
        assert_eq!(sent["operation"]["status"], "rejected");
        assert_eq!(sent["retry_allowed"], false);
        imap_task.await.unwrap();
        let smtp = smtp_task.await.unwrap().into_std().unwrap();
        let lookup = service
            .execute(
                "get_operation",
                &json!({"operation_key":"test-8bit"}),
                "owner",
            )
            .await
            .unwrap();
        let repeat = service.execute("send_draft", &args, "owner").await.unwrap();
        assert_eq!(lookup["operation"], sent["operation"]);
        assert_eq!(repeat["operation"], sent["operation"]);
        assert_eq!(
            smtp.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn bounded_parser_rejects_incomplete_response_at_application_limit() {
        use tokio::io::AsyncWriteExt;
        let (input, mut output) = tokio::io::duplex(128);
        output
            .write_all(b"* 1 FETCH (BODY[] {2048}\r\n")
            .await
            .unwrap();
        let (stream, _) = BudgetStream::new(input, 1024);
        let mut client = async_imap::Client::with_buffer_limit(stream, 1024);
        // Keep the peer open: rejection must precede another network read.
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), client.read_response()).await;
        assert!(result.unwrap().is_err());
        assert!(client.read_response().await.unwrap().is_none());
    }
}
