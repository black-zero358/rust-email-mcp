use super::mime::Message;
use crate::server::output_tests::r13_acceptance::ACCOUNTS;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, AtomicU64, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::Notify,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

pub(super) const INBOX_COUNT: u32 = 2_500;
pub(super) const ARCHIVE_COUNT: u32 = 2_532;
pub(super) const DOWNLOADS_COUNT: u32 = 3;
pub(super) const MAILBOXES: [&str; 3] = ["INBOX", "Archive", "Downloads"];

pub(super) fn validity(account: usize, mailbox: usize) -> u32 {
    900 + 100 * account as u32 + mailbox as u32
}

#[derive(Default)]
struct HoldProgress {
    armed: bool,
    entered: u8,
    released: bool,
}

#[derive(Default)]
pub(super) struct Holds {
    progress: Mutex<HoldProgress>,
    changed: Notify,
}

impl Holds {
    pub fn arm(&self) -> bool {
        let mut state = self.progress.lock().unwrap();
        if state.armed && !state.released {
            return false;
        }
        *state = HoldProgress {
            armed: true,
            ..HoldProgress::default()
        };
        true
    }

    pub fn release(&self) {
        self.progress.lock().unwrap().released = true;
        self.changed.notify_waiters();
    }

    pub fn sample(&self) -> Value {
        let state = self.progress.lock().unwrap();
        json!({"armed": state.armed, "entered": state.entered.count_ones(), "released": state.released})
    }

    fn enter(&self, account: usize, command: &str) -> bool {
        let Some(uid) = command
            .strip_prefix("UID FETCH ")
            .and_then(|rest| rest.split_once(" (UID BODY.PEEK[2]<0."))
            .and_then(|(uid, _)| uid.parse::<u32>().ok())
            .filter(|uid| (1..=2).contains(uid))
        else {
            return false;
        };
        let bit = 1 << (account * 2 + uid as usize - 1);
        let mut state = self.progress.lock().unwrap();
        if !state.armed || state.released || state.entered & bit != 0 {
            return false;
        }
        state.entered |= bit;
        true
    }

    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.progress.lock().unwrap().released {
                return;
            }
            changed.await;
        }
    }
}

#[derive(Default)]
struct Metrics {
    connections: AtomicU64,
    commands: AtomicU64,
    completions: AtomicU64,
    received_bytes: AtomicU64,
    sent_bytes: AtomicU64,
    active: AtomicU64,
    max_active: AtomicU64,
    unsupported: AtomicU64,
    qresync_enables: AtomicU64,
    qresync_fetches: AtomicU64,
    condstore_fetches: AtomicU64,
}

pub(super) struct Imap {
    account: usize,
    added: AtomicU32,
    holds: Arc<Holds>,
    metrics: Metrics,
}

impl Imap {
    pub fn start(
        account: usize,
        listener: TcpListener,
        tls: TlsAcceptor,
        holds: Arc<Holds>,
    ) -> Arc<Self> {
        let imap = Arc::new(Self {
            account,
            added: AtomicU32::new(0),
            holds,
            metrics: Metrics::default(),
        });
        let protocol = imap.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let _ = socket.set_nodelay(true);
                let tls = tls.clone();
                let protocol = protocol.clone();
                protocol.metrics.connections.fetch_add(1, Ordering::Relaxed);
                let active = protocol.metrics.active.fetch_add(1, Ordering::Relaxed) + 1;
                protocol
                    .metrics
                    .max_active
                    .fetch_max(active, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _guard = Active(&protocol.metrics.active);
                    if let Ok(stream) = tls.accept(socket).await {
                        let _ = protocol.serve(stream).await;
                    }
                });
            }
        });
        imap
    }

    pub fn sample(&self) -> Value {
        let m = &self.metrics;
        let load = |count: &AtomicU64| count.load(Ordering::Relaxed);
        json!({"connections":load(&m.connections), "commands":load(&m.commands),
            "completions":load(&m.completions), "received_bytes":load(&m.received_bytes),
            "sent_bytes":load(&m.sent_bytes), "active":load(&m.active), "max_active":load(&m.max_active),
            "unsupported":load(&m.unsupported), "qresync_enables":load(&m.qresync_enables),
            "qresync_fetches":load(&m.qresync_fetches), "condstore_fetches":load(&m.condstore_fetches)})
    }

    pub fn append(&self) -> Value {
        let uid = INBOX_COUNT + self.added.fetch_add(1, Ordering::Relaxed) + 1;
        json!({"account_id":ACCOUNTS[self.account], "mailbox":"INBOX", "uid":uid, "uid_validity":validity(self.account, 0)})
    }

    fn high(&self, mailbox: usize) -> u32 {
        match mailbox {
            0 => INBOX_COUNT + self.added.load(Ordering::Relaxed),
            1 => ARCHIVE_COUNT,
            _ => DOWNLOADS_COUNT,
        }
    }

    async fn serve(&self, stream: TlsStream<tokio::net::TcpStream>) -> std::io::Result<()> {
        let mut io = BufReader::new(stream);
        self.send(&mut io, b"* OK isolated R13 IMAP\r\n").await?;
        let mut selected = None;
        let mut enabled = false;
        let mut line = String::new();
        loop {
            line.clear();
            let count = io.read_line(&mut line).await?;
            if count == 0 || count > 32 * 1024 {
                return Ok(());
            }
            self.metrics
                .received_bytes
                .fetch_add(count as u64, Ordering::Relaxed);
            let Some((tag, command)) = line.trim_end_matches("\r\n").split_once(' ') else {
                return Ok(());
            };
            self.metrics.commands.fetch_add(1, Ordering::Relaxed);
            if selected == Some(2) && self.holds.enter(self.account, command) {
                tokio::select! {
                    _ = io.read_u8() => return Ok(()),
                    () = self.holds.wait() => {},
                }
            }
            let mut reply = Vec::new();
            let status = self
                .respond(command, &mut selected, &mut enabled, &mut reply)
                .unwrap_or_else(|| {
                    self.metrics.unsupported.fetch_add(1, Ordering::Relaxed);
                    reply.clear();
                    "BAD"
                });
            reply.extend_from_slice(format!("{tag} {status} synthetic\r\n").as_bytes());
            self.send(&mut io, &reply).await?;
            self.metrics.completions.fetch_add(1, Ordering::Relaxed);
            if status.starts_with("OK") {
                if command == "ENABLE QRESYNC" {
                    self.metrics.qresync_enables.fetch_add(1, Ordering::Relaxed);
                } else if command.ends_with(" VANISHED)") {
                    self.metrics.qresync_fetches.fetch_add(1, Ordering::Relaxed);
                } else if command.contains(" (CHANGEDSINCE ") {
                    self.metrics
                        .condstore_fetches
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            if command == "LOGOUT" {
                return Ok(());
            }
        }
    }

    async fn send(
        &self,
        io: &mut BufReader<TlsStream<tokio::net::TcpStream>>,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let count = io.write(rest).await?;
            if count == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            self.metrics
                .sent_bytes
                .fetch_add(count as u64, Ordering::Relaxed);
            rest = &rest[count..];
        }
        io.flush().await
    }

    fn respond(
        &self,
        command: &str,
        selected: &mut Option<usize>,
        enabled: &mut bool,
        reply: &mut Vec<u8>,
    ) -> Option<&'static str> {
        if command.starts_with("LOGIN ") || command == "NOOP" {
            return Some("OK");
        }
        if command == "CAPABILITY" {
            reply.extend_from_slice(
                b"* CAPABILITY IMAP4rev1 UIDPLUS SPECIAL-USE CONDSTORE ENABLE QRESYNC\r\n",
            );
            return Some("OK");
        }
        if command == "ENABLE QRESYNC" && selected.is_none() {
            *enabled = true;
            reply.extend_from_slice(b"* ENABLED QRESYNC\r\n");
            return Some("OK");
        }
        if command == "LOGOUT" {
            reply.extend_from_slice(b"* BYE isolated logout\r\n");
            return Some("OK");
        }
        if command.starts_with("LIST ") {
            for (mailbox, flags) in MAILBOXES.into_iter().zip(["", "\\Archive", ""]) {
                reply.extend_from_slice(
                    format!("* LIST ({flags}) \"/\" \"{mailbox}\"\r\n").as_bytes(),
                );
            }
            return Some("OK");
        }
        if let Some(select) = command.strip_prefix("EXAMINE ") {
            let (wire_name, extension) = select.split_once(' ').unwrap_or((select, ""));
            let mailbox = mailbox(wire_name)?;
            let high = self.high(mailbox);
            *selected = Some(mailbox);
            reply.extend_from_slice(format!("* {high} EXISTS\r\n* OK [UIDVALIDITY {}] generation\r\n* OK [UIDNEXT {}] next\r\n", validity(self.account, mailbox), high + 1).as_bytes());
            if extension == "(CONDSTORE)" || *enabled && extension.starts_with("(QRESYNC (") {
                reply.extend_from_slice(
                    format!("* OK [HIGHESTMODSEQ {}] current\r\n", modseq(high, mailbox))
                        .as_bytes(),
                );
            } else if !extension.is_empty() {
                return None;
            }
            return Some("OK [READ-ONLY]");
        }
        if let Some(status) = command.strip_prefix("STATUS ") {
            let (name, _) = status.split_once(' ')?;
            let mailbox = mailbox(name)?;
            let high = self.high(mailbox);
            reply.extend_from_slice(format!("* STATUS {name} (MESSAGES {high} UNSEEN 0 RECENT 0 UIDNEXT {} UIDVALIDITY {})\r\n", high + 1, validity(self.account, mailbox)).as_bytes());
            return Some("OK");
        }
        let mailbox = (*selected)?;
        let high = self.high(mailbox);
        if let Some(search) = command.strip_prefix("UID SEARCH ") {
            self.search(mailbox, high, search, reply)?;
            return Some("OK");
        }
        let (set, tail) = command.strip_prefix("UID FETCH ")?.split_once(' ')?;
        let (attrs, changed_since) =
            if let Some((attrs, changes)) = tail.split_once(") (CHANGEDSINCE ") {
                let since = changes
                    .strip_suffix(')')?
                    .split(' ')
                    .next()?
                    .parse::<u64>()
                    .ok()?;
                (attrs, Some(since))
            } else {
                (tail, None)
            };
        for uid in uid_set(set, high)? {
            if changed_since.is_some_and(|base| base >= modseq(uid, mailbox)) {
                continue;
            }
            self.fetch(uid, mailbox, attrs, reply)?;
        }
        if changed_since.is_some() {
            reply.extend_from_slice(
                format!("* OK [HIGHESTMODSEQ {}] current\r\n", modseq(high, mailbox)).as_bytes(),
            );
        }
        Some("OK")
    }

    fn search(&self, mailbox: usize, high: u32, query: &str, reply: &mut Vec<u8>) -> Option<()> {
        let query = query.strip_prefix("CHARSET UTF-8 ").unwrap_or(query);
        let query = query.strip_prefix("UID ")?;
        let (set, filter) = query.split_once(' ').unwrap_or((query, ""));
        let mut found = String::from("* SEARCH");
        for uid in uid_set(set, high)? {
            if matches!(filter, "" | "ALL" | "SEEN")
                || search_matches(self.account, mailbox, uid, filter)
            {
                use std::fmt::Write;
                write!(&mut found, " {uid}").ok()?;
            }
        }
        reply.extend_from_slice(found.as_bytes());
        reply.extend_from_slice(b"\r\n");
        Some(())
    }

    fn fetch(&self, uid: u32, mailbox: usize, attrs: &str, reply: &mut Vec<u8>) -> Option<()> {
        let has_size = attrs.contains(" RFC822.SIZE");
        let has_structure = attrs.contains(" BODYSTRUCTURE");
        let message = (has_size || has_structure || attrs.contains("BODY.PEEK["))
            .then(|| Message::new(self.account, MAILBOXES[mailbox], uid));
        let mut prefix = format!("* {uid} FETCH (UID {uid}");
        use std::fmt::Write;
        if attrs.contains(" FLAGS") {
            prefix.push_str(" FLAGS (\\Seen)");
        }
        if attrs.contains(" INTERNALDATE") {
            prefix.push_str(" INTERNALDATE \" 7-Feb-2025 09:00:00 +0000\"");
        }
        if has_size {
            write!(&mut prefix, " RFC822.SIZE {}", message.as_ref()?.size()).ok()?;
        }
        if has_structure {
            write!(
                &mut prefix,
                " BODYSTRUCTURE {}",
                message.as_ref()?.structure()
            )
            .ok()?;
        }
        if attrs.contains(" MODSEQ") {
            write!(&mut prefix, " MODSEQ ({})", modseq(uid, mailbox)).ok()?;
        }
        reply.extend_from_slice(prefix.as_bytes());
        for selector in attrs.split("BODY.PEEK[").skip(1) {
            let (section, rest) = selector.split_once(']')?;
            let (offset, count, origin) = if let Some(range) = rest.strip_prefix('<') {
                let (range, _) = range.split_once('>')?;
                let (offset, count) = range.split_once('.')?;
                (
                    offset.parse::<usize>().ok()?,
                    count.parse::<usize>().ok()?,
                    format!("<{offset}>"),
                )
            } else {
                (0, usize::MAX, String::new())
            };
            let literal = message.as_ref()?.section(section, offset, count)?;
            reply.extend_from_slice(
                format!(" BODY[{section}]{origin} {{{}}}\r\n", literal.len()).as_bytes(),
            );
            reply.extend_from_slice(&literal);
        }
        reply.extend_from_slice(b")\r\n");
        Some(())
    }
}

struct Active<'a>(&'a AtomicU64);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn mailbox(wire: &str) -> Option<usize> {
    MAILBOXES
        .iter()
        .position(|name| *name == wire.trim_matches('"'))
}

fn modseq(uid: u32, mailbox: usize) -> u64 {
    100 + if mailbox == 0 {
        u64::from(uid.saturating_sub(INBOX_COUNT))
    } else {
        0
    }
}

fn uid_set(wire: &str, high: u32) -> Option<Vec<u32>> {
    let mut uids = Vec::new();
    let number = |number: &str| {
        if number == "*" {
            Some(high)
        } else {
            number.parse::<u32>().ok()
        }
    };
    for part in wire.split(',') {
        if let Some((a, b)) = part.split_once(':') {
            let (a, b) = (number(a)?, number(b)?);
            uids.extend(a.min(b).max(1)..=a.max(b).min(high));
        } else {
            let uid = number(part)?;
            if uid > 0 && uid <= high {
                uids.push(uid);
            }
        }
    }
    uids.sort_unstable();
    uids.dedup();
    Some(uids)
}

fn search_matches(account: usize, mailbox: usize, uid: u32, filter: &str) -> bool {
    let (field, needle) = filter
        .strip_prefix("HEADER ")
        .unwrap_or(filter)
        .split_once(' ')
        .unwrap_or(("", ""));
    let Some(needle) = needle
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return false;
    };
    let message = Message::new(account, MAILBOXES[mailbox], uid);
    let header = String::from_utf8_lossy(&message.header);
    let needle = needle.to_lowercase();
    if field == "TEXT" {
        return header.to_lowercase().contains(&needle)
            || "合成邮件 缓存搜索 中文关键字 synthetic workload".contains(&needle);
    }
    header.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case(field) && value.to_lowercase().contains(&needle)
        })
    })
}
