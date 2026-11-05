//! Multi-folder, mutable local TLS IMAP fixture for the production MCP handler.
use super::super::{attachments::Credentials, attachments::router, *};
use crate::config::{AccountConfigSource, Config, ValidatedAccountConfig};
use mailparse::MailHeaderMap;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

pub(in crate::server::output_tests) struct Folder {
    pub flags: &'static str,
    pub validity: u32,
    pub next: Option<u32>,
    pub messages: BTreeMap<u32, Vec<u8>>,
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
            messages: messages.into(),
        }
    }

    fn high_water(&self) -> u32 {
        self.next
            .unwrap_or_else(|| self.messages.keys().next_back().copied().unwrap_or(0) + 1)
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
    Hang,
}

#[derive(Clone, Debug)]
pub(in crate::server::output_tests) struct Command {
    pub connection: usize,
    pub mailbox: String,
    pub text: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Search<'a> {
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
    commands: Mutex<Vec<Command>>,
    connections: AtomicUsize,
    changed: tokio::sync::Notify,
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
            commands: Mutex::default(),
            connections: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        });
        let protocol = imap.clone();
        let backend = tokio::spawn(async move {
            let tls = TlsAcceptor::from(Arc::new(tls));
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                socket.set_nodelay(true).unwrap();
                let id = protocol.connections.fetch_add(1, Ordering::SeqCst) + 1;
                let tls = tls.clone();
                let protocol = protocol.clone();
                tokio::spawn(async move {
                    if let Ok(stream) = tls.accept(socket).await {
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

    pub fn commands(&self) -> Vec<Command> {
        self.imap.commands.lock().unwrap().clone()
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

    async fn call_named(
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
        assert_contract(name, &content);
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
        if command.starts_with("LOGIN ") {
            if stream
                .write_all(format!("{tag} OK login\r\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
            continue;
        }
        if let Some(mailbox) = command.strip_prefix("EXAMINE ") {
            selected = unquote(mailbox).unwrap_or_else(|| mailbox.to_owned());
        }
        protocol.commands.lock().unwrap().push(Command {
            connection,
            mailbox: selected.clone(),
            text: command.into(),
        });
        protocol.changed.notify_waiters();
        let fault = protocol
            .faults
            .lock()
            .unwrap()
            .remove(&(selected.clone(), command.into()));
        if matches!(fault, Some(Fault::Hang)) {
            let _ = stream.read_u8().await;
            return;
        }
        let reply = {
            let folders = protocol.folders.lock().unwrap();
            respond(&folders, &selected, command)
        };
        let (mut reply, status) = match (reply, fault) {
            (Some(reply), Some(Fault::NoAfterData)) => (reply, "NO"),
            (Some(reply), _) => (reply, "OK"),
            (None, _) => (Vec::new(), "NO"),
        };
        reply.extend_from_slice(format!("{tag} {status} synthetic\r\n").as_bytes());
        if stream.write_all(&reply).await.is_err() {
            return;
        }
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
    if command.starts_with("EXAMINE ") {
        return Some(
            format!(
                "* {} EXISTS\r\n* OK [UIDVALIDITY {}] valid\r\n* OK [UIDNEXT {}] next\r\n",
                folder.messages.len(),
                folder.validity,
                folder.high_water()
            )
            .into_bytes(),
        );
    }
    if let Some(search) = Search::parse(command) {
        let mut reply = "* SEARCH".to_owned();
        for (uid, raw) in folder.messages.range(search.start..=search.end) {
            if search
                .field
                .is_none_or(|field| header_matches(raw, field, &search.anchor))
            {
                reply.push_str(&format!(" {uid}"));
            }
        }
        reply.push_str("\r\n");
        return Some(reply.into_bytes());
    }
    let (ids, attrs) = command.strip_prefix("UID FETCH ")?.split_once(' ')?;
    let mut reply = Vec::new();
    for uid in ids.split(',').map(|id| id.parse::<u32>().unwrap()) {
        let Some(raw) = folder.messages.get(&uid) else {
            continue;
        };
        let headers = header(raw);
        let encoded = &raw[headers.len()..];
        if attrs.contains("BODYSTRUCTURE") {
            reply.extend_from_slice(format!("* 1 FETCH (UID {uid} BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"8BIT\" {} 1 NIL NIL))\r\n", encoded.len()).as_bytes());
        } else if attrs == "(UID RFC822.SIZE)" {
            reply.extend_from_slice(
                format!("* 1 FETCH (UID {uid} RFC822.SIZE {})\r\n", raw.len()).as_bytes(),
            );
        } else {
            let (selector, range) = attrs.split_once("BODY.PEEK[")?.1.split_once(']')?;
            let bytes = match selector {
                "HEADER" => headers,
                "" => raw,
                "1" | "TEXT" => encoded,
                _ => panic!("unexpected FETCH selector: {attrs}"),
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
            reply.extend_from_slice(
                format!(
                    "* 1 FETCH (UID {uid} BODY[{selector}]{origin} {{{}}}\r\n",
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
