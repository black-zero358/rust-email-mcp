use crate::{
    config::Config,
    error::{MailError, Result},
    journal::{Journal, Operation},
};
use futures::TryStreamExt;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor, address::Envelope, message::Mailbox,
    transport::smtp::authentication::Credentials,
};
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

/// Limits bytes before the IMAP parser can allocate an attacker-controlled literal.
#[derive(Debug)]
pub struct BudgetStream<T> {
    inner: T,
    remaining: Arc<AtomicUsize>,
    line: Vec<u8>,
    literal_remaining: usize,
    max_literal: usize,
}
impl<T> BudgetStream<T> {
    pub fn new(inner: T, budget: usize) -> (Self, Arc<AtomicUsize>) {
        let remaining = Arc::new(AtomicUsize::new(budget));
        (
            Self {
                inner,
                remaining: remaining.clone(),
                line: Vec::new(),
                literal_remaining: 0,
                max_literal: budget,
            },
            remaining,
        )
    }
    fn inspect(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        for &byte in bytes {
            if self.literal_remaining > 0 {
                self.literal_remaining -= 1;
                continue;
            }
            if self.line.len() >= 65536 {
                return Err(std::io::Error::other("IMAP line limit"));
            }
            self.line.push(byte);
            if byte == b'\n' {
                let line = self.line.strip_suffix(b"\r\n").unwrap_or(&self.line);
                if let Some(prefix) = line.strip_suffix(b"}")
                    && let Some(start) = prefix.iter().rposition(|b| *b == b'{')
                {
                    let digits = &prefix[start + 1..];
                    let digits = digits.strip_suffix(b"+").unwrap_or(digits);
                    if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) {
                        let n = std::str::from_utf8(digits)
                            .ok()
                            .and_then(|s| s.parse::<usize>().ok())
                            .ok_or_else(|| std::io::Error::other("Invalid literal size"))?;
                        if n > self.max_literal || n > self.remaining.load(Ordering::Relaxed) {
                            return Err(std::io::Error::other("IMAP literal limit"));
                        }
                        self.literal_remaining = n;
                    }
                }
                self.line.clear();
            }
        }
        Ok(())
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
                self.inspect(limited.filled())?;
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
    smtp: AsyncSmtpTransport<Tokio1Executor>,
}
impl MailService {
    pub fn new(config: Arc<Config>, journal: Arc<Journal>) -> anyhow::Result<Self> {
        let builder = if config.smtp_starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_host)?
        };
        let smtp = builder
            .port(config.smtp_port)
            .credentials(Credentials::new(
                config.address.clone(),
                config.password.clone(),
            ))
            .timeout(Some(config.timeout))
            .build();
        Ok(Self {
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
        let (stream, _) = BudgetStream::new(stream, self.config.max_message + 2 * 1024 * 1024);
        let mut client = async_imap::Client::new(stream);
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
    async fn with_session(&self, s: &mut Session, name: &str, a: &Value) -> Result<Value> {
        match name {
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
                Ok(
                    json!({"mailboxes":list.iter().map(|m|json!({"name":m.name(),"attributes":format!("{:?}",m.attributes())})).collect::<Vec<_>>()}),
                )
            }
            "list_emails" | "search_emails" => self.list(s, a, name == "search_emails").await,
            "get_email" | "get_email_status" | "download_attachment" => {
                let folder = folder(a)?;
                let uid = uid(a)?;
                let state = s.examine(folder).await.map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                if name == "get_email_status" {
                    let f = s
                        .uid_fetch(uid.to_string(), "(UID FLAGS)")
                        .await
                        .map_err(|_| MailError::backend())?
                        .try_collect::<Vec<_>>()
                        .await
                        .map_err(|_| MailError::backend())?;
                    return Ok(
                        json!({"uid":uid,"uid_validity":state.uid_validity,"flags":f.iter().flat_map(|x|x.flags().map(|f|format!("{f:?}"))).collect::<Vec<_>>()}),
                    );
                }
                let raw = self.raw(s, uid).await?;
                if name == "download_attachment" {
                    return attachment(
                        &raw,
                        number(a, "attachment_index", 0, 0, 1000)? as usize,
                        self.config.max_output / 2,
                    );
                }
                parsed(&raw, uid, state.uid_validity, self.config.max_output / 2)
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
                    .examine(folder(a)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                let mut items = Vec::new();
                for id in ids {
                    let id = id
                        .as_u64()
                        .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                        .ok_or_else(MailError::input)? as u32;
                    match self
                        .raw(s, id)
                        .await
                        .and_then(|raw| parsed(&raw, id, state.uid_validity, 16000))
                    {
                        Ok(value) => items.push(json!({"uid":id,"ok":true,"email":value})),
                        Err(error) => {
                            items.push(json!({"uid":id,"ok":false,"error":error}));
                            break;
                        }
                    }
                }
                Ok(json!({"items":items,"requested":ids.len(),"completed":items.len()}))
            }
            "save_draft" => {
                let (raw, _) = compose(a, &self.config.address, None)?;
                if raw.len() > self.config.max_message {
                    return Err(MailError::limit());
                }
                s.append(&self.config.drafts, Some("\\Draft"), None, &raw)
                    .await
                    .map_err(|_| MailError::backend())?;
                Ok(json!({"status":"saved","mailbox":self.config.drafts}))
            }
            "create_mailbox" => {
                s.create(text(a, "name")?)
                    .await
                    .map_err(|_| MailError::backend())?;
                Ok(json!({"status":"created"}))
            }
            "rename_mailbox" => {
                s.rename(text(a, "name")?, text(a, "new_name")?)
                    .await
                    .map_err(|_| MailError::backend())?;
                Ok(json!({"status":"renamed"}))
            }
            "delete_mailbox" => {
                if a["confirm"] != true {
                    return Err(MailError::input());
                }
                s.delete(text(a, "name")?)
                    .await
                    .map_err(|_| MailError::backend())?;
                Ok(json!({"status":"deleted"}))
            }
            "move_email" | "delete_email" | "mark_email" => {
                let state = s
                    .select(folder(a)?)
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
                        s.uid_mv(&uid, dest)
                            .await
                            .map_err(|_| MailError::backend())?;
                    }
                }
                Ok(json!({"status":"updated"}))
            }
            _ => Err(MailError::input()),
        }
    }
    async fn raw(&self, s: &mut Session, uid: u32) -> Result<Vec<u8>> {
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
    async fn list(&self, s: &mut Session, a: &Value, search: bool) -> Result<Value> {
        let m = s
            .examine(folder(a)?)
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
        let query = if search {
            format!("UID {start}:{end} TEXT {}", quote(text(a, "query")?)?)
        } else {
            format!("UID {start}:{end}")
        };
        let mut ids = s
            .uid_search(query)
            .await
            .map_err(|_| MailError::backend())?
            .into_iter()
            .filter(|u| *u >= start && *u <= end)
            .collect::<Vec<_>>();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let more = ids.len() > limit;
        ids.truncate(limit);
        let next = if more {
            ids.last().copied()
        } else if start > 1 {
            Some(start)
        } else {
            None
        };
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
            let headers = mailparse::parse_headers(f.header().unwrap_or_default())
                .map_err(|_| MailError::backend())?
                .0;
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
                    .examine(&self.config.drafts)
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
                    .examine(folder(a)?)
                    .await
                    .map_err(|_| MailError::backend())?;
                check_validity(a, state.uid_validity)?;
                self.raw(&mut s, uid(a)?).await
            })
            .await
            .map_err(|_| MailError::timeout())??;
            let p = mailparse::parse_mail(&original).map_err(|_| MailError::backend())?;
            let mut args = a.clone();
            let prefix = if name == "reply_email" { "Re:" } else { "Fwd:" };
            args["subject"] = json!(format!(
                "{prefix} {}",
                p.headers.get_first_value("Subject").unwrap_or_default()
            ));
            if name == "reply_email" {
                args["in_reply_to"] =
                    json!(p.headers.get_first_value("Message-ID").unwrap_or_default());
            }
            if name == "forward_email" {
                args["body"] = json!(format!(
                    "{}\n\n---------- Forwarded message ----------\n{}",
                    text(a, "body")?,
                    plain_body(&p, 0)?
                ));
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
        let result =
            tokio::time::timeout(self.config.timeout, self.smtp.send_raw(&envelope, &raw)).await;
        let status = match result {
            Ok(Ok(_)) => "accepted",
            Ok(Err(ref e)) if e.is_permanent() || e.is_transient() => "rejected",
            _ => "unknown",
        };
        let operation = record_send(
            &self.journal,
            principal,
            key,
            status,
            message_id,
            name == "send_draft",
        );
        Ok(json!({"operation":operation,"retry_allowed":false}))
    }
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
fn quote(s: &str) -> Result<String> {
    if s.len() > 512 || s.contains(['\r', '\n', '\0']) {
        return Err(MailError::input());
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
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
        .body(text(a, "body")?.to_owned())
        .map_err(|_| MailError::input())?;
    Ok((message.formatted(), message.envelope().clone()))
}
pub fn prepare_draft(raw: &[u8], from: &str, id: &str) -> Result<(Vec<u8>, Envelope)> {
    let parsed = mailparse::parse_mail(raw).map_err(|_| MailError::input())?;
    let mut recipients = Vec::new();
    for key in ["To", "Cc", "Bcc"] {
        for val in parsed.headers.get_all_values(key) {
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
    let header_end = raw
        .windows(4)
        .position(|x| x == b"\r\n\r\n")
        .map(|p| (p, 4))
        .or_else(|| raw.windows(2).position(|x| x == b"\n\n").map(|p| (p, 2)))
        .ok_or_else(MailError::input)?;
    let headers = std::str::from_utf8(&raw[..header_end.0]).map_err(|_| MailError::input())?;
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
    clean.extend_from_slice(&raw[header_end.0 + header_end.1..]);
    Ok((clean, envelope))
}
fn plain_body(p: &mailparse::ParsedMail<'_>, depth: usize) -> Result<String> {
    if depth > 20 {
        return Err(MailError::limit());
    }
    if p.subparts.is_empty() {
        if p.ctype.mimetype == "text/plain" {
            return p.get_body().map_err(|_| MailError::backend());
        }
        return Ok(String::new());
    }
    let mut out = String::new();
    for part in &p.subparts {
        out.push_str(&plain_body(part, depth + 1)?);
    }
    Ok(out)
}
fn leaves<'a, 'b>(
    p: &'a mailparse::ParsedMail<'b>,
    out: &mut Vec<&'a mailparse::ParsedMail<'b>>,
    depth: usize,
) -> Result<()> {
    if depth > 20 || out.len() > 1000 {
        return Err(MailError::limit());
    }
    if p.subparts.is_empty() {
        out.push(p);
    } else {
        for part in &p.subparts {
            leaves(part, out, depth + 1)?;
        }
    }
    Ok(())
}
fn parsed(raw: &[u8], uid: u32, validity: Option<u32>, max: usize) -> Result<Value> {
    let p = mailparse::parse_mail(raw).map_err(|_| MailError::backend())?;
    let mut body = plain_body(&p, 0)?;
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
    let attachments=parts.iter().enumerate().filter(|(_,p)|p.get_content_disposition().disposition==mailparse::DispositionType::Attachment).map(|(i,p)|json!({"attachment_index":i,"content_type":p.ctype.mimetype,"filename":p.get_content_disposition().params.get("filename")})).collect::<Vec<_>>();
    Ok(
        json!({"uid":uid,"uid_validity":validity,"subject":p.headers.get_first_value("Subject"),"from":p.headers.get_first_value("From"),"to":p.headers.get_first_value("To"),"body":body,"body_truncated":truncated,"attachments":attachments,"content_trust":"untrusted"}),
    )
}
fn attachment(raw: &[u8], index: usize, max: usize) -> Result<Value> {
    use base64::Engine;
    let p = mailparse::parse_mail(raw).map_err(|_| MailError::backend())?;
    let mut parts = Vec::new();
    leaves(&p, &mut parts, 0)?;
    let part = parts.get(index).ok_or_else(MailError::input)?;
    let bytes = part.get_body_raw().map_err(|_| MailError::backend())?;
    if bytes.len() > max {
        return Err(MailError::limit());
    }
    Ok(
        json!({"content_type":part.ctype.mimetype,"size":bytes.len(),"base64":base64::engine::general_purpose::STANDARD.encode(bytes)}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn smtp_fixture(disconnect_after_data: bool, fail_journal: bool) {
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
        service.smtp = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
            .port(port)
            .build();
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
        smtp_fixture(false, true).await;
    }
    #[tokio::test]
    async fn smtp_lost_reply_does_not_resend() {
        smtp_fixture(true, false).await;
    }
    #[tokio::test]
    async fn smtp_accepted_send_is_deduplicated() {
        smtp_fixture(false, false).await;
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
    #[test]
    fn rejects_split_oversized_literal_before_parser() {
        let (mut stream, _) = BudgetStream::new(&b""[..], 1024);
        stream.inspect(b"* 1 FETCH (BODY[] {536870").unwrap();
        assert!(stream.inspect(b"912}\r\n").is_err());
    }
    #[test]
    fn literal_contents_are_not_parsed_as_protocol() {
        let (mut stream, _) = BudgetStream::new(&b""[..], 1024);
        stream
            .inspect(b"* 1 FETCH (BODY[] {10}\r\n{999999}\r\n)\r\n")
            .unwrap();
        assert_eq!(stream.literal_remaining, 0);
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
        assert!(quote("x\r\nDELETE INBOX").is_err());
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
