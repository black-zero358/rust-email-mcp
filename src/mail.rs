use crate::{
    config::Config,
    error::{MailError, Result},
    imap_text::{display_name, mailbox_name, uid_field_search},
    journal::{Journal, Operation},
    mime,
    smtp::SmtpSender,
};
pub mod attachments;
pub mod insights;
use futures::TryStreamExt;
use lettre::{address::Envelope, message::Mailbox};
use mailparse::MailHeaderMap;
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
    sync::Semaphore,
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
        (
            Self {
                inner,
                remaining: remaining.clone(),
            },
            remaining,
        )
    }
}
impl<T: AsyncRead + Unpin> AsyncRead for BudgetStream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let limit = self.remaining.load(Ordering::Relaxed).min(buf.remaining());
        if limit == 0 {
            return Poll::Ready(Err(std::io::Error::other("IMAP response byte limit")));
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

pub struct MailService {
    pub config: Arc<Config>,
    pub journal: Arc<Journal>,
    slots: Semaphore,
    smtp: SmtpSender,
    pub attachments: attachments::AttachmentStore,
    metadata_cache: std::sync::Mutex<insights::MetadataCache>,
}
impl MailService {
    pub fn new(config: Arc<Config>, journal: Arc<Journal>) -> anyhow::Result<Self> {
        let smtp = SmtpSender::new(&config)?;
        Ok(Self {
            attachments: attachments::AttachmentStore::new(config.clone()),
            metadata_cache: Default::default(),
            config,
            journal,
            slots: Semaphore::new(2),
            smtp,
        })
    }
    async fn connect(&self) -> Result<Session> {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = TcpStream::connect((&*self.config.imap_host, self.config.imap_port))
            .await
            .map_err(|_| MailError::backend())?;
        let name =
            ServerName::try_from(self.config.imap_host.clone()).map_err(|_| MailError::input())?;
        let stream = TlsConnector::from(Arc::new(tls))
            .connect(name, tcp)
            .await
            .map_err(|_| MailError::backend())?;
        let budget = self.config.max_message + 2 * 1024 * 1024;
        let (stream, _) = BudgetStream::new(stream, budget);
        let mut client = async_imap::Client::with_buffer_limit(stream, budget);
        client
            .read_response()
            .await
            .map_err(|_| MailError::backend())?;
        client
            .login(&self.config.address, &self.config.password)
            .await
            .map_err(|_| MailError::backend())
    }
    pub async fn execute(&self, name: &str, args: &Value, principal: &str) -> Result<Value> {
        if name == "list_accounts" {
            return Ok(
                json!({"accounts":[{"id":"default","address":self.config.address,"mode":self.config.mode}]}),
            );
        }
        if name == "get_operation" {
            return Ok(
                json!({"operation":self.journal.get(principal,text(args,"operation_key")?)?}),
            );
        }
        let _permit = tokio::time::timeout(std::time::Duration::from_secs(2), self.slots.acquire())
            .await
            .map_err(|_| MailError {
                code: "BUSY",
                message: "Mailbox concurrency limit reached.",
                retryable: true,
            })?
            .map_err(|_| MailError::backend())?;
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
            .map_err(|_| MailError::timeout())?;
        }
        if name == "diagnose_connectivity" {
            return self.diagnose_connectivity().await;
        }
        if ["mark_emails", "move_emails", "delete_emails"].contains(&name) {
            let ids = batch_ids(args)?;
            folder(args)?;
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
        tokio::time::timeout(self.config.timeout, self.imap_operation(name, args))
            .await
            .map_err(|_| MailError::timeout())?
    }
    async fn imap_operation(&self, name: &str, a: &Value) -> Result<Value> {
        let mut s = self.connect().await?;
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
        match name {
            "read_conversation" | "locate_message" | "get_mailbox_statistics" => {
                self.insight_operation(s, name, a).await
            }
            "list_mailboxes" => {
                let list = s
                    .list(Some(""), Some("*"))
                    .await
                    .map_err(|_| MailError::backend())?
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(|_| MailError::backend())?;
                if list.len() > 200 {
                    return Err(MailError::limit());
                }
                let mailboxes = list.iter().map(|m| Ok(json!({"name":display_name(m.name())?,"attributes":format!("{:?}",m.attributes())}))).collect::<Result<Vec<_>>>()?;
                Ok(json!({"mailboxes":mailboxes}))
            }
            "list_emails" | "search_emails" => self.list(s, a, name == "search_emails").await,
            "get_email" | "get_email_status" | "download_attachment" => {
                let folder = folder(a)?;
                let uid = uid(a)?;
                let state = s
                    .examine(mailbox_name(folder)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                if name == "get_email_status" {
                    return email_status(s, uid, state.uid_validity).await;
                }
                let raw = self.raw(s, uid).await?;
                if name == "download_attachment" {
                    return attachment(
                        &raw,
                        number(a, "attachment_index", 0, 0, 1000)? as usize,
                        self.config.max_output / 2,
                    );
                }
                insights::parsed_with_options(
                    &raw,
                    uid,
                    state.uid_validity,
                    self.config.max_output / 2,
                    a,
                )
            }
            "get_emails" => {
                let ids = a
                    .get("uids")
                    .and_then(Value::as_array)
                    .ok_or_else(MailError::input)?;
                if ids.is_empty() || ids.len() > 10 {
                    return Err(MailError::input());
                }
                let state = s
                    .examine(mailbox_name(folder(a)?)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                let mut items = Vec::new();
                let mut stopped = false;
                let mut attempted = 0;
                for id in ids {
                    let id = id
                        .as_u64()
                        .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                        .ok_or_else(MailError::input)? as u32;
                    if stopped {
                        items.push(json!({"uid":id,"ok":false,"status":"unprocessed"}));
                        continue;
                    }
                    attempted += 1;
                    match self.raw(s, id).await.and_then(|raw| {
                        insights::parsed_with_options(&raw, id, state.uid_validity, 16000, a)
                    }) {
                        Ok(value) => items.push(json!({"uid":id,"ok":true,"email":value})),
                        Err(error) => {
                            items.push(json!({"uid":id,"ok":false,"error":error}));
                            stopped = true;
                        }
                    }
                }
                Ok(json!({"items":items,"requested":ids.len(),"completed":attempted}))
            }
            "save_draft" => save_draft(s, &self.config, a).await,
            "create_mailbox" => {
                s.create(mailbox_name(text(a, "name")?)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                Ok(json!({"status":"created"}))
            }
            "rename_mailbox" => {
                s.rename(
                    mailbox_name(text(a, "name")?)?,
                    mailbox_name(text(a, "new_name")?)?,
                )
                .await
                .map_err(|_| MailError::backend())?;
                Ok(json!({"status":"renamed"}))
            }
            "delete_mailbox" => {
                if a["confirm"] != true {
                    return Err(MailError::input());
                }
                s.delete(mailbox_name(text(a, "name")?)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                Ok(json!({"status":"deleted"}))
            }
            "move_email" | "delete_email" | "mark_email" => {
                let state = s
                    .select(mailbox_name(folder(a)?)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                let uid = uid(a)?.to_string();
                match name {
                    "mark_email" => {
                        let flag = match text(a, "flag")? {
                            "seen" => "\\Seen",
                            "flagged" => "\\Flagged",
                            "answered" => "\\Answered",
                            _ => return Err(MailError::input()),
                        };
                        let op = if a["value"].as_bool().ok_or_else(MailError::input)? {
                            "+FLAGS.SILENT"
                        } else {
                            "-FLAGS.SILENT"
                        };
                        s.uid_store(uid, format!("{op} ({flag})"))
                            .await
                            .map_err(|_| MailError::backend())?
                            .try_collect::<Vec<_>>()
                            .await
                            .map_err(|_| MailError::backend())?;
                    }
                    "delete_email" if a["permanent"] == true => {
                        if a["confirm"] != true {
                            return Err(MailError::input());
                        }
                        let caps = s.capabilities().await.map_err(|_| MailError::backend())?;
                        if !caps.has_str("UIDPLUS") {
                            return Err(MailError {
                                code: "UNSUPPORTED",
                                message: "Permanent deletion requires UIDPLUS.",
                                retryable: false,
                            });
                        }
                        s.uid_store(&uid, "+FLAGS.SILENT (\\Deleted)")
                            .await
                            .map_err(|_| MailError::backend())?
                            .try_collect::<Vec<_>>()
                            .await
                            .map_err(|_| MailError::backend())?;
                        s.uid_expunge(&uid)
                            .await
                            .map_err(|_| MailError::backend())?
                            .try_collect::<Vec<_>>()
                            .await
                            .map_err(|_| MailError::backend())?;
                    }
                    _ => {
                        let dest = if name == "delete_email" {
                            &self.config.trash
                        } else {
                            text(a, "destination")?
                        };
                        let caps = s.capabilities().await.map_err(|_| MailError::backend())?;
                        if !caps.has_str("MOVE") {
                            return Err(MailError {
                                code: "UNSUPPORTED",
                                message: "Safe movement requires the server MOVE capability.",
                                retryable: false,
                            });
                        }
                        s.uid_mv(&uid, mailbox_name(dest)?)
                            .await
                            .map_err(|_| MailError::backend())?;
                    }
                }
                Ok(json!({"status":"updated"}))
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
        };
        Ok(run_batch(&mut executor, &ids, deadline).await)
    }

    async fn raw<S>(&self, s: &mut async_imap::Session<S>, uid: u32) -> Result<Vec<u8>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let metadata = s
            .uid_fetch(uid.to_string(), "(UID RFC822.SIZE)")
            .await
            .map_err(|_| MailError::backend())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|_| MailError::backend())?;
        let size = metadata
            .iter()
            .find(|f| f.uid == Some(uid))
            .and_then(|f| f.size)
            .ok_or_else(MailError::backend)? as usize;
        if size > self.config.max_message {
            return Err(MailError::limit());
        }
        let f = s
            .uid_fetch(
                uid.to_string(),
                format!("(UID BODY.PEEK[]<0.{}>)", self.config.max_message + 1),
            )
            .await
            .map_err(|_| MailError::backend())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|_| MailError::backend())?;
        let body = f
            .iter()
            .find(|f| f.uid == Some(uid))
            .and_then(|f| f.body())
            .ok_or_else(MailError::backend)?;
        if body.len() > self.config.max_message {
            return Err(MailError::limit());
        }
        Ok(body.to_vec())
    }
    async fn list<S>(
        &self,
        s: &mut async_imap::Session<S>,
        a: &Value,
        search: bool,
    ) -> Result<Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let m = s
            .examine(mailbox_name(folder(a)?)?)
            .await
            .map_err(|_| MailError::backend())?;
        check_validity(a, m.uid_validity)?;
        let limit = number(a, "limit", 20, 1, 50)? as usize;
        let before = number(
            a,
            "before_uid",
            m.uid_next.unwrap_or(u32::MAX) as u64,
            1,
            u32::MAX as u64,
        )? as u32;
        if m.exists == 0 || before <= 1 {
            return Ok(json!({"emails":[],"next_before_uid":null,"uid_validity":m.uid_validity}));
        }
        // Scan a finite UID window; callers explicitly advance the cursor even for sparse results.
        let end = before - 1;
        let start = end.saturating_sub(499).max(1);
        let mut filters = a.clone();
        for field in ["query", "from", "to", "subject"] {
            filters
                .as_object_mut()
                .ok_or_else(MailError::input)?
                .remove(field);
        }
        let query = search_query(&filters, start, end, search)?;
        let has_text = search
            && ["query", "from", "to", "subject"]
                .iter()
                .any(|key| a.get(key).is_some());
        let mut found = if has_text && query == format!("UID {start}:{end}") {
            None
        } else {
            Some(
                s.uid_search(query)
                    .await
                    .map_err(|_| MailError::backend())?,
            )
        };
        if search {
            for (field, key) in [
                ("query", "TEXT"),
                ("from", "FROM"),
                ("to", "TO"),
                ("subject", "SUBJECT"),
            ] {
                if a.get(field).is_some() {
                    let matching = uid_field_search(s, start, end, key, text(a, field)?).await?;
                    if let Some(ids) = &mut found {
                        ids.retain(|uid| matching.contains(uid));
                    } else {
                        found = Some(matching);
                    }
                }
            }
        }
        let found = found.unwrap_or_default();
        let mut ids = found
            .into_iter()
            .filter(|u| *u >= start && *u <= end)
            .collect::<Vec<_>>();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let mut last_examined = None;
        if let Some(wanted) = a.get("has_attachment") {
            let wanted = wanted.as_bool().ok_or_else(MailError::input)?;
            let mut matches = Vec::new();
            for id in &ids {
                last_examined = Some(*id);
                let raw = self.raw(s, *id).await?;
                if attachments::has_attachment(&raw)? == wanted {
                    matches.push(*id);
                }
                if matches.len() == limit {
                    break;
                }
            }
            // Retain the last examined UID, not the last server match: rejected
            // BODYSTRUCTURE candidates must not be revisited or skipped incorrectly.
            ids = matches;
        }
        let more = ids.len() > limit;
        ids.truncate(limit);
        let next = page_cursor(
            start,
            &ids,
            limit,
            more,
            a.get("has_attachment").is_some(),
            last_examined,
        );
        if ids.is_empty() {
            return Ok(json!({"emails":[],"next_before_uid":next,"uid_validity":m.uid_validity}));
        }
        let set = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let fetched = s
            .uid_fetch(set, "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER]<0.8192>)")
            .await
            .map_err(|_| MailError::backend())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|_| MailError::backend())?;
        let mut out = Vec::new();
        for f in fetched {
            if !f.uid.is_some_and(|id| ids.contains(&id)) {
                continue;
            }
            if let Some(id) = f.uid
                && f.header().is_some_and(|header| header.len() < 8192)
            {
                self.remember_headers(
                    folder(a)?,
                    m.uid_validity,
                    id,
                    f.header().unwrap_or_default(),
                );
            }
            let headers = mime::headers(f.header().unwrap_or_default())?;
            out.push(json!({"uid":f.uid,"subject":headers.get_first_value("Subject"),"from":headers.get_first_value("From"),"date":headers.get_first_value("Date"),"size":f.size,"flags":f.flags().map(|f|format!("{f:?}")).collect::<Vec<_>>()}));
        }
        out.sort_by_key(|v| std::cmp::Reverse(v["uid"].as_u64()));
        Ok(
            json!({"emails":out,"uid_validity":m.uid_validity,"next_before_uid":next,"scanned_uid_range":[start,end]}),
        )
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
        let message_id = format!("<{}@email-mcp.local>", uuid::Uuid::new_v4());
        let (raw, envelope) = if name == "send_draft" {
            let raw = tokio::time::timeout(self.config.timeout, async {
                let mut s = self.connect().await?;
                let state = s
                    .examine(mailbox_name(&self.config.drafts)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                self.raw(&mut s, uid(a)?).await
            })
            .await
            .map_err(|_| MailError::timeout())??;
            prepare_draft(&raw, &self.config.address, &message_id)?
        } else if name == "reply_email" || name == "forward_email" {
            let original = tokio::time::timeout(self.config.timeout, async {
                let mut s = self.connect().await?;
                let state = s
                    .examine(mailbox_name(folder(a)?)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                self.raw(&mut s, uid(a)?).await
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
        if raw.len() > self.config.max_message {
            return Err(MailError::limit());
        }
        if let Some(old) = self
            .journal
            .claim(principal, key, &fingerprint, &message_id)?
        {
            return Ok(json!({"operation":old}));
        }
        let submission = self.smtp.submit(&envelope, &raw).await;
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
}

trait BatchExecutor {
    fn apply(&mut self, uid: u32) -> impl std::future::Future<Output = Result<Value>> + Send;
}
struct SessionBatch<'a> {
    service: &'a MailService,
    session: &'a mut Session,
    name: &'a str,
    args: &'a Value,
}
impl BatchExecutor for SessionBatch<'_> {
    async fn apply(&mut self, id: u32) -> Result<Value> {
        let mut args = self.args.clone();
        args["uid"] = json!(id);
        args.as_object_mut().unwrap().remove("uids");
        let state = self
            .session
            .select(mailbox_name(folder(&args)?)?)
            .await
            .map_err(|_| MailError::backend())?;
        check_validity(&args, state.uid_validity)?;
        let found = self
            .session
            .uid_fetch(id.to_string(), "(UID)")
            .await
            .map_err(|_| MailError::backend())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|_| MailError::backend())?;
        if !found.iter().any(|f| f.uid == Some(id)) {
            return Ok(json!({"uid":id,"status":"not_found"}));
        }
        self.service
            .with_session(self.session, self.name, &args)
            .await?;
        Ok(json!({"uid":id,"status":"updated"}))
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
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
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
fn folder(a: &Value) -> Result<&str> {
    let f = a.get("mailbox").and_then(Value::as_str).unwrap_or("INBOX");
    if f.len() > 512 || f.contains(['\r', '\n', '\0']) {
        return Err(MailError::input());
    }
    Ok(f)
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
async fn email_status<S>(
    s: &mut async_imap::Session<S>,
    uid: u32,
    validity: Option<u32>,
) -> Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let fetched = s
        .uid_fetch(uid.to_string(), "(UID FLAGS)")
        .await
        .map_err(|_| MailError::backend())?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|_| MailError::backend())?;
    // FETCH may also contain unsolicited updates for other messages. A target
    // update replaces its previous flags, so use the last matching response.
    let target = fetched
        .iter()
        .rev()
        .find(|f| f.uid == Some(uid))
        .ok_or(MailError {
            code: "NOT_FOUND",
            message: "The requested email UID was not returned by the mailbox.",
            retryable: false,
        })?;
    Ok(
        json!({"uid":uid,"uid_validity":validity,"flags":target.flags().map(|f|format!("{f:?}")).collect::<Vec<_>>()}),
    )
}

async fn save_draft<S>(s: &mut async_imap::Session<S>, config: &Config, a: &Value) -> Result<Value>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
{
    let (raw, _) = compose(a, &config.address, None)?;
    if raw.len() > config.max_message {
        return Err(MailError::limit());
    }
    // async-imap serializes flags verbatim; RFC 3501 APPEND requires a flag list.
    s.append(mailbox_name(&config.drafts)?, Some("(\\Draft)"), None, &raw)
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
        for val in headers.get_all_values(key) {
            let list = mailparse::addrparse(&val).map_err(|_| MailError::input())?;
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
    Ok((clean, envelope))
}
fn forward_body(raw: &[u8], note: &str) -> Result<String> {
    Ok(format!(
        "{note}\n\n---------- Forwarded message ----------\n{}",
        mime::parse(raw)?.plain_body()?
    ))
}

fn leaves<'a, 'b>(
    p: &'a mime::Part<'b>,
    out: &mut Vec<&'a mime::Part<'b>>,
    depth: usize,
) -> Result<()> {
    if depth > 20 || out.len() > 1000 {
        return Err(MailError::limit());
    }
    if p.subparts.is_empty() || p.is_attachment() {
        out.push(p);
    } else {
        for part in &p.subparts {
            leaves(part, out, depth + 1)?;
        }
    }
    Ok(())
}
fn parsed(raw: &[u8], uid: u32, validity: Option<u32>, max: usize) -> Result<Value> {
    let p = mime::parse(raw)?;
    let mut body = p.plain_body()?;
    let truncated = body.len() > max;
    if truncated {
        let mut cut = max;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    let mut parts = Vec::new();
    leaves(&p, &mut parts, 0)?;
    let attachments=parts.iter().enumerate().filter(|(_,p)|p.is_attachment()).map(|(i,p)|json!({"attachment_index":i,"content_type":p.ctype.mimetype,"filename":p.filename()})).collect::<Vec<_>>();
    Ok(
        json!({"uid":uid,"uid_validity":validity,"subject":p.headers.get_first_value("Subject"),"from":p.headers.get_first_value("From"),"to":p.headers.get_first_value("To"),"body":body,"body_truncated":truncated,"attachments":attachments,"content_trust":"untrusted"}),
    )
}
fn attachment(raw: &[u8], index: usize, max: usize) -> Result<Value> {
    use base64::Engine;
    let p = mime::parse(raw)?;
    let mut parts = Vec::new();
    leaves(&p, &mut parts, 0)?;
    let part = parts.get(index).ok_or_else(MailError::input)?;
    let bytes = part.body_raw(max)?;
    Ok(
        json!({"content_type":part.ctype.mimetype,"size":bytes.len(),"base64":base64::engine::general_purpose::STANDARD.encode(bytes)}),
    )
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn advanced_search_intersects_filters_and_sync_literal_fields() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let (client, server) = tokio::io::duplex(4096);
            let backend = tokio::spawn(async move {
                let mut server = BufReader::new(server);
                for (expected, rows) in [
                    ("LOGIN \"synthetic\" \"test-only\"", ""),
                    ("EXAMINE \"&mHl27g-&-\"", "* 2 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n"),
                    ("UID SEARCH UID 1:7 SINCE 29-Feb-2024 UNSEEN", "* SEARCH 6 7\r\n"),
                    ("UID SEARCH UID 1:7 FROM \"sender@example.test\"", "* SEARCH 7\r\n"),
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
                assert_eq!(command, "UID SEARCH CHARSET UTF-8 UID 1:7 SUBJECT {6}");
                let tag = tag.to_owned();
                server.write_all(b"+ ready\r\n").await.unwrap();
                let mut query = [0; 8];
                server.read_exact(&mut query).await.unwrap();
                assert_eq!(&query, "发票\r\n".as_bytes());
                server.write_all(format!("* SEARCH 6\r\n{tag} OK completed\r\n").as_bytes()).await.unwrap();
            });
            let service = MailService::new(crate::config::fixture(), Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap())).unwrap();
            let mut session = async_imap::Client::new(client).login("synthetic", "test-only").await.unwrap();
            let found = service.with_session(&mut session, "search_emails", &json!({"mailbox":"项目&","uid_validity":9,"since":"2024-02-29","seen":false,"from":"sender@example.test","subject":"发票"})).await.unwrap();
            assert_eq!(found["emails"], json!([]));
            backend.await.unwrap();
        }).await.unwrap();
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
                    ("CAPABILITY", "* CAPABILITY IMAP4rev1 MOVE\r\n"),
                    ("UID MOVE 7 \"&ZeVnLIqe-\"", ""),
                    ("SELECT \"&mHl27g-&-\"", "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n"),
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
            let found = service.with_session(&mut session, "search_emails", &json!({"mailbox":name,"query":"发票"})).await.unwrap();
            assert_eq!(found["emails"], json!([]));
            service.with_session(&mut session, "save_draft", &json!({"to":["recipient@example.com"],"subject":"synthetic","body":"synthetic"})).await.unwrap();
            backend.await.unwrap();
        }).await.unwrap();
    }
    #[tokio::test]
    async fn status_uses_only_target_uid_and_distinguishes_missing_from_empty_flags() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        for (rows, expected) in [
            (
                "* 2 FETCH (UID 99 FLAGS (\\Seen))\r\n* 1 FETCH (UID 7 FLAGS (\\Flagged))\r\n* 3 FETCH (UID 100 FLAGS (\\Answered))\r\n",
                Some(json!(["Flagged"])),
            ),
            (
                "* 2 FETCH (FLAGS (\\Seen))\r\n* 1 FETCH (UID 7 FLAGS (\\Flagged))\r\n* 3 FETCH (FLAGS (\\Answered))\r\n",
                Some(json!(["Flagged"])),
            ),
            (
                "* 2 FETCH (UID 99 FLAGS (\\Seen))\r\n* 3 FETCH (FLAGS (\\Answered))\r\n",
                None,
            ),
            ("", None),
            (
                "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n* 1 FETCH (UID 7 FLAGS (\\Flagged))\r\n",
                Some(json!(["Flagged"])),
            ),
            (
                "* 2 FETCH (UID 99 FLAGS (\\Seen))\r\n* 1 FETCH (UID 7 FLAGS ())\r\n",
                Some(json!([])),
            ),
        ] {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let (client, server) = tokio::io::duplex(8192);
                let backend = tokio::spawn(async move {
                    let mut server = BufReader::new(server);
                    let mut line = String::new();
                    server.read_line(&mut line).await.unwrap();
                    let tag = line.split_whitespace().next().unwrap();
                    server
                        .write_all(format!("{tag} OK login\r\n").as_bytes())
                        .await
                        .unwrap();
                    line.clear();
                    server.read_line(&mut line).await.unwrap();
                    let (tag, command) = line.split_once(' ').unwrap();
                    assert_eq!(command, "UID FETCH 7 (UID FLAGS)\r\n");
                    server
                        .write_all(format!("{rows}{tag} OK fetched\r\n").as_bytes())
                        .await
                        .unwrap();
                });
                let mut session = async_imap::Client::new(client)
                    .login("synthetic", "test-only")
                    .await
                    .unwrap();
                let result = email_status(&mut session, 7, Some(9)).await;
                backend.await.unwrap();
                match expected {
                    Some(flags) => assert_eq!(
                        result.unwrap(),
                        json!({"uid":7,"uid_validity":9,"flags":flags})
                    ),
                    None => assert_eq!(result.unwrap_err().code, "NOT_FOUND"),
                }
            })
            .await
            .unwrap();
        }
    }
    #[tokio::test]
    async fn refused_imap_tools_return_backend_error_and_stop_delete() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let service = MailService::new(
            crate::config::fixture(),
            Arc::new(Journal::open(&dir.path().join("db")).unwrap()),
        )
        .unwrap();
        for (tool, target, row) in [
            (
                "mark_email",
                "UID STORE",
                "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n",
            ),
            (
                "delete_email",
                "UID STORE",
                "* 1 FETCH (UID 7 FLAGS (\\Deleted))\r\n",
            ),
            ("delete_email", "UID EXPUNGE", "* 1 EXPUNGE\r\n"),
            ("list_mailboxes", "LIST", "* LIST () \"/\" \"INBOX\"\r\n"),
            ("search_emails", "UID SEARCH", "* SEARCH 7\r\n"),
            (
                "get_email_status",
                "UID FETCH",
                "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n",
            ),
        ] {
            for status in ["NO", "BAD", "EOF"] {
                for partial in [false, true] {
                    tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        let (client, server) = tokio::io::duplex(8192);
                        let backend = tokio::spawn(async move {
                            let mut server = BufReader::new(server);
                            let mut rejected = false;
                            loop {
                                let mut line = String::new();
                                if server.read_line(&mut line).await.unwrap() == 0 {
                                    assert!(rejected);
                                    break;
                                }
                                assert!(!rejected, "command after rejection: {line}");
                                let (tag, command) = line.split_once(' ').unwrap();
                                let mut response = String::new();
                                if command.starts_with(target) {
                                    rejected = true;
                                    if partial { response.push_str(row); }
                                    if status != "EOF" {
                                        response.push_str(&format!("{tag} {status} refused\r\n"));
                                    }
                                } else if command.starts_with("SELECT") || command.starts_with("EXAMINE") {
                                    response.push_str("* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n");
                                } else if command.starts_with("CAPABILITY") {
                                    response.push_str("* CAPABILITY IMAP4rev1 UIDPLUS\r\n");
                                } else {
                                    assert!(command.starts_with("LOGIN") || (target == "UID EXPUNGE" && command.starts_with("UID STORE")), "unexpected: {command}");
                                }
                                if !rejected { response.push_str(&format!("{tag} OK completed\r\n")); }
                                server.write_all(response.as_bytes()).await.unwrap();
                                if rejected && status == "EOF" { break; }
                            }
                        });
                        let mut session = async_imap::Client::new(client).login("synthetic", "test-only").await.unwrap();
                        let result = service.with_session(&mut session, tool, &json!({"folder":"INBOX","uid":7,"uid_validity":9,"flag":"seen","value":true,"permanent":true,"confirm":true,"query":"synthetic"})).await;
                        assert_eq!(result.unwrap_err().code, "MAIL_BACKEND", "{tool}/{target}/{status}/{partial}");
                        drop(session);
                        backend.await.unwrap();
                    }).await.unwrap();
                }
            }
        }
    }
    #[tokio::test]
    async fn save_draft_append_protocol() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        // Exercise the same operation as the tool, using only synthetic mailbox data.
        for (status, continuation) in [
            ("OK", true),
            ("NO", true),
            ("BAD", true),
            ("NO", false),
            ("BAD", false),
        ] {
            let (client, server) = tokio::io::duplex(8192);
            let server = tokio::spawn(async move {
                let mut server = BufReader::new(server);
                let mut line = String::new();
                server.read_line(&mut line).await.unwrap();
                assert!(line.ends_with(" LOGIN \"synthetic\" \"test-only\"\r\n"));
                let tag = line.split_whitespace().next().unwrap().to_owned();
                server
                    .write_all(format!("{tag} OK logged in\r\n").as_bytes())
                    .await
                    .unwrap();
                line.clear();
                server.read_line(&mut line).await.unwrap();
                let (tag, command) = line.split_once(' ').unwrap();
                let prefix = "APPEND \"Drafts\" (\\Draft) {";
                assert!(command.starts_with(prefix), "invalid APPEND: {command:?}");
                let size: usize = command
                    .strip_prefix(prefix)
                    .unwrap()
                    .strip_suffix("}\r\n")
                    .unwrap()
                    .parse()
                    .unwrap();
                let tag = tag.to_owned();
                if continuation {
                    server.write_all(b"+ ready for literal\r\n").await.unwrap();
                    let mut literal = vec![0; size];
                    server.read_exact(&mut literal).await.unwrap();
                    let parsed = mailparse::parse_mail(&literal).unwrap();
                    assert_eq!(
                        parsed.headers.get_first_value("Subject").as_deref(),
                        Some("synthetic draft")
                    );
                    assert_eq!(
                        parsed.headers.get_first_value("Bcc").as_deref(),
                        Some("hidden@example.com")
                    );
                    assert_eq!(parsed.get_body().unwrap().trim(), "synthetic body");
                    let mut end = [0; 2];
                    server.read_exact(&mut end).await.unwrap();
                    assert_eq!(&end, b"\r\n");
                }
                server
                    .write_all(format!("{tag} {status} append result\r\n").as_bytes())
                    .await
                    .unwrap();
            });
            let client = async_imap::Client::new(client);
            let mut session = client.login("synthetic", "test-only").await.unwrap();
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), save_draft(
                &mut session, &crate::config::fixture(),
                &json!({"to":["recipient@example.com"],"bcc":["hidden@example.com"],"subject":"synthetic draft","body":"synthetic body"}),
            )).await.unwrap();
            server.await.unwrap();
            if status == "OK" {
                assert_eq!(
                    result.unwrap(),
                    json!({"status":"saved","mailbox":"Drafts"})
                );
            } else {
                assert_eq!(result.unwrap_err().code, "MAIL_BACKEND");
            }
        }
    }
    use super::*;
    #[test]
    fn draft_header_names_are_canonical_before_interpretation() {
        let (canonical, body_start) =
            canonical_draft_headers(b"X-Example \t: value\n\tfolded\n\nbody").unwrap();
        assert_eq!(canonical, b"x-example: value\r\n\tfolded\r\n\r\n");
        assert_eq!(
            &b"X-Example \t: value\n\tfolded\n\nbody"[body_start..],
            b"body"
        );
        assert_eq!(
            mime::headers(&canonical)
                .unwrap()
                .get_first_value("X-Example")
                .unwrap(),
            "value folded"
        );
    }

    #[test]
    fn draft_envelope_and_privacy_filter_share_canonical_fields() {
        let mut raw = b"From: old@example.com\nSender: old@example.com\nMessage-ID: <old@example.com>\nReturn-Path: <old@example.com>\nTo: visible@example.com\nCc: copy@example.com\nBcc: hidden@example.com,\n second@example.com\nbCc: third@example.com\nSubject: preserved\nContent-Type: application/octet-stream\n\n".to_vec();
        let body = b"binary\0\xff\n\r\n\r\n--unchanged";
        raw.extend_from_slice(body);
        let (clean, envelope) = prepare_draft(&raw, "me@example.com", "<new@example.com>").unwrap();
        assert_eq!(envelope.to().len(), 5);
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
    }

    #[test]
    fn ambiguous_draft_headers_are_rejected() {
        for raw in [
            &b"To: a@example.com\nNo-Colon\n\nbody"[..],
            &b"To: a@example.com\nBad Name: value\n\nbody"[..],
            &b"To: a@example.com\nX: bad\0value\n\nbody"[..],
            &b"\tOrphan: value\nTo: a@example.com\n\nbody"[..],
            &b"To: a@example.com\n"[..],
        ] {
            assert!(prepare_draft(raw, "me@example.com", "<id@example.com>").is_err());
        }
        let error = prepare_draft(
            b"To: a@example.com\nResent-To: b@example.com\n\nbody",
            "me@example.com",
            "<id@example.com>",
        )
        .unwrap_err();
        assert_eq!(error.code, "UNSUPPORTED_DRAFT_HEADERS");
        assert!(!error.retryable);
    }
    #[test]
    fn text_attachments_stay_downloadable_but_leave_body_and_forward() {
        use base64::Engine;
        for headers in [
            "Content-Type: text/plain\r\nContent-Disposition: attachment; filename=notes.txt",
            "Content-Type: text/plain; name=notes.txt",
            "Content-Type: text/plain\r\nContent-Disposition: inline; filename=notes.txt",
            "Content-Type: text/plain\r\nContent-Disposition: inline; filename*=x-unknown''notes.txt",
            "Content-Type: text/plain; name*=x-unknown''notes.txt",
            "Content-Type: text/plain\r\nContent-Disposition: inline; filename*1*=utf-8''notes.txt",
            "Content-Type: text/plain; name*1=notes.txt",
            "Content-Type: multipart/mixed; boundary=inside\r\nContent-Disposition: attachment; filename=bundle.mime",
        ] {
            let attached = if headers.starts_with("Content-Type: multipart/") {
                "--inside\r\nContent-Type: text/plain\r\n\r\nATTACHMENT_ONLY_MARKER\r\n--inside--"
            } else {
                "ATTACHMENT_ONLY_MARKER"
            };
            let raw = format!(
                "Content-Type: multipart/mixed; boundary=outside\r\n\r\n--outside\r\nContent-Type: text/plain\r\n\r\nVisible body\r\n--outside\r\n{headers}\r\n\r\n{attached}\r\n--outside--\r\n"
            );
            let read = parsed(raw.as_bytes(), 1, None, 16000).unwrap();
            assert_eq!(read["body"], "Visible body");
            assert_eq!(read["attachments"].as_array().unwrap().len(), 1);
            let index = read["attachments"][0]["attachment_index"].as_u64().unwrap() as usize;
            let downloaded = attachment(raw.as_bytes(), index, 1024).unwrap();
            let data = base64::engine::general_purpose::STANDARD
                .decode(downloaded["base64"].as_str().unwrap())
                .unwrap();
            assert!(String::from_utf8_lossy(&data).contains("ATTACHMENT_ONLY_MARKER"));
            let body = forward_body(raw.as_bytes(), "FYI").unwrap();
            let (forward, _) = compose(
                &json!({"to":["recipient@example.com"],"subject":"forward","body":body}),
                "me@example.com",
                Some("<id@example.com>"),
            )
            .unwrap();
            let forwarded = mailparse::parse_mail(&forward).unwrap().get_body().unwrap();
            assert!(forwarded.contains("Visible body"));
            assert!(!forwarded.contains("ATTACHMENT_ONLY_MARKER"));
        }
    }
    #[test]
    fn html_read_and_forward_preserve_original_content() {
        use base64::Engine;
        for (headers, encoded, expected) in [
            (
                "Content-Type: text/html; charset=utf-8",
                "<html><body>Invoice 123 due Friday</body></html>".to_owned(),
                "Invoice 123 due Friday",
            ),
            (
                "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64",
                base64::engine::general_purpose::STANDARD.encode("<p>中 &amp; Friday</p>"),
                "中 & Friday",
            ),
            (
                "Content-Type: text/html; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable",
                "<p>caf=E9 invoice</p>".to_owned(),
                "café invoice",
            ),
        ] {
            let raw = format!("Subject: HTML original\r\n{headers}\r\n\r\n{encoded}");
            let read = parsed(raw.as_bytes(), 1, None, 16000).unwrap();
            assert_eq!(read["body"], expected);
            assert_eq!(read["body_truncated"], false);
            let body = forward_body(raw.as_bytes(), "FYI").unwrap();
            let (forward, _) = compose(
                &json!({"to":["recipient@example.com"],"subject":"forward","body":body}),
                "me@example.com",
                Some("<id@example.com>"),
            )
            .unwrap();
            assert!(
                mailparse::parse_mail(&forward)
                    .unwrap()
                    .get_body()
                    .unwrap()
                    .contains(expected)
            );
        }
    }
    #[test]
    fn composed_unicode_subject_address_and_body_keep_utf8_semantics() {
        let args = json!({"to":["用户@example.test"],"subject":"中文主题","body":"中 café 😀"});
        let (raw, envelope) = compose(&args, "me@example.test", Some("<id@example.test>")).unwrap();
        let parsed = mailparse::parse_mail(&raw).unwrap();
        assert_eq!(
            parsed.headers.get_first_value("Subject").unwrap(),
            "中文主题"
        );
        assert_eq!(parsed.ctype.charset, "utf-8");
        assert_eq!(parsed.get_body().unwrap().trim_end(), "中 café 😀");
        let recipient: &str = envelope.to()[0].as_ref();
        assert_eq!(recipient, "用户@example.test");
    }
    #[test]
    fn empty_bodies_are_distinct_from_unsupported_or_over_budget() {
        for raw in [
            &b"Content-Type: text/plain\n\n"[..],
            &b"Content-Type: text/html\n\n<html><body></body></html>"[..],
        ] {
            assert_eq!(parsed(raw, 1, None, 1024).unwrap()["body"], "");
            assert!(forward_body(raw, "FYI").is_ok());
        }
        let raw = b"Content-Type: application/x-unknown\n\nnonempty original content";
        assert_eq!(
            parsed(raw, 1, None, 1024).unwrap_err().code,
            "UNSUPPORTED_BODY"
        );
        assert_eq!(
            forward_body(raw, "FYI").unwrap_err().code,
            "UNSUPPORTED_BODY"
        );
        for charset in ["x-unknown", "iso-2022-kr"] {
            let raw = format!("Content-Type: text/html; charset={charset}\n\n<p>original</p>");
            assert_eq!(
                parsed(raw.as_bytes(), 1, None, 1024).unwrap_err().code,
                "UNSUPPORTED_BODY"
            );
            assert_eq!(
                forward_body(raw.as_bytes(), "FYI").unwrap_err().code,
                "UNSUPPORTED_BODY"
            );
        }
        for foreign in [
            "<svg><text><![CDATA[CRITICAL_INVOICE]]></text></svg>",
            "<math><mtext>CRITICAL_INVOICE</mtext></math>",
        ] {
            let raw = format!("Content-Type: text/html\n\n<p>Visible intro</p>{foreign}");
            assert_eq!(
                parsed(raw.as_bytes(), 1, None, 1024).unwrap_err().code,
                "UNSUPPORTED_BODY"
            );
            assert_eq!(
                forward_body(raw.as_bytes(), "FYI").unwrap_err().code,
                "UNSUPPORTED_BODY"
            );
        }
        let raw = format!("Content-Type: text/html\n\n{}", "x".repeat(128 * 1024 + 1));
        assert_eq!(
            parsed(raw.as_bytes(), 1, None, 1024).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
        assert_eq!(
            forward_body(raw.as_bytes(), "FYI").unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
    }
    #[test]
    fn mail_consumers_enforce_mime_limits_and_recover() {
        let oversized_header = vec![b'x'; 33 * 1024];
        assert_eq!(
            parsed(&oversized_header, 1, None, 1024).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
        assert_eq!(
            attachment(&oversized_header, 0, 1024).unwrap_err().code,
            "LIMIT_EXCEEDED"
        );
        assert_eq!(
            prepare_draft(&oversized_header, "me@example.com", "<id@example.com>")
                .unwrap_err()
                .code,
            "LIMIT_EXCEEDED"
        );
        assert_eq!(
            parsed(b"Subject: ok\r\n\r\nhello", 1, None, 3).unwrap()["body"],
            "hel"
        );
        assert_eq!(attachment(b"\r\nhello", 0, 5).unwrap()["size"], 5);
    }
    #[test]
    fn attachment_filter_cursor_preserves_holes_and_empty_pages() {
        // Server candidates 499,490,480,470; only 490 and 470 have attachments.
        // Returning 490 must resume below the candidate consumed at 490.
        assert_eq!(page_cursor(1, &[490], 1, false, true, Some(490)), Some(490));
        // Exhausted sparse window advances even without an attachment match.
        assert_eq!(page_cursor(501, &[], 20, false, true, Some(520)), Some(501));
        assert_eq!(page_cursor(1, &[], 20, false, true, Some(20)), None);
        assert_eq!(
            page_cursor(501, &[980, 970], 2, true, false, None),
            Some(970)
        );
    }
    #[test]
    fn advanced_search_is_inclusive_bounded_and_quoted() {
        let query = search_query(&json!({"since":"2024-02-29","before":"2024-03-02","from":"a\"b","seen":false,"flagged":true,"answered":false,"min_size":100,"max_size":200}),1,500,true).unwrap();
        assert_eq!(
            query,
            "UID 1:500 FROM \"a\\\"b\" SINCE 29-Feb-2024 BEFORE 2-Mar-2024 UNSEEN FLAGGED UNANSWERED LARGER 99 SMALLER 201"
        );
        for date in [
            "2023-02-29",
            "2024-02-30",
            "0000-01-01",
            "2024-13-01",
            "2024-01-00",
            "2024-1-01",
            "é24-01-01",
        ] {
            assert!(imap_date(date).is_err());
        }
        assert!(
            search_query(
                &json!({"since":"2024-03-01","before":"2024-02-29"}),
                1,
                500,
                true
            )
            .is_err()
        );
        assert!(search_query(&json!({"from":"x\nALL"}), 1, 500, true).is_err());
        assert!(search_query(&json!({"min_size":201,"max_size":200}), 1, 500, true).is_err());
    }
    #[test]
    fn batch_requires_unique_explicit_uids_and_validity() {
        assert_eq!(
            batch_ids(&json!({"uids":[3,1],"uid_validity":7})).unwrap(),
            vec![3, 1]
        );
        for args in [
            json!({"uids":[1]}),
            json!({"uids":[1,1],"uid_validity":7}),
            json!({"uids":[],"uid_validity":7}),
            json!({"uids":[0],"uid_validity":7}),
            json!({"uids":vec![1;51],"uid_validity":7}),
        ] {
            assert!(batch_ids(&args).is_err());
        }
        let result = batch_unprocessed(&[5, 3]);
        assert_eq!(result["items"].as_array().unwrap().len(), 2);
        assert_eq!(result["items"][1]["status"], "unprocessed");
        assert_eq!(result["retry_allowed"], false);
    }
    async fn smtp_fixture(disconnect_after_data: bool, fail_journal: bool, slow_quit: bool) {
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
            let mut payload = String::new();
            loop {
                let mut line = String::new();
                if socket.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                if data {
                    if line == ".\r\n" {
                        if fail_journal {
                            backend_journal.fail_writes();
                        }
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
                    if !fail_journal {
                        assert_eq!(
                            backend_journal
                                .get("owner", "test-send")
                                .unwrap()
                                .unwrap()
                                .status,
                            "accepted"
                        );
                    }
                    if slow_quit {
                        // Longer than the transaction deadline: acceptance was
                        // already journaled and must survive bounded cleanup.
                        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                        break;
                    }
                    socket.get_mut().write_all(b"221 bye\r\n").await.unwrap();
                    break;
                } else {
                    panic!("Unexpected mock SMTP command");
                }
            }
            assert!(payload.contains("Subject: synthetic"));
            assert!(!payload.contains("hidden@example.com"));
        });
        let mut service = MailService::new(crate::config::fixture(), journal.clone()).unwrap();
        service.smtp = SmtpSender::fixture(port, std::time::Duration::from_millis(300));
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
        if fail_journal {
            assert!(
                result["operation"]["warnings"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("journal_update_failed"))
            );
        } else {
            let repeat = service.send("send_email", &args, "owner").await.unwrap();
            assert_eq!(
                repeat["operation"]["message_id"],
                result["operation"]["message_id"]
            );
        }
        backend.await.unwrap();
    }
    #[tokio::test]
    async fn smtp_acceptance_then_storage_failure() {
        smtp_fixture(false, true, false).await;
    }
    #[tokio::test]
    async fn smtp_lost_reply_does_not_resend() {
        smtp_fixture(true, false, false).await;
    }
    #[tokio::test]
    async fn smtp_accepted_send_is_deduplicated() {
        smtp_fixture(false, false, false).await;
    }
    #[tokio::test]
    async fn smtp_accepted_is_persisted_before_unresponsive_quit() {
        smtp_fixture(false, false, true).await;
    }
    #[tokio::test]
    async fn smtp_accepted_survives_quit_timeout_and_journal_failure() {
        smtp_fixture(false, true, true).await;
    }
    #[test]
    fn smtp_success_survives_journal_failure() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("db")).unwrap();
        journal.claim("owner", "key", "hash", "id").unwrap();
        journal.fail_writes();
        let result = record_send(&journal, "owner", "key", "accepted", "id".into(), true);
        assert_eq!(result.status, "accepted");
        assert!(result.warnings.contains(&"journal_update_failed".into()));
        assert!(result.warnings.contains(&"draft_retained".into()));
        assert_eq!(
            journal.get("owner", "key").unwrap().unwrap().status,
            "sending"
        );
    }
    #[test]
    fn ambiguous_send_remains_non_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&dir.path().join("db")).unwrap();
        journal.claim("owner", "key", "hash", "id").unwrap();
        let result = record_send(&journal, "owner", "key", "unknown", "id".into(), false);
        assert_eq!(result.status, "unknown");
        assert_eq!(
            journal
                .claim("owner", "key", "hash", "id")
                .unwrap()
                .unwrap()
                .status,
            "unknown"
        );
    }
    #[tokio::test]
    async fn bounded_parser_accepts_fragmented_text_and_normal_literal() {
        use tokio::io::AsyncWriteExt;
        let (input, mut output) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            for byte in
                b"* OK ordinary text {2048}\r\n* 1 FETCH (UID 1 BODY[] {10}\r\n{999999}\r\n)\r\n"
            {
                output.write_all(&[*byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let (stream, _) = BudgetStream::new(input, 1024);
        let mut client = async_imap::Client::with_buffer_limit(stream, 1024);
        assert!(client.read_response().await.unwrap().is_some());
        assert!(client.read_response().await.unwrap().is_some());
        writer.await.unwrap();
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

    #[tokio::test]
    async fn bounded_parser_handles_exact_text_budget_and_unknown_need() {
        use tokio::io::AsyncWriteExt;
        for complete in [true, false] {
            let (input, mut output) = tokio::io::duplex(2048);
            let mut text = b"* OK ".to_vec();
            text.resize(if complete { 1022 } else { 1024 }, b'a');
            if complete {
                text.extend_from_slice(b"\r\n");
            }
            output.write_all(&text).await.unwrap();
            let (stream, _) = BudgetStream::new(input, 1024);
            let mut client = async_imap::Client::with_buffer_limit(stream, 1024);
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(1), client.read_response())
                    .await
                    .unwrap();
            assert_eq!(result.is_ok(), complete);
        }
    }

    #[test]
    fn saved_draft_keeps_bcc_but_sent_message_strips_it() {
        let args = json!({"to":["a@example.com"],"bcc":["secret@example.com"],"subject":"s","body":"hello"});
        let (draft, _) = compose(&args, "me@example.com", None).unwrap();
        assert!(String::from_utf8_lossy(&draft).contains("Bcc: secret@example.com"));
        let (sent, envelope) = compose(&args, "me@example.com", Some("<id@example.com>")).unwrap();
        assert!(!String::from_utf8_lossy(&sent).contains("secret@example.com"));
        assert_eq!(envelope.to().len(), 2);
    }
    #[test]
    fn draft_keeps_bcc_envelope_and_mime() {
        let raw=b"From: old@example.com\r\nTo: a@example.com\r\nBcc: secret@example.com,\r\n hidden@example.com\r\nSubject: draft\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nhello\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--x--\r\n";
        let (clean, e) = prepare_draft(raw, "me@example.com", "<id@example.com>").unwrap();
        assert_eq!(e.to().len(), 3);
        assert!(!String::from_utf8_lossy(&clean).contains("secret@example"));
        assert!(clean.ends_with(b"YWJj\r\n--x--\r\n"));
    }
    #[test]
    fn reject_injection_and_local_attachments() {
        assert!(mailbox_name("x\r\nDELETE INBOX").is_err());
        assert!(compose(&json!({"to":["a@example.com"],"subject":"x","body":"x","attachments":["/etc/passwd"]}),"me@example.com",None).is_err());
    }
    #[tokio::test]
    async fn stream_stops_at_actual_limit() {
        use tokio::io::AsyncReadExt;
        let input = &b"123456789"[..];
        let (mut stream, _) = BudgetStream::new(input, 4);
        let mut bytes = Vec::new();
        assert!(stream.read_to_end(&mut bytes).await.is_err());
        assert_eq!(bytes, b"1234");
    }
}
