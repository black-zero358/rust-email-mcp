//! Multi-folder, mutable local TLS IMAP fixture for the production MCP handler.
use super::super::{attachments::Credentials, attachments::router, *};
use crate::config::{AccountConfigSource, Config, ValidatedAccountConfig};
use mailparse::MailHeaderMap;
use std::{
    borrow::Cow,
    ops::RangeInclusive,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

type GenerateRaw = fn(u32) -> Vec<u8>;
type OverrideReply = dyn Fn(&str, &str) -> Option<(Vec<u8>, &'static str)> + Send + Sync;

pub(in crate::server::output_tests) struct Folder {
    pub flags: &'static str,
    pub validity: u32,
    pub next: Option<u32>,
    pub report_uid_next: bool,
    pub messages: BTreeMap<u32, Vec<u8>>,
    pub message_flags: BTreeMap<u32, &'static str>,
    pub sizes: BTreeMap<u32, u32>,
    pub structures: BTreeMap<u32, String>,
    pub sections: BTreeMap<(u32, String), Vec<u8>>,
    generated: Option<(RangeInclusive<u32>, GenerateRaw)>,
}

impl Folder {
    pub fn new<const N: usize>(
        flags: &'static str,
        validity: u32,
        messages: [(u32, Vec<u8>); N],
    ) -> Self {
        Self {
            flags,
            validity,
            next: None,
            report_uid_next: true,
            messages: messages.into(),
            message_flags: BTreeMap::new(),
            sizes: BTreeMap::new(),
            structures: BTreeMap::new(),
            sections: BTreeMap::new(),
            generated: None,
        }
    }

    pub fn generate(&mut self, uids: RangeInclusive<u32>, raw: GenerateRaw) {
        assert!(!uids.is_empty() && *uids.start() > 0 && *uids.end() < u32::MAX);
        self.generated = Some((uids, raw));
    }

    fn raw(&self, uid: u32) -> Option<Cow<'_, [u8]>> {
        if let Some(bytes) = self.messages.get(&uid) {
            return Some(Cow::Borrowed(bytes));
        }
        let (range, raw) = self.generated.as_ref()?;
        range.contains(&uid).then(|| Cow::Owned(raw(uid)))
    }

    fn uids(&self, range: RangeInclusive<u32>) -> impl Iterator<Item = u32> + '_ {
        let generated = self.generated.as_ref().and_then(|(generated, _)| {
            let start = (*generated.start()).max(*range.start());
            let end = (*generated.end()).min(*range.end());
            (start <= end).then_some(start..=end)
        });
        self.messages.range(range).map(|(uid, _)| *uid).chain(
            generated
                .into_iter()
                .flatten()
                .filter(|uid| !self.messages.contains_key(uid)),
        )
    }

    fn exists(&self) -> usize {
        let generated = self.generated.as_ref().map_or(0, |(range, _)| {
            (*range.end() - *range.start() + 1) as usize
                - self.messages.range(range.clone()).count()
        });
        self.messages.len() + generated
    }

    fn high_water(&self) -> u32 {
        self.next.unwrap_or_else(|| {
            let last_generated = self.generated.as_ref().map_or(0, |(range, _)| *range.end());
            self.messages
                .keys()
                .next_back()
                .copied()
                .unwrap_or(0)
                .max(last_generated)
                + 1
        })
    }
}

pub(in crate::server::output_tests) fn message(
    id: Option<&str>,
    refs: &[&str],
    reply: Option<&str>,
    subject: &str,
    body: &str,
) -> Vec<u8> {
    let mut raw =
        format!("Subject: {subject}\r\nFrom: sender@example.test\r\nTo: owner@example.test\r\n");
    if let Some(id) = id {
        raw.push_str(&format!("Message-ID: <{id}>\r\n"));
    }
    if !refs.is_empty() {
        let list = refs
            .iter()
            .map(|id| format!("<{id}>"))
            .collect::<Vec<_>>()
            .join(" ");
        raw.push_str(&format!("References: {list}\r\n"));
    }
    if let Some(id) = reply {
        raw.push_str(&format!("In-Reply-To: <{id}>\r\n"));
    }
    raw.push_str(
        "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n",
    );
    raw.push_str(body);
    raw.into_bytes()
}

#[derive(Clone, Copy)]
pub(in crate::server::output_tests) enum Fault {
    NoAfterData,
    NoDataOk,
    OnlyUid(u32),
    Hang,
}

#[derive(Clone, Debug)]
pub(in crate::server::output_tests) struct Command {
    pub connection: usize,
    pub mailbox: String,
    pub text: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::server::output_tests) struct Search<'a> {
    pub start: u32,
    pub end: u32,
    pub field: Option<&'a str>,
    pub anchor: String,
}

impl<'a> Search<'a> {
    pub fn parse(command: &'a str) -> Option<Self> {
        let request = command.strip_prefix("UID SEARCH UID ")?;
        let (range, header) = request.split_once(' ').unwrap_or((request, ""));
        let (start, end) = range.split_once(':').unwrap_or((range, range));
        let (field, anchor) = if header.is_empty() {
            (None, String::new())
        } else {
            let (field, quoted) = header.strip_prefix("HEADER ")?.split_once(' ')?;
            (Some(field), unquote(quoted)?)
        };
        Some(Self {
            start: start.parse().ok()?,
            end: end.parse().ok()?,
            field,
            anchor,
        })
    }
}

fn unquote(wire: &str) -> Option<String> {
    let mut rest = wire.strip_prefix('"')?.strip_suffix('"')?.chars();
    let mut output = String::new();
    while let Some(c) = rest.next() {
        output.push(if c == '\\' { rest.next()? } else { c });
    }
    Some(output)
}

struct Imap {
    folders: Mutex<BTreeMap<String, Folder>>,
    faults: Mutex<BTreeMap<(String, String), Fault>>,
    holds: Mutex<BTreeMap<(String, String), Arc<CommandHold>>>,
    override_reply: Mutex<Option<Arc<OverrideReply>>>,
    commands: Mutex<Vec<Command>>,
    connections: AtomicUsize,
    commands_total: AtomicUsize,
    completions: AtomicUsize,
    received_bytes: AtomicUsize,
    sent_bytes: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    changed: tokio::sync::Notify,
}

#[derive(Debug)]
pub(in crate::server::output_tests) struct ImapMetrics {
    pub commands: usize,
    pub completions: usize,
    pub received_bytes: usize,
    pub sent_bytes: usize,
    pub active: usize,
    pub max_active: usize,
}

struct Metered<S> {
    inner: S,
    protocol: Arc<Imap>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Metered<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.protocol
                .received_bytes
                .fetch_add(buf.filled().len() - before, Ordering::Relaxed);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Metered<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(bytes)) = result {
            self.protocol.sent_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct ActiveConnection(Arc<Imap>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(in crate::server::output_tests) struct CommandHold {
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    closed: tokio::sync::Semaphore,
}

impl CommandHold {
    pub fn is_waiting(&self) -> bool {
        self.entered.available_permits() != 0
    }

    pub async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.acquire())
            .await
            .expect("fixture command did not reach its hold")
            .unwrap()
            .forget();
    }

    pub fn release(&self) {
        self.release.add_permits(1);
    }

    pub async fn disconnected(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.closed.acquire())
            .await
            .expect("fixture did not observe the held connection close")
            .unwrap()
            .forget();
    }
}

pub(in crate::server::output_tests) struct Harness {
    imap: Arc<Imap>,
    backend: tokio::task::JoinHandle<()>,
    pub catalog: Arc<AccountCatalog>,
    pub runtime: Arc<MailRuntime>,
    settings: ValidatedAccountSet,
    roots: RootCertStore,
    credentials: Credentials,
    app: Router,
    _staging: tempfile::TempDir,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.backend.abort();
    }
}

impl Harness {
    pub async fn new(
        folders: impl IntoIterator<Item = (&'static str, Folder)>,
        max_output: usize,
    ) -> Self {
        Self::configured(folders, max_output, |_| {}).await
    }

    pub async fn configured(
        folders: impl IntoIterator<Item = (&'static str, Folder)>,
        max_output: usize,
        configure: impl FnOnce(&mut Config),
    ) -> Self {
        Self::build(folders, max_output, configure, None).await
    }

    pub async fn with_second_account(
        folders: impl IntoIterator<Item = (&'static str, Folder)>,
        max_output: usize,
        second: &Self,
    ) -> Self {
        Self::build(folders, max_output, |_| {}, Some(second)).await
    }

    async fn build(
        folders: impl IntoIterator<Item = (&'static str, Folder)>,
        max_output: usize,
        configure: impl FnOnce(&mut Config),
        second: Option<&Self>,
    ) -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = certified.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        let tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let staging = tempfile::tempdir().unwrap();
        let mut cfg = (*config::fixture()).clone();
        cfg.imap_host = "localhost".into();
        cfg.imap_port = listener.local_addr().unwrap().port();
        cfg.attachment_staging_dir = Some(staging.path().join("mail-downloads"));
        cfg.max_output = max_output;
        cfg.timeout = Duration::from_secs(8);
        cfg.subjects = ["alice", "bob"].map(str::to_owned).into();
        configure(&mut cfg);
        let cfg = Arc::new(cfg);
        let mut accounts = ValidatedAccountSet::legacy(cfg.clone());
        if let Some(second) = second {
            let mut secondary = (**second.runtime.config()).clone();
            secondary.mode = "read".into();
            secondary.address = "second@example.test".into();
            accounts.source = AccountConfigSource::ExtendLegacy;
            accounts.accounts.push(ValidatedAccountConfig {
                registration_key: "thread-second".into(),
                alias: "second".into(),
                display_name: "Second mailbox".into(),
                config: Arc::new(secondary),
                read_subjects: vec!["alice".into()],
                write_subjects: vec![],
                is_legacy: false,
            });
            roots.roots.extend(second.roots.roots.iter().cloned());
        }
        let catalog = AccountCatalog::from_test_accounts(accounts.clone()).unwrap();
        let journal = Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap());
        let runtime = Arc::new(
            MailRuntime::new(catalog.clone(), journal)
                .unwrap()
                .with_test_imap_roots(roots.clone()),
        );
        let credentials = Credentials::new(cfg);
        let app = router(runtime.clone(), credentials.auth.clone());
        let imap = Arc::new(Imap {
            folders: Mutex::new(
                folders
                    .into_iter()
                    .map(|(name, folder)| (name.into(), folder))
                    .collect(),
            ),
            faults: Mutex::default(),
            holds: Mutex::default(),
            override_reply: Mutex::default(),
            commands: Mutex::default(),
            connections: AtomicUsize::new(0),
            commands_total: AtomicUsize::new(0),
            completions: AtomicUsize::new(0),
            received_bytes: AtomicUsize::new(0),
            sent_bytes: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        });
        let protocol = imap.clone();
        let backend = tokio::spawn(async move {
            let tls = TlsAcceptor::from(Arc::new(tls));
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                socket.set_nodelay(true).unwrap();
                let id = protocol.connections.fetch_add(1, Ordering::SeqCst) + 1;
                let active = protocol.active.fetch_add(1, Ordering::SeqCst) + 1;
                protocol.max_active.fetch_max(active, Ordering::SeqCst);
                let connection = ActiveConnection(protocol.clone());
                let tls = tls.clone();
                let protocol = protocol.clone();
                tokio::spawn(async move {
                    let _connection = connection;
                    if let Ok(stream) = tls.accept(socket).await {
                        let stream = Metered {
                            inner: stream,
                            protocol: protocol.clone(),
                        };
                        serve(protocol, id, stream).await;
                    }
                });
            }
        });
        Self {
            imap,
            backend,
            catalog,
            runtime,
            settings: accounts,
            roots,
            credentials,
            app,
            _staging: staging,
        }
    }

    pub fn connections(&self) -> usize {
        self.imap.connections.load(Ordering::SeqCst)
    }

    pub fn metrics(&self) -> ImapMetrics {
        ImapMetrics {
            commands: self.imap.commands_total.load(Ordering::Relaxed),
            completions: self.imap.completions.load(Ordering::Relaxed),
            received_bytes: self.imap.received_bytes.load(Ordering::Relaxed),
            sent_bytes: self.imap.sent_bytes.load(Ordering::Relaxed),
            active: self.imap.active.load(Ordering::SeqCst),
            max_active: self.imap.max_active.load(Ordering::SeqCst),
        }
    }

    pub fn stop_imap(&self) {
        self.backend.abort();
    }

    pub fn commands(&self) -> Vec<Command> {
        self.imap.commands.lock().unwrap().clone()
    }

    pub fn override_replies(
        &self,
        reply: impl Fn(&str, &str) -> Option<(Vec<u8>, &'static str)> + Send + Sync + 'static,
    ) {
        *self.imap.override_reply.lock().unwrap() = Some(Arc::new(reply));
    }

    pub fn update(&self, mailbox: &str, apply: impl FnOnce(&mut Folder)) {
        apply(self.imap.folders.lock().unwrap().get_mut(mailbox).unwrap());
    }

    pub fn fault(&self, mailbox: &str, command: &str, fault: Fault) {
        self.imap
            .faults
            .lock()
            .unwrap()
            .insert((mailbox.into(), command.into()), fault);
    }

    pub fn hold(&self, mailbox: &str, command: &str) -> Arc<CommandHold> {
        let hold = Arc::new(CommandHold {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            closed: tokio::sync::Semaphore::new(0),
        });
        let previous = self
            .imap
            .holds
            .lock()
            .unwrap()
            .insert((mailbox.into(), command.into()), hold.clone());
        assert!(previous.is_none(), "overlapping fixture command hold");
        hold
    }

    pub async fn wait_for(&self, matches: impl Fn(&Command) -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let changed = self.imap.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.imap.commands.lock().unwrap().iter().any(&matches) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fixture command did not arrive: {:?}", self.commands()));
    }

    pub async fn call(&self, principal: &str, args: Value) -> Value {
        self.call_id(principal, args, json!("conversation-中\\\""))
            .await
    }

    pub async fn call_id(&self, principal: &str, args: Value, id: Value) -> Value {
        self.call_on(&self.app, principal, args, id).await.0
    }

    pub async fn measure(&self, principal: &str, args: Value, id: Value) -> (Value, usize) {
        self.call_on(&self.app, principal, args, id).await
    }

    pub async fn call_restarted(&self, principal: &str, args: Value) -> Value {
        let cfg = self.runtime.config().clone();
        let catalog = AccountCatalog::from_test_accounts(ValidatedAccountSet::legacy(cfg)).unwrap();
        let journal = Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap());
        let runtime = Arc::new(
            MailRuntime::new(catalog, journal)
                .unwrap()
                .with_test_imap_roots(self.roots.clone()),
        );
        let app = router(runtime, self.credentials.auth.clone());
        self.call_on(&app, principal, args, json!(1)).await.0
    }

    pub fn cache_router(&self, path: &std::path::Path) -> (Router, crate::mail::PreparedCache) {
        let settings = config::load_cache_file(path, &self.settings).unwrap();
        let cache = MailRuntime::prepare_cache(&self.catalog, settings).unwrap();
        let journal = Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap());
        let runtime = Arc::new(
            MailRuntime::with_cache(self.catalog.clone(), journal, cache.clone())
                .unwrap()
                .with_test_imap_roots(self.roots.clone()),
        );
        (router(runtime, self.credentials.auth.clone()), cache)
    }

    pub fn full_cache_router(
        &self,
        path: &std::path::Path,
    ) -> (
        Router,
        Arc<MailRuntime>,
        crate::mail::PreparedCache,
        crate::accounts::CacheRun,
        Arc<AccountCatalog>,
    ) {
        let mut accounts = self.settings.clone();
        let mut service = (*accounts.service_config).clone();
        service.account_registry = Some(path.with_file_name("accounts.sqlite"));
        service.attachment_staging_dir = Some(path.with_file_name("full-downloads"));
        accounts.service_config = Arc::new(service);
        let settings = config::load_cache_file(path, &accounts).unwrap();
        let catalog = crate::accounts::open_catalog(accounts).unwrap();
        let witness = crate::accounts::CacheRun::begin(catalog.config()).unwrap();
        let cache =
            MailRuntime::prepare_cache_for_run(&catalog, settings, Some(witness.identity()))
                .unwrap();
        let journal = Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap());
        let runtime = Arc::new(
            MailRuntime::with_cache(catalog.clone(), journal, cache.clone())
                .unwrap()
                .with_test_imap_roots(self.roots.clone()),
        );
        let app = router(runtime.clone(), self.credentials.auth.clone());
        (app, runtime, cache, witness, catalog)
    }

    pub async fn call_tool(
        &self,
        app: Option<&Router>,
        principal: &str,
        name: &str,
        args: Value,
    ) -> Value {
        self.call_named(app.unwrap_or(&self.app), principal, name, args, json!(1))
            .await
            .0
    }

    pub async fn wait_for_cache_listing(&self, app: &Router, args: Value) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let result = self
                .call_tool(Some(app), "alice", "list_emails", args.clone())
                .await;
            // Background work can change the scope revision during an offline read.
            if result["error"]["code"] != "CACHE_NEEDS_VERIFICATION" {
                return result;
            }
            assert!(tokio::time::Instant::now() < deadline, "{result}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn attachment(
        &self,
        app: &Router,
        principal: &str,
        url: &str,
    ) -> axum::response::Response {
        let token = self.credentials.changed("sub", json!(principal));
        super::super::attachments::get_attachment(app, Some(&token), url).await
    }

    async fn call_on(
        &self,
        app: &Router,
        principal: &str,
        args: Value,
        id: Value,
    ) -> (Value, usize) {
        self.call_named(app, principal, "read_conversation", args, id)
            .await
    }

    pub async fn call_named(
        &self,
        app: &Router,
        principal: &str,
        name: &str,
        args: Value,
        id: Value,
    ) -> (Value, usize) {
        let token = self.credentials.changed("sub", json!(principal));
        let response = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header("Host", "localhost")
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .header("MCP-Protocol-Version", "2025-06-18")
                    .body(Body::from(
                        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":name,"arguments":args}})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let trace = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let wire = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        assert!(
            wire.len() <= self.runtime.config().max_output,
            "wire bytes={}",
            wire.len()
        );
        let length = wire.len();
        let wire: Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(wire["id"], id);
        let result = &wire["result"];
        let content = result["structuredContent"].clone();
        assert_eq!(content["request_id"], trace, "{wire}");
        if is_read(name) {
            assert_contract(name, &content);
        }
        let copy: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(copy, content);
        (content, length)
    }
}

async fn serve(
    protocol: Arc<Imap>,
    connection: usize,
    stream: impl AsyncReadExt + AsyncWriteExt + Unpin,
) {
    let mut stream = BufReader::new(stream);
    if stream
        .write_all(b"* OK isolated conversation fixture\r\n")
        .await
        .is_err()
        || stream.flush().await.is_err()
    {
        return;
    }
    let mut selected = String::new();
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
        protocol.commands_total.fetch_add(1, Ordering::Relaxed);
        if command.starts_with("LOGIN ") {
            if stream
                .write_all(format!("{tag} OK login\r\n").as_bytes())
                .await
                .is_err()
                || stream.flush().await.is_err()
            {
                return;
            }
            protocol.completions.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if let Some(mailbox) = command
            .strip_prefix("EXAMINE ")
            .or_else(|| command.strip_prefix("SELECT "))
        {
            let mailbox = mailbox.strip_suffix(" (CONDSTORE)").unwrap_or(mailbox);
            let mailbox = mailbox
                .rsplit_once(" (QRESYNC (")
                .map_or(mailbox, |(name, _)| name);
            selected = unquote(mailbox).unwrap_or_else(|| mailbox.to_owned());
        }
        {
            // Preserve existing small-scenario traces without retaining the
            // entire command stream during long synthetic load runs.
            let mut commands = protocol.commands.lock().unwrap();
            if commands.len() < 65_536 {
                commands.push(Command {
                    connection,
                    mailbox: selected.clone(),
                    text: command.into(),
                });
            }
        }
        protocol.changed.notify_waiters();
        let hold = protocol
            .holds
            .lock()
            .unwrap()
            .remove(&(selected.clone(), command.into()));
        if let Some(hold) = hold {
            hold.entered.add_permits(1);
            tokio::select! {
                result = stream.read_u8() => {
                    assert!(result.is_err(), "another command arrived while a reply was held");
                    hold.closed.add_permits(1);
                    return;
                }
                released = hold.release.acquire() => released.unwrap().forget(),
            }
        }
        let fault = protocol
            .faults
            .lock()
            .unwrap()
            .remove(&(selected.clone(), command.into()));
        if matches!(fault, Some(Fault::Hang)) {
            let _ = stream.read_u8().await;
            return;
        }
        let overridden = protocol
            .override_reply
            .lock()
            .unwrap()
            .clone()
            .and_then(|reply| reply(&selected, command));
        let (mut reply, status) = if let Some(overridden) = overridden {
            overridden
        } else {
            let reply = {
                let folders = protocol.folders.lock().unwrap();
                respond(&folders, &selected, command)
            };
            match (reply, fault) {
                (_, Some(Fault::NoDataOk)) => (Vec::new(), "OK"),
                (_, Some(Fault::OnlyUid(uid))) => (
                    format!("* 1 FETCH (UID {uid} FLAGS ())\r\n").into_bytes(),
                    "OK",
                ),
                (Some(reply), Some(Fault::NoAfterData)) => (reply, "NO"),
                (Some(reply), _) => (reply, "OK"),
                (None, _) => (Vec::new(), "NO"),
            }
        };
        reply.extend_from_slice(format!("{tag} {status} synthetic\r\n").as_bytes());
        // TLS can accept plaintext while encrypted bytes are still buffered.
        if stream.write_all(&reply).await.is_err() || stream.flush().await.is_err() {
            return;
        }
        protocol.completions.fetch_add(1, Ordering::Relaxed);
    }
}

fn respond(folders: &BTreeMap<String, Folder>, selected: &str, command: &str) -> Option<Vec<u8>> {
    if command.starts_with("LIST ") {
        return Some(
            folders
                .iter()
                .map(|(name, folder)| format!("* LIST ({}) \"/\" \"{name}\"\r\n", folder.flags))
                .collect::<String>()
                .into_bytes(),
        );
    }
    if command == "CAPABILITY" {
        return Some(b"* CAPABILITY IMAP4rev1 UIDPLUS SPECIAL-USE\r\n".to_vec());
    }
    let folder = folders.get(selected)?;
    if command.starts_with("EXAMINE ") || command.starts_with("SELECT ") {
        let next = if folder.report_uid_next {
            format!("* OK [UIDNEXT {}] next\r\n", folder.high_water())
        } else {
            String::new()
        };
        return Some(
            format!(
                "* {} EXISTS\r\n* OK [UIDVALIDITY {}] valid\r\n{next}",
                folder.exists(),
                folder.validity
            )
            .into_bytes(),
        );
    }
    if command.starts_with("UID STORE ") {
        return Some(Vec::new());
    }
    if let Some(search) = Search::parse(command) {
        let mut reply = "* SEARCH".to_owned();
        for uid in folder.uids(search.start..=search.end) {
            if search.field.is_none_or(|field| {
                folder
                    .raw(uid)
                    .is_some_and(|raw| header_matches(&raw, field, &search.anchor))
            }) {
                reply.push_str(&format!(" {uid}"));
            }
        }
        reply.push_str("\r\n");
        return Some(reply.into_bytes());
    }
    let (ids, attrs) = command.strip_prefix("UID FETCH ")?.split_once(' ')?;
    let mut reply = Vec::new();
    for uid in ids.split(',').map(|id| id.parse::<u32>().unwrap()) {
        let Some(raw) = folder.raw(uid) else {
            continue;
        };
        let raw = raw.as_ref();
        let headers = header(raw);
        let encoded = &raw[headers.len()..];
        let size = folder
            .sizes
            .get(&uid)
            .map_or(raw.len(), |size| *size as usize);
        let flags = folder.message_flags.get(&uid).copied().unwrap_or("\\Seen");
        if attrs.contains("BODYSTRUCTURE") {
            let default = || {
                format!(
                    "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"8BIT\" {} 1 NIL NIL)",
                    encoded.len()
                )
            };
            let structure = folder.structures.get(&uid).cloned().unwrap_or_else(default);
            reply.extend_from_slice(
                format!("* 1 FETCH (UID {uid} BODYSTRUCTURE {structure})\r\n").as_bytes(),
            );
        } else if attrs == "(UID RFC822.SIZE)" {
            reply.extend_from_slice(
                format!("* 1 FETCH (UID {uid} RFC822.SIZE {size})\r\n").as_bytes(),
            );
        } else if attrs == "(UID FLAGS)" {
            reply
                .extend_from_slice(format!("* 1 FETCH (UID {uid} FLAGS ({flags}))\r\n").as_bytes());
        } else {
            let (selector, range) = attrs.split_once("BODY.PEEK[")?.1.split_once(']')?;
            let bytes = if let Some(bytes) = folder.sections.get(&(uid, selector.into())) {
                bytes
            } else {
                match selector {
                    "HEADER" => headers,
                    "" => raw,
                    "1" | "TEXT" => encoded,
                    _ => panic!("unexpected FETCH selector: {attrs}"),
                }
            };
            let (origin, bytes) = if let Some((offset, count)) = range
                .strip_prefix('<')
                .and_then(|range| range.split_once('>'))
                .and_then(|(range, _)| range.split_once('.'))
            {
                let offset = offset.parse::<usize>().unwrap();
                let count = count.parse::<usize>().unwrap();
                let tail = bytes.get(offset..).unwrap_or_default();
                (format!("<{offset}>"), &tail[..tail.len().min(count)])
            } else {
                (String::new(), bytes)
            };
            let mut evidence = String::new();
            if attrs.contains("FLAGS") {
                evidence.push_str(&format!(" FLAGS ({flags})"));
            }
            if attrs.contains("RFC822.SIZE") {
                evidence.push_str(&format!(" RFC822.SIZE {size}"));
            }
            if attrs.contains("INTERNALDATE") {
                evidence.push_str(" INTERNALDATE \" 7-Feb-2025 09:00:00 +0000\"");
            }
            reply.extend_from_slice(
                format!(
                    "* 1 FETCH (UID {uid}{evidence} BODY[{selector}]{origin} {{{}}}\r\n",
                    bytes.len()
                )
                .as_bytes(),
            );
            reply.extend_from_slice(bytes);
            reply.extend_from_slice(b")\r\n");
        }
    }
    Some(reply)
}

fn header_matches(raw: &[u8], field: &str, anchor: &str) -> bool {
    let (headers, _) = mailparse::parse_headers(header(raw)).unwrap();
    let anchor = anchor.to_ascii_lowercase();
    headers
        .get_all_values(field)
        .iter()
        .any(|value| value.to_ascii_lowercase().contains(&anchor))
}
