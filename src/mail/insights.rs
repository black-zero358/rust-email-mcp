//! Bounded read-only mailbox insights. Cached data contains immutable headers only.
use super::*;
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

const HEADER_LIMIT: usize = 16 * 1024;
const CACHE_BYTES: usize = 2 * 1024 * 1024;
const CACHE_ITEMS: usize = 128;
const CACHE_TTL: Duration = Duration::from_secs(30);

struct Entry {
    folder: String,
    validity: u32,
    uid: u32,
    inserted: Instant,
    headers: Vec<u8>,
}

#[derive(Default)]
pub(super) struct MetadataCache {
    entries: VecDeque<Entry>,
}
impl MetadataCache {
    fn get(
        &mut self,
        folder: &str,
        validity: Option<u32>,
        uid: u32,
        now: Instant,
    ) -> Option<Vec<u8>> {
        self.entries.retain(|e| {
            now.duration_since(e.inserted) < CACHE_TTL
                && (e.folder != folder || Some(e.validity) == validity)
        });
        let validity = validity?;
        self.entries
            .iter()
            .find(|e| e.folder == folder && e.validity == validity && e.uid == uid)
            .map(|e| e.headers.clone())
    }
    fn insert(&mut self, folder: &str, validity: Option<u32>, uid: u32, headers: Vec<u8>) {
        let Some(validity) = validity else { return };
        if headers.len() > HEADER_LIMIT {
            return;
        }
        self.entries
            .retain(|e| !(e.folder == folder && e.uid == uid));
        while self.entries.len() >= CACHE_ITEMS
            || self.entries.iter().map(|e| e.headers.len()).sum::<usize>() + headers.len()
                > CACHE_BYTES
        {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            folder: folder.into(),
            validity,
            uid,
            inserted: Instant::now(),
            headers,
        });
    }
}

/// Call only after EXAMINE and validating the caller's UIDVALIDITY in this session.
impl MailService {
    pub(super) fn remember_headers(
        &self,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
        bytes: &[u8],
    ) {
        // Partial HEADER fetches must never masquerade as complete immutable metadata.
        if bytes.len() >= HEADER_LIMIT
            || !(bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\n\n"))
        {
            return;
        }
        if let Ok(mut cache) = self.metadata_cache.lock() {
            cache.insert(mailbox, validity, uid, bytes.to_vec());
        }
    }

    pub(super) async fn cached_headers<S>(
        &self,
        s: &mut async_imap::Session<S>,
        mailbox: &str,
        validity: Option<u32>,
        uid: u32,
    ) -> Result<Vec<u8>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        if let Some(headers) = self
            .metadata_cache
            .lock()
            .map_err(|_| MailError::backend())?
            .get(mailbox, validity, uid, Instant::now())
        {
            return Ok(headers);
        }
        let items = s
            .uid_fetch(
                uid.to_string(),
                format!("(UID BODY.PEEK[HEADER]<0.{}>)", HEADER_LIMIT + 1),
            )
            .await
            .map_err(|_| MailError::backend())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|_| MailError::backend())?;
        let bytes = items
            .iter()
            .find(|f| f.uid == Some(uid))
            .and_then(|f| f.header())
            .ok_or_else(MailError::backend)?;
        if bytes.len() > HEADER_LIMIT || !(bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\n\n"))
        {
            return Err(MailError::limit());
        }
        let headers = bytes.to_vec();
        self.metadata_cache
            .lock()
            .map_err(|_| MailError::backend())?
            .insert(mailbox, validity, uid, headers.clone());
        Ok(headers)
    }

    pub(super) async fn insight_operation<S>(
        &self,
        s: &mut async_imap::Session<S>,
        name: &str,
        a: &Value,
    ) -> Result<Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        match name {
            "locate_message" => self.locate(s, a).await,
            "read_conversation" => self.conversation(s, a).await,
            "get_mailbox_statistics" => self.statistics(s, a).await,
            _ => Err(MailError::input()),
        }
    }

    async fn locate<S>(&self, s: &mut async_imap::Session<S>, a: &Value) -> Result<Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let target = message_id(text(a, "message_id")?)?;
        let folders = explicit_folders(a)?;
        let candidates = number(a, "candidate_limit", 10, 1, 50)? as usize;
        let limit = number(a, "limit", 20, 1, 50)? as usize;
        let mut matches = Vec::new();
        let mut scopes = Vec::new();
        for mailbox in folders {
            let state = s
                .examine(mailbox_name(mailbox)?)
                .await
                .map_err(|_| MailError::backend())?;
            require_reference(&state)?;
            let (start, end) = window(a, state.uid_next)?;
            let mut ids = if state.exists == 0 || end == 0 {
                Vec::new()
            } else {
                s.uid_search(format!(
                    "UID {start}:{end} HEADER Message-ID {}",
                    quote(&target)?
                ))
                .await
                .map_err(|_| MailError::backend())?
                .into_iter()
                .filter(|id| *id >= start && *id <= end)
                .collect::<Vec<_>>()
            };
            ids.sort_unstable_by(|a, b| b.cmp(a));
            let omitted = ids.len() > candidates;
            ids.truncate(candidates);
            let mut checked = 0;
            for &id in &ids {
                if matches.len() >= limit {
                    break;
                }
                let headers = self
                    .cached_headers(s, mailbox, state.uid_validity, id)
                    .await?;
                checked += 1;
                let h = thread_headers(&headers)?;
                // IMAP HEADER is substring matching; verify the entire parsed identifier.
                if h.id.as_deref() == Some(&target) {
                    matches.push(json!({"mailbox":mailbox,"uid":id,"uid_validity":state.uid_validity,"message_id":target}));
                }
            }
            scopes.push(json!({"mailbox":mailbox,"uid_validity":state.uid_validity,"start_uid":start,"end_uid":end,
                "next_before_uid":scope_cursor(start,end,&ids,checked,omitted),
                "candidates_checked":checked,"candidate_limit_reached":omitted || checked<ids.len(),"older_uids_omitted":start>1}));
        }
        Ok(
            json!({"matches":matches,"scopes":scopes,"complete":false,"scope":"Only explicitly supplied folders and reported UID windows were searched; follow each cursor."}),
        )
    }

    async fn conversation<S>(&self, s: &mut async_imap::Session<S>, a: &Value) -> Result<Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let mailbox = folder(a)?;
        let seed = uid(a)?;
        let state = s
            .examine(mailbox_name(mailbox)?)
            .await
            .map_err(|_| MailError::backend())?;
        require_reference(&state)?;
        check_validity(a, state.uid_validity)?;
        let candidate_limit = number(a, "candidate_limit", 100, 1, 200)? as usize;
        let limit = number(a, "limit", 10, 1, 20)? as usize;
        let seed_headers = self
            .cached_headers(s, mailbox, state.uid_validity, seed)
            .await?;
        let seed_links = thread_headers(&seed_headers)?;
        let (start, end) = window(a, state.uid_next)?;
        let mut ids = if state.exists == 0 || end == 0 {
            Vec::new()
        } else {
            s.uid_search(format!("UID {start}:{end}"))
                .await
                .map_err(|_| MailError::backend())?
                .into_iter()
                .filter(|id| *id >= start && *id <= end && *id != seed)
                .collect::<Vec<_>>()
        };
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let candidate_limit_reached = ids.len() > candidate_limit;
        ids.truncate(candidate_limit);
        let next = if candidate_limit_reached {
            ids.last().copied()
        } else if start > 1 {
            Some(start)
        } else {
            None
        };
        let mut headers = Vec::new();
        for id in ids {
            let raw = self
                .cached_headers(s, mailbox, state.uid_validity, id)
                .await?;
            headers.push((id, thread_headers(&raw)?));
        }
        let (connected, linked) = connected_messages(seed, &seed_links, &headers, usize::MAX);
        let selected = connected.iter().copied().take(limit).collect::<Vec<_>>();
        let total_budget = self.config.max_output.saturating_div(2).min(65536);
        let mut remaining = total_budget;
        let mut messages = Vec::new();
        let mut output_remaining = self.config.max_output.saturating_sub(4096);
        let mut response_budget_omitted = false;
        for id in selected {
            let raw = self.raw(s, id).await?;
            let mut value =
                parsed_with_options(&raw, id, state.uid_validity, remaining.min(16000), a)?;
            bound_conversation_metadata(&mut value);
            let bytes = serde_json::to_vec(&value)
                .map_err(|_| MailError::backend())?
                .len()
                + 1;
            if bytes > output_remaining {
                response_budget_omitted = true;
                break;
            }
            output_remaining -= bytes;
            remaining = remaining.saturating_sub(value["body"].as_str().map_or(0, str::len));
            messages.push(value);
        }
        let omitted_uids = connected
            .into_iter()
            .filter(|id| {
                !messages
                    .iter()
                    .any(|m| m["uid"].as_u64() == Some(*id as u64))
            })
            .collect::<Vec<_>>();
        Ok(
            json!({"mailbox":mailbox,"uid_validity":state.uid_validity,"messages":messages,"omitted_uids":omitted_uids,
            "threading":"Message-ID/References/In-Reply-To exact identifier graph; no subject matching",
            "scope":{"start_uid":start,"end_uid":end,"candidates_checked":headers.len(),"seed_uid":seed},
            "next_before_uid":next,"candidate_limit_reached":candidate_limit_reached,"messages_omitted":linked>messages.len(),
            "response_budget_omitted":response_budget_omitted,
            "older_uids_omitted":start>1,"total_body_byte_limit":total_budget,
            "complete":!candidate_limit_reached && start<=1 && linked<=messages.len(),"content_trust":"untrusted"}),
        )
    }

    async fn statistics<S>(&self, s: &mut async_imap::Session<S>, a: &Value) -> Result<Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug,
    {
        let mailbox = folder(a)?;
        // STATUS UNSEEN is a count; EXAMINE UNSEEN is a sequence number.
        let status = crate::imap_text::status_counts(s, mailbox).await?;
        let mut result = json!({"mailbox":mailbox,"messages":status.exists,"unseen":status.unseen,"recent":status.recent,
            "uid_validity":status.uid_validity,"uid_next":status.uid_next,"snapshot_atomic":false});
        if a.get("date_start").is_some() || a.get("date_end").is_some() {
            let start = CivilDate::parse(text(a, "date_start")?)?;
            let end = CivilDate::parse(text(a, "date_end")?)?;
            let days = end.ordinal() - start.ordinal();
            if !(1..=31).contains(&days) {
                return Err(MailError::input());
            }
            let state = s
                .examine(mailbox_name(mailbox)?)
                .await
                .map_err(|_| MailError::backend())?;
            require_reference(&state)?;
            let (lower, upper) = window(a, state.uid_next)?;
            let mut day = start;
            let mut distribution = Vec::new();
            for _ in 0..days {
                let next = day.next();
                let count = if state.exists == 0 || upper == 0 {
                    0
                } else {
                    s.uid_search(format!(
                        "UID {lower}:{upper} SINCE {} BEFORE {}",
                        day.imap(),
                        next.imap()
                    ))
                    .await
                    .map_err(|_| MailError::backend())?
                    .iter()
                    .filter(|id| **id >= lower && **id <= upper)
                    .count()
                };
                distribution.push(json!({"date":day.iso(),"messages":count}));
                day = next;
            }
            result["distribution"] = json!(distribution);
            result["distribution_scope"] = json!({"basis":"IMAP INTERNALDATE server calendar day","date_end_exclusive":end.iso(),
                "start_uid":lower,"end_uid":upper,"older_uids_omitted":lower>1,"next_before_uid":if lower>1 {Some(lower)} else {None}});
        }
        Ok(result)
    }

    /// Every returned string is static. Never return hostnames, credentials, banners or backend errors.
    pub(super) async fn diagnose_connectivity(&self) -> Result<Value> {
        let mut stages = vec![json!({"stage":"mcp_auth","status":"passed"})];
        let diagnose = async {
            let tcp =
                match TcpStream::connect((&*self.config.imap_host, self.config.imap_port)).await {
                    Ok(tcp) => tcp,
                    Err(_) => return diagnostic_failure(&mut stages, "tcp"),
                };
            stages.push(json!({"stage":"tcp","status":"passed"}));
            let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let tls = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = match ServerName::try_from(self.config.imap_host.clone()) {
                Ok(name) => name,
                Err(_) => return diagnostic_failure(&mut stages, "tls"),
            };
            let stream = match TlsConnector::from(Arc::new(tls)).connect(name, tcp).await {
                Ok(stream) => stream,
                Err(_) => return diagnostic_failure(&mut stages, "tls"),
            };
            stages.push(json!({"stage":"tls","status":"passed"}));
            let (stream, _) = BudgetStream::new(stream, 128 * 1024);
            let mut client = async_imap::Client::new(stream);
            if !client
                .read_response()
                .await
                .ok()
                .flatten()
                .is_some_and(|response| {
                    matches!(
                        response.parsed(),
                        imap_proto::Response::Data {
                            status: imap_proto::Status::Ok,
                            ..
                        }
                    )
                })
            {
                return diagnostic_failure(&mut stages, "greeting");
            }
            stages.push(json!({"stage":"greeting","status":"passed"}));
            let mut session = match client
                .login(&self.config.address, &self.config.password)
                .await
            {
                Ok(session) => session,
                Err(_) => return diagnostic_failure(&mut stages, "login"),
            };
            stages.push(json!({"stage":"login","status":"passed"}));
            let capabilities = match session.capabilities().await {
                Ok(caps) => json!({"move":caps.has_str("MOVE"),"uidplus":caps.has_str("UIDPLUS"),
                    "imap4rev1":caps.has_str("IMAP4rev1"),"imap4rev2":caps.has_str("IMAP4rev2")}),
                Err(_) => return diagnostic_failure(&mut stages, "capability"),
            };
            stages.push(json!({"stage":"capability","status":"passed"}));
            json!({"ok":true,"stages":stages,"capabilities":capabilities,"service":"imap","smtp_tested":false})
        };
        match tokio::time::timeout(self.config.timeout, diagnose).await {
            Ok(value) => Ok(value),
            Err(_) => Ok(
                json!({"ok":false,"stages":stages,"failure":"timeout","service":"imap","smtp_tested":false}),
            ),
        }
    }
}

fn require_reference(state: &async_imap::types::Mailbox) -> Result<()> {
    if !state.uid_validity.is_some_and(|n| n > 0) || !state.uid_next.is_some_and(|n| n > 0) {
        return Err(MailError::backend());
    }
    Ok(())
}

fn diagnostic_failure(stages: &mut Vec<Value>, stage: &str) -> Value {
    stages.push(json!({"stage":stage,"status":"failed"}));
    json!({"ok":false,"stages":stages,"failure":"connection_check_failed","service":"imap","smtp_tested":false})
}

fn explicit_folders(a: &Value) -> Result<Vec<&str>> {
    let array = a["mailboxes"].as_array().ok_or_else(MailError::input)?;
    if array.is_empty() || array.len() > 20 {
        return Err(MailError::input());
    }
    let mut folders = Vec::new();
    for value in array {
        let name = value
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
            .ok_or_else(MailError::input)?;
        if !folders.contains(&name) {
            folders.push(name);
        }
    }
    Ok(folders)
}
fn window(a: &Value, next: Option<u32>) -> Result<(u32, u32)> {
    let before = number(
        a,
        "before_uid",
        next.unwrap_or(u32::MAX) as u64,
        1,
        u32::MAX as u64,
    )? as u32;
    let end = before - 1;
    Ok((end.saturating_sub(499).max(1), end))
}
fn scope_cursor(start: u32, end: u32, ids: &[u32], checked: usize, omitted: bool) -> Option<u32> {
    if checked == 0 && !ids.is_empty() {
        Some(end + 1)
    } else if omitted || checked < ids.len() {
        ids.get(checked.checked_sub(1)?).copied()
    } else if start > 1 {
        Some(start)
    } else {
        None
    }
}
fn message_id(input: &str) -> Result<String> {
    let id = input.trim();
    if id.len() < 5
        || id.len() > 998
        || !id.is_ascii()
        || !id.starts_with('<')
        || !id.ends_with('>')
        || !id.contains('@')
        || id[1..id.len() - 1]
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>')
    {
        return Err(MailError::input());
    }
    Ok(id.into())
}
fn identifiers(value: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = value;
    while let Some(begin) = rest.find('<') {
        rest = &rest[begin..];
        let Some(end) = rest.find('>') else { break };
        if let Ok(id) = message_id(&rest[..=end]) {
            ids.push(id);
        }
        rest = &rest[end + 1..];
    }
    ids
}
struct ThreadHeaders {
    id: Option<String>,
    links: Vec<String>,
}
impl ThreadHeaders {
    fn identifiers(&self) -> HashSet<String> {
        self.id.iter().chain(self.links.iter()).cloned().collect()
    }
}
fn thread_headers(raw: &[u8]) -> Result<ThreadHeaders> {
    let (headers, _) = (mime::headers(raw)?, 0);
    let id = headers
        .get_first_value("Message-ID")
        .and_then(|s| message_id(&s).ok());
    let mut links = Vec::new();
    for header in ["References", "In-Reply-To"] {
        for value in headers.get_all_values(header) {
            links.extend(identifiers(&value));
        }
    }
    Ok(ThreadHeaders { id, links })
}
fn connected_messages(
    seed: u32,
    seed_headers: &ThreadHeaders,
    headers: &[(u32, ThreadHeaders)],
    limit: usize,
) -> (Vec<u32>, usize) {
    let mut connected = seed_headers.identifiers();
    let mut selected = vec![seed];
    loop {
        let old = selected.len();
        for (uid, h) in headers {
            if !selected.contains(uid) && h.identifiers().iter().any(|id| connected.contains(id)) {
                connected.extend(h.identifiers());
                selected.push(*uid);
            }
        }
        if selected.len() == old {
            let linked = selected.len();
            // Always retain the requested seed, then return other messages in UID order.
            selected[1..].sort_unstable();
            selected.truncate(limit);
            return (selected, linked);
        }
    }
}

pub(super) fn parsed_with_options(
    raw: &[u8],
    uid: u32,
    validity: Option<u32>,
    hard_limit: usize,
    a: &Value,
) -> Result<Value> {
    let mode = a.get("body_mode").and_then(Value::as_str).unwrap_or("full");
    if !["full", "trimmed"].contains(&mode) || a.get("body_mode").is_some_and(|v| !v.is_string()) {
        return Err(MailError::input());
    }
    let max = if a.get("max_body_length").is_some() {
        number(a, "max_body_length", 0, 0, 65536)? as usize
    } else {
        hard_limit
    };
    let mut value = parsed(raw, uid, validity, hard_limit.max(1))?;
    let p = mime::parse(raw)?;
    let full = p.plain_body()?;
    let (trimmed, quoted_omitted, signature_omitted) = if mode == "trimmed" {
        trim_body(&full)
    } else {
        (full.as_str(), false, false)
    };
    let limit = max.min(hard_limit);
    let cut = utf8_prefix(trimmed, limit);
    value["body"] = json!(cut);
    value["body_mode"] = json!(mode);
    value["body_truncated"] = json!(cut.len() < trimmed.len());
    value["body_omitted"] = json!(limit == 0 && !full.is_empty());
    value["quoted_content_omitted"] = json!(quoted_omitted);
    value["signature_omitted"] = json!(signature_omitted);
    value["body_original_bytes"] = json!(full.len());
    value["body_byte_limit"] = json!(limit);
    value["message_id"] = json!(p.headers.get_first_value("Message-ID"));
    value["references"] = json!(p.headers.get_first_value("References"));
    value["in_reply_to"] = json!(p.headers.get_first_value("In-Reply-To"));
    Ok(value)
}
fn utf8_prefix(s: &str, max: usize) -> &str {
    let mut cut = max.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}
fn trim_body(body: &str) -> (&str, bool, bool) {
    let mut quote_offset = body.len();
    let mut signature_offset = body.len();
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('>')
            || trimmed == "-----Original Message-----"
            || trimmed == "-----原始邮件-----"
            || (trimmed.starts_with("On ") && trimmed.ends_with(" wrote:"))
        {
            quote_offset = quote_offset.min(offset);
        }
        if line.trim_end_matches(['\r', '\n']) == "-- " {
            signature_offset = signature_offset.min(offset);
        }
        offset += line.len();
    }
    let end = quote_offset.min(signature_offset);
    let output = if end < body.len() {
        body[..end].trim_end()
    } else {
        body
    };
    (
        output,
        quote_offset < body.len(),
        signature_offset < body.len(),
    )
}
fn bound_conversation_metadata(value: &mut Value) {
    let attachments = value["attachments"].as_array().map_or(0, Vec::len);
    value["attachments"] = json!([]);
    value["attachment_metadata_omitted"] = json!(attachments > 0);
    let mut omitted = Vec::new();
    for key in [
        "subject",
        "from",
        "to",
        "message_id",
        "references",
        "in_reply_to",
    ] {
        if value[key].as_str().is_some_and(|s| s.len() > 1024) {
            value[key] = Value::Null;
            omitted.push(key);
        }
    }
    value["metadata_fields_omitted"] = json!(omitted);
}

#[derive(Clone, Copy, Debug)]
struct CivilDate {
    year: i32,
    month: u32,
    day: u32,
}
impl CivilDate {
    fn parse(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        if b.len() != 10
            || b[4] != b'-'
            || b[7] != b'-'
            || b.iter()
                .enumerate()
                .any(|(i, c)| i != 4 && i != 7 && !c.is_ascii_digit())
        {
            return Err(MailError::input());
        }
        let year = s[..4].parse().map_err(|_| MailError::input())?;
        let month = s[5..7].parse().map_err(|_| MailError::input())?;
        let day = s[8..].parse().map_err(|_| MailError::input())?;
        let date = Self { year, month, day };
        if !(1970..=9999).contains(&year)
            || !(1..=12).contains(&month)
            || day < 1
            || day > date.month_days()
        {
            return Err(MailError::input());
        }
        Ok(date)
    }
    fn month_days(self) -> u32 {
        match self.month {
            4 | 6 | 9 | 11 => 30,
            2 => {
                if self.year % 4 == 0 && (self.year % 100 != 0 || self.year % 400 == 0) {
                    29
                } else {
                    28
                }
            }
            _ => 31,
        }
    }
    fn next(mut self) -> Self {
        self.day += 1;
        if self.day > self.month_days() {
            self.day = 1;
            self.month += 1;
            if self.month > 12 {
                self.month = 1;
                self.year += 1;
            }
        }
        self
    }
    fn ordinal(self) -> i32 {
        let y = self.year - i32::from(self.month <= 2);
        let era = y / 400;
        let year = y - era * 400;
        let month = self.month as i32 + if self.month > 2 { -3 } else { 9 };
        era * 146097 + year * 365 + year / 4 - year / 100 + (153 * month + 2) / 5 + self.day as i32
            - 1
    }
    fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
    fn imap(self) -> String {
        format!(
            "{}-{}-{}",
            self.day,
            [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
            ][(self.month - 1) as usize],
            self.year
        )
    }
}

pub(crate) fn tool_specs() -> Vec<(&'static str, &'static str, Value, Vec<&'static str>)> {
    let uid = json!({"type":"integer","minimum":1,"maximum":4294967295_u64});
    let mailbox = json!({"type":"string","maxLength":512,"default":"INBOX"});
    vec![
        (
            "read_conversation",
            "Read a bounded same-folder conversation linked by message identifiers; inspect scope and omission flags.",
            json!({"mailbox":mailbox,"uid":uid,"uid_validity":uid,"before_uid":uid,"candidate_limit":{"type":"integer","minimum":1,"maximum":200},"limit":{"type":"integer","minimum":1,"maximum":20},"body_mode":{"type":"string","enum":["full","trimmed"]},"max_body_length":{"type":"integer","minimum":0,"maximum":65536}}),
            vec!["uid", "uid_validity"],
        ),
        (
            "locate_message",
            "Locate an exact Message-ID in explicit folders and bounded UID windows; follow per-folder cursors.",
            json!({"message_id":{"type":"string","maxLength":998},"mailboxes":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"string","maxLength":512}},"before_uid":uid,"candidate_limit":{"type":"integer","minimum":1,"maximum":50},"limit":{"type":"integer","minimum":1,"maximum":50}}),
            vec!["message_id", "mailboxes"],
        ),
        (
            "get_mailbox_statistics",
            "Read STATUS counts with optional day distribution over a bounded 500-UID window; date_end is exclusive (maximum 31 days).",
            json!({"mailbox":mailbox,"date_start":{"type":"string","pattern":"^[0-9]{4}-[0-9]{2}-[0-9]{2}$"},"date_end":{"type":"string","pattern":"^[0-9]{4}-[0-9]{2}-[0-9]{2}$"},"before_uid":uid}),
            vec![],
        ),
        (
            "diagnose_connectivity",
            "Check authenticated MCP and IMAP TCP, TLS, greeting, login and capability stages with sanitized results; SMTP is not tested.",
            json!({}),
            vec![],
        ),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn new_reads_require_nonzero_reference_metadata() {
        let mut state = async_imap::types::Mailbox::default();
        assert!(require_reference(&state).is_err());
        state.uid_validity = Some(1);
        assert!(require_reference(&state).is_err());
        state.uid_next = Some(1);
        assert!(require_reference(&state).is_ok());
        state.uid_validity = Some(0);
        assert!(require_reference(&state).is_err());
    }
    #[test]
    fn body_options_keep_shared_html_selection_and_legacy_default_limit() {
        let raw = b"Content-Type: text/html\r\n\r\n<p>Hello &amp; world</p>";
        let value = parsed_with_options(raw, 1, Some(1), 100000, &json!({})).unwrap();
        assert_eq!(
            value["body"],
            parsed(raw, 1, Some(1), 100000).unwrap()["body"]
        );
        let mut long = b"Content-Type: text/plain\r\n\r\n".to_vec();
        long.extend(vec![b'x'; 70000]);
        assert_eq!(
            parsed_with_options(&long, 1, Some(1), 100000, &json!({})).unwrap()["body"]
                .as_str()
                .unwrap()
                .len(),
            70000
        );
        assert!(
            parsed_with_options(
                b"Content-Type: text/html\r\n\r\n<svg>hidden</svg>",
                1,
                None,
                1000,
                &json!({})
            )
            .is_err()
        );
    }

    use super::*;
    #[test]
    fn message_identifiers_are_exact_and_threading_ignores_subject() {
        let h=thread_headers(b"Message-ID: <a@example.org>\r\nReferences: <root@example.org>\r\n In-Reply-To: <not-a-separate-header@example.org>\r\nSubject: same\r\n\r\n").unwrap();
        assert_eq!(h.id.as_deref(), Some("<a@example.org>"));
        assert!(h.identifiers().contains("<root@example.org>"));
        assert!(message_id("<a@example.org>suffix").is_err());
        assert!(message_id("<a\r\n@example.org>").is_err());
        assert_ne!(h.id.as_deref(), Some("<aa@example.org>"));
    }
    #[test]
    fn conversation_resolves_transitive_links_without_subject_guessing() {
        let headers = |raw: &str| thread_headers(raw.as_bytes()).unwrap();
        let seed = headers("Message-ID: <seed@example.org>\r\nSubject: same\r\n\r\n");
        let candidates = vec![
            (
                3,
                headers(
                    "Message-ID: <third@example.org>\r\nIn-Reply-To: <second@example.org>\r\n\r\n",
                ),
            ),
            (
                2,
                headers(
                    "Message-ID: <second@example.org>\r\nReferences: <seed@example.org>\r\n\r\n",
                ),
            ),
            (
                4,
                headers("Message-ID: <other@example.org>\r\nSubject: same\r\n\r\n"),
            ),
            (
                5,
                headers("Message-ID: <seed@example.org.evil>\r\nSubject: same\r\n\r\n"),
            ),
        ];
        let (mut connected, linked) = connected_messages(1, &seed, &candidates, 20);
        connected.sort_unstable();
        assert_eq!(connected, vec![1, 2, 3]);
        assert_eq!(linked, 3);
        let (capped, linked) = connected_messages(1, &seed, &candidates, 2);
        assert_eq!(capped, vec![1, 2]);
        assert_eq!(linked, 3);
    }
    #[test]
    fn conversation_cycles_terminate_without_duplicate_messages() {
        let seed = thread_headers(
            b"Message-ID: <seed@example.org>\r\nReferences: <a@example.org>\r\n\r\n",
        )
        .unwrap();
        let candidates = vec![
            (
                2,
                thread_headers(
                    b"Message-ID: <a@example.org>\r\nIn-Reply-To: <b@example.org>\r\n\r\n",
                )
                .unwrap(),
            ),
            (
                3,
                thread_headers(
                    b"Message-ID: <b@example.org>\r\nIn-Reply-To: <a@example.org>\r\n\r\n",
                )
                .unwrap(),
            ),
        ];
        assert_eq!(
            connected_messages(1, &seed, &candidates, 20),
            (vec![1, 2, 3], 3)
        );
    }
    #[test]
    fn header_cache_expires_invalidates_and_bounds_capacity() {
        let mut cache = MetadataCache::default();
        cache.insert("INBOX", Some(1), 1, b"header".to_vec());
        assert!(cache.get("Other", Some(1), 1, Instant::now()).is_none());
        assert!(cache.get("INBOX", Some(1), 1, Instant::now()).is_some());
        assert!(cache.get("INBOX", Some(2), 1, Instant::now()).is_none());
        assert!(cache.entries.is_empty());
        cache.insert("INBOX", Some(1), 1, b"header".to_vec());
        assert!(
            cache
                .get("INBOX", Some(1), 1, Instant::now() + CACHE_TTL)
                .is_none()
        );
        for uid in 1..=200 {
            cache.insert("INBOX", Some(1), uid, vec![b'x'; HEADER_LIMIT]);
        }
        assert!(cache.entries.len() <= CACHE_ITEMS);
        assert!(cache.entries.iter().map(|e| e.headers.len()).sum::<usize>() <= CACHE_BYTES);
        assert!(cache.get("INBOX", Some(1), 1, Instant::now()).is_none());
        cache.insert("INBOX", None, 201, vec![0; 10]);
        assert!(!cache.entries.iter().any(|e| e.uid == 201));
    }
    #[test]
    fn body_modes_preserve_utf8_and_report_each_omission() {
        let raw="Subject: hi\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n你好\n\nOn Tue wrote:\n> previous".as_bytes();
        let full = parsed_with_options(raw, 1, Some(1), 1000, &json!({})).unwrap();
        assert!(full["body"].as_str().unwrap().contains("previous"));
        let short = parsed_with_options(
            raw,
            1,
            Some(1),
            1000,
            &json!({"body_mode":"trimmed","max_body_length":4}),
        )
        .unwrap();
        assert_eq!(short["body"], "你");
        assert_eq!(short["body_truncated"], true);
        assert_eq!(short["quoted_content_omitted"], true);
        let omitted =
            parsed_with_options(raw, 1, Some(1), 1000, &json!({"max_body_length":0})).unwrap();
        assert_eq!(omitted["body_omitted"], true);
        assert!(parsed_with_options(raw, 1, None, 1000, &json!({"body_mode":"bad"})).is_err());
        let signature=b"Content-Type: text/plain\r\n\r\nAnswer\r\n-- \r\nSignature\r\nOn Tue wrote:\r\n> Quoted";
        let trimmed =
            parsed_with_options(signature, 1, None, 1000, &json!({"body_mode":"trimmed"})).unwrap();
        assert_eq!(trimmed["body"], "Answer");
        assert_eq!(trimmed["signature_omitted"], true);
        assert_eq!(trimmed["quoted_content_omitted"], true);
    }
    #[test]
    fn conversation_metadata_omissions_are_explicit_and_bounded() {
        let mut value = json!({"uid":1,"subject":"x".repeat(2000),"message_id":"<safe@example.org>","attachments":[{"filename":"x".repeat(10000)}]});
        bound_conversation_metadata(&mut value);
        assert_eq!(value["attachment_metadata_omitted"], true);
        assert_eq!(value["metadata_fields_omitted"], json!(["subject"]));
        assert_eq!(value["message_id"], "<safe@example.org>");
        assert!(serde_json::to_vec(&value).unwrap().len() < 500);
    }
    #[test]
    fn date_windows_validate_leap_days_and_month_rollover() {
        assert!(CivilDate::parse("2025-02-29").is_err());
        assert!(CivilDate::parse("2024-02-29").is_ok());
        let day = CivilDate::parse("2024-02-28").unwrap();
        assert_eq!(day.next().next().iso(), "2024-03-01");
        assert_eq!(day.next().next().ordinal() - day.ordinal(), 2);
        assert!(CivilDate::parse("2024-13-01").is_err());
    }
    #[test]
    fn folder_and_uid_scopes_are_explicit_and_bounded() {
        assert!(explicit_folders(&json!({})).is_err());
        assert!(explicit_folders(&json!({"mailboxes":vec!["INBOX";21]})).is_err());
        assert_eq!(
            window(&json!({"before_uid":1001}), Some(2000)).unwrap(),
            (501, 1000)
        );
        assert_eq!(window(&json!({"before_uid":1}), Some(2)).unwrap(), (1, 0));
        // A previous folder consuming the result limit must not skip this folder's first UID.
        assert_eq!(scope_cursor(1, 100, &[99, 98], 0, false), Some(101));
        assert_eq!(scope_cursor(1, 100, &[99, 98], 1, false), Some(99));
        assert_eq!(scope_cursor(1, 100, &[99, 98], 2, false), None);
        assert_eq!(scope_cursor(501, 1000, &[], 0, false), Some(501));
    }
    #[test]
    fn diagnostic_failures_contain_only_sanitized_stage_labels() {
        let mut stages = Vec::new();
        let result = diagnostic_failure(&mut stages, "login");
        assert_eq!(result["stages"][0]["status"], "failed");
        assert_eq!(result["failure"], "connection_check_failed");
        assert!(!result.to_string().contains('@'));
    }
}
