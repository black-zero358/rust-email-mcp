//! A small TLS IMAP fixture for exercising the production HTTP entry points.
use super::*;
use crate::{
    accounts::AccountCatalog,
    auth::Auth,
    config::{self, Config, ValidatedAccountConfig, ValidatedAccountSet},
    journal::Journal,
};
use aws_lc_rs::{
    encoding::{AsDer, Pkcs8V1Der},
    rsa::{KeyPair, KeySize},
    signature::KeyPair as _,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use std::{
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{RootCertStore, pki_types::PrivateKeyDer},
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

#[cfg(unix)]
mod shutdown;

const PART_HEADER: &[u8] = b"Content-Type: application/octet-stream; name=part.bin\r\nContent-Disposition: attachment; filename=part.bin\r\nContent-Transfer-Encoding: base64\r\n\r\n";
const TEXT_HEADER: &[u8] =
    b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n";
const ATTACHMENT: &[u8] = &[0, 10, 128, 255];

struct Message {
    text: String,
    encoded: String,
    header: Vec<u8>,
    raw: Vec<u8>,
}

impl Message {
    fn new(text: &str) -> Self {
        let header = b"Subject: Synthetic\r\nMessage-ID: <message@example.test>\r\nContent-Type: multipart/mixed; boundary=test\r\n\r\n".to_vec();
        let encoded = STANDARD.encode(ATTACHMENT);
        let mut raw = header.clone();
        raw.extend_from_slice(b"--test\r\n");
        raw.extend_from_slice(TEXT_HEADER);
        raw.extend_from_slice(text.as_bytes());
        raw.extend_from_slice(b"\r\n--test\r\n");
        raw.extend_from_slice(PART_HEADER);
        raw.extend_from_slice(encoded.as_bytes());
        raw.extend_from_slice(b"\r\n--test--\r\n");
        Self {
            text: text.into(),
            encoded,
            header,
            raw,
        }
    }

    fn fetch(&self, attrs: &str) -> Vec<u8> {
        if attrs == "(UID BODYSTRUCTURE)" {
            return format!(
                "* 1 FETCH (UID 7 BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"8BIT\" {} 1 NIL NIL NIL NIL)(\"APPLICATION\" \"OCTET-STREAM\" (\"NAME\" \"part.bin\") NIL NIL \"BASE64\" {} NIL NIL NIL NIL) \"MIXED\" (\"BOUNDARY\" \"test\") NIL NIL))\r\n",
                self.text.len(), self.encoded.len()
            ).into_bytes();
        }
        let mut response = "* 1 FETCH (UID 7".to_owned();
        if attrs.contains("FLAGS") {
            response.push_str(" FLAGS ()");
        }
        if attrs.contains("RFC822.SIZE") {
            response.push_str(&format!(" RFC822.SIZE {}", self.raw.len()));
        }
        if attrs.contains("INTERNALDATE") {
            response.push_str(" INTERNALDATE \" 7-Feb-2025 09:00:00 +0000\"");
        }
        let mut response = response.into_bytes();
        for selector in attrs.split("BODY.PEEK[").skip(1) {
            let (section, tail) = selector.split_once(']').unwrap();
            let bytes = match section {
                "" => self.raw.as_slice(),
                "HEADER" => self.header.as_slice(),
                "1.MIME" => TEXT_HEADER,
                "1" => self.text.as_bytes(),
                "2.MIME" => PART_HEADER,
                "2" => self.encoded.as_bytes(),
                _ => panic!("unexpected FETCH section: {section}"),
            };
            let (origin, bytes) = if let Some((offset, count)) = tail
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
            response.extend_from_slice(
                format!(" BODY[{section}]{origin} {{{}}}\r\n", bytes.len()).as_bytes(),
            );
            response.extend_from_slice(bytes);
        }
        response.extend_from_slice(b")\r\n");
        response
    }
}

type DraftGate = tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>;
struct Backend {
    port: u16,
    connections: Arc<AtomicUsize>,
    draft: Arc<Mutex<Option<Vec<u8>>>>,
    task: JoinHandle<()>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Backend {
    async fn start(tls: TlsAcceptor, text: &str) -> Self {
        Self::start_with_draft_gate(tls, text, None).await
    }

    async fn start_with_draft_gate(tls: TlsAcceptor, text: &str, gate: Option<DraftGate>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        let mut message = Message::new(text);
        if gate.is_some() {
            message
                .raw
                .splice(..0, b"To: recipient@example.test\r\n".iter().copied());
        }
        let message = Arc::new(message);
        let draft = Arc::new(Mutex::new(None));
        let captured = draft.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let tls = tls.clone();
                let message = message.clone();
                let gate = gate.clone();
                let captured = captured.clone();
                tokio::spawn(async move {
                    let Ok(stream) = tls.accept(socket).await else {
                        return;
                    };
                    let mut stream = BufReader::new(stream);
                    stream.write_all(b"* OK fixture\r\n").await.unwrap();
                    loop {
                        let mut line = String::new();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let (tag, command) = line.trim_end().split_once(' ').unwrap();
                        let mut reply = match command {
                            "CAPABILITY" => b"* CAPABILITY IMAP4rev1\r\n".to_vec(),
                            command if command == "EXAMINE \"Drafts\"" || command.strip_prefix("EXAMINE ").is_some_and(|name| name.trim_matches('"').eq_ignore_ascii_case("INBOX")) => b"* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n".to_vec(),
                            command if command.starts_with("LOGIN ") => Vec::new(),
                            command if command.starts_with("LIST ") => b"* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n".to_vec(),
                            command if command.starts_with("APPEND \"Drafts\" (\\Draft) {") => {
                                let size: usize = command.rsplit_once('{').unwrap().1.strip_suffix('}').unwrap().parse().unwrap();
                                stream.write_all(b"+ continue\r\n").await.unwrap();
                                stream.flush().await.unwrap();
                                let mut bytes = vec![0; size];
                                stream.read_exact(&mut bytes).await.unwrap();
                                let mut ending = [0; 2];
                                stream.read_exact(&mut ending).await.unwrap();
                                assert_eq!(&ending, b"\r\n");
                                *captured.lock().unwrap() = Some(bytes);
                                Vec::new()
                            }
                            command if command.starts_with("UID SEARCH ") => b"* SEARCH 7\r\n".to_vec(),
                            command if command.starts_with("UID FETCH ") => {
                                let (uids, attrs) = command.strip_prefix("UID FETCH ").unwrap().split_once(' ').unwrap();
                                if uids.split(',').any(|uid| uid == "7") {
                                    if attrs.contains("BODY.PEEK[]") && let Some(gate) = &gate {
                                        let (release, wait) = tokio::sync::oneshot::channel();
                                        gate.send(release).await.unwrap();
                                        wait.await.unwrap();
                                    }
                                    message.fetch(attrs)
                                } else {
                                    Vec::new()
                                }
                            }
                            _ => panic!("unexpected IMAP command: {command}"),
                        };
                        reply.extend_from_slice(format!("{tag} OK complete\r\n").as_bytes());
                        if stream.write_all(&reply).await.is_err() || stream.flush().await.is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            port,
            connections,
            draft,
            task,
        }
    }
}

struct Credentials {
    auth: Arc<Auth>,
    signing: EncodingKey,
    header: Header,
    config: Arc<Config>,
}

impl Credentials {
    fn new(config: Arc<Config>) -> Self {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let public = key.public_key();
        let jwks = serde_json::from_value(json!({"keys":[{"kty":"RSA","kid":"fixture","alg":"RS256","use":"sig","n":URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),"e":URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero())}]})).unwrap();
        let der = AsDer::<Pkcs8V1Der>::as_der(&key).unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(der.as_ref())
        );
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture".into());
        Self {
            auth: Auth::from_test_jwks(config.clone(), jwks),
            signing: EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
            header,
            config,
        }
    }

    fn token(&self, subject: &str) -> String {
        self.token_with_expiry(subject, 300)
    }

    fn token_with_expiry(&self, subject: &str, seconds_from_now: i64) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        encode(
            &self.header,
            &json!({"sub":subject,"iss":self.config.issuer,"aud":self.config.audience,"exp":now.saturating_add_signed(seconds_from_now),"nbf":now-1,"scope":self.config.scope,"typ":"Bearer"}),
            &self.signing,
        ).unwrap()
    }
}

struct Harness {
    app: Router,
    credentials: Credentials,
    backends: [Backend; 2],
    journal: Arc<Journal>,
    cache: crate::mail::PreparedCache,
    _staging: tempfile::TempDir,
}

impl Harness {
    async fn new(metadata_cache: bool) -> Self {
        Self::with_cache(metadata_cache.then_some("metadata")).await
    }

    async fn with_cache(cache_mode: Option<&str>) -> Self {
        let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = signed.cert.der().clone();
        let tls = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                PrivateKeyDer::Pkcs8(signed.signing_key.serialize_der().into()),
            )
            .unwrap();
        let tls = TlsAcceptor::from(Arc::new(tls));
        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();
        let backends = [
            Backend::start(tls.clone(), "第一个邮箱").await,
            Backend::start(tls, "Second mailbox").await,
        ];
        let staging = tempfile::tempdir().unwrap();
        let mut config = (*config::fixture()).clone();
        config.imap_host = "localhost".into();
        config.imap_port = backends[0].port;
        config.subjects = vec!["owner".into(), "billing".into()];
        config.attachment_staging_dir = Some(staging.path().join("downloads"));
        let config = Arc::new(config);
        let mut secondary = (*config).clone();
        secondary.imap_port = backends[1].port;
        secondary.address = "billing@example.test".into();
        secondary.mode = "read".into();
        let mut settings = ValidatedAccountSet::legacy(config.clone());
        settings.source = config::AccountConfigSource::ExtendLegacy;
        settings.accounts[0].read_subjects = vec!["owner".into()];
        settings.accounts[0].write_subjects = vec!["owner".into()];
        settings.accounts.push(ValidatedAccountConfig {
            registration_key: "billing-registration".into(),
            alias: "billing".into(),
            display_name: "Billing".into(),
            config: Arc::new(secondary),
            read_subjects: vec!["billing".into()],
            write_subjects: vec![],
            is_legacy: false,
        });
        let catalog = AccountCatalog::from_test_accounts(settings.clone()).unwrap();
        let cache_settings = cache_mode.map(|mode| {
            let path = staging.path().join("cache.json");
            std::fs::write(
                &path,
                json!({"version":1,"mode":mode,"directory":"cache","max_bytes":33554432,
                "accounts":[{"account_id":"default","mailboxes":["INBOX"],"max_bytes":8388608}]})
                .to_string(),
            )
            .unwrap();
            config::load_cache_file(&path, &settings).unwrap().unwrap()
        });
        let cache = if cache_mode == Some("full") {
            let run = crate::accounts::CacheRunIdentity {
                instance: uuid::Uuid::new_v4(),
                previous: None,
                current: uuid::Uuid::new_v4(),
            };
            MailRuntime::prepare_cache_for_run(&catalog, cache_settings, Some(run)).unwrap()
        } else {
            MailRuntime::prepare_cache(&catalog, cache_settings).unwrap()
        };
        let journal = Arc::new(Journal::open(Path::new(":memory:")).unwrap());
        let mail = Arc::new(
            MailRuntime::with_cache(catalog, journal.clone(), cache.clone())
                .unwrap()
                .with_test_imap_roots(roots),
        );
        let credentials = Credentials::new(config.clone());
        let app = http_router(
            &config,
            mail,
            credentials.auth.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        Self {
            app,
            credentials,
            backends,
            journal,
            cache,
            _staging: staging,
        }
    }

    fn connections(&self) -> [usize; 2] {
        self.backends
            .each_ref()
            .map(|backend| backend.connections.load(Ordering::SeqCst))
    }

    async fn rpc(
        &self,
        subject: Option<&str>,
        id: Value,
        method: &str,
        params: Value,
    ) -> axum::response::Response {
        let authorization =
            subject.map(|subject| format!("Bearer {}", self.credentials.token(subject)));
        self.rpc_with_authorization(authorization.as_deref(), id, method, params)
            .await
    }

    async fn rpc_with_authorization(
        &self,
        authorization: Option<&str>,
        id: Value,
        method: &str,
        params: Value,
    ) -> axum::response::Response {
        let mut request = Request::post("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-06-18");
        if let Some(authorization) = authorization {
            request = request.header(header::AUTHORIZATION, authorization);
        }
        let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn call(&self, subject: &str, name: &str, arguments: Value) -> Value {
        let id = json!("逃逸 \"\\");
        let response = self
            .rpc(
                Some(subject),
                id.clone(),
                "tools/call",
                json!({"name":name,"arguments":arguments}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = to_bytes(response.into_body(), self.credentials.config.max_output)
            .await
            .unwrap();
        let wire: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(wire["id"], id);
        let result = &wire["result"];
        let content = &result["structuredContent"];
        assert_eq!(content["request_id"], request_id);
        assert_eq!(
            serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
            *content
        );
        let tool = tools("full")
            .into_iter()
            .find(|tool| tool.name == name)
            .unwrap();
        let schema = serde_json::to_value(tool.output_schema.unwrap()).unwrap();
        jsonschema::draft202012::options()
            .should_validate_formats(true)
            .build(&schema)
            .unwrap()
            .validate(content)
            .unwrap_or_else(|error| panic!("{name} {content}: {error}"));
        content.clone()
    }

    async fn attachment(&self, authorization: &str, url: &str) -> axum::response::Response {
        let url = reqwest::Url::parse(url).unwrap();
        self.app
            .clone()
            .oneshot(
                Request::get(url.path())
                    .header(header::HOST, "localhost")
                    .header(header::AUTHORIZATION, authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn signed_http_reads_keep_same_uid_in_each_authorized_account() {
    let h = Harness::new(false).await;
    let denied = h
        .call(
            "billing",
            "get_email",
            json!({"account_id":"default","uid":7,"uid_validity":9}),
        )
        .await;
    assert!(denied["error"].is_object(), "{denied}");
    assert_eq!(h.connections(), [0, 0]);
    for (subject, account, expected) in [
        ("owner", "default", "第一个邮箱"),
        ("billing", "billing", "Second mailbox"),
    ] {
        let body = h
            .call(
                subject,
                "get_email",
                json!({"account_id":account,"uid":7,"uid_validity":9}),
            )
            .await;
        assert_eq!(body["data"]["account_id"], account, "{body}");
        assert_eq!(body["data"]["body"], expected, "{body}");
    }
    let stale = h
        .call("owner", "get_email", json!({"uid":7,"uid_validity":8}))
        .await;
    assert_eq!(stale["error"]["code"], "STALE_UID", "{stale}");
}

#[tokio::test]
async fn signed_legacy_writes_accept_crlf_in_body_only() {
    use sha2::{Digest, Sha256};

    async fn write(h: &Harness, name: &str, arguments: &Value) -> Value {
        let response = h
            .rpc(
                Some("owner"),
                json!(1),
                "tools/call",
                json!({"name":name,"arguments":arguments}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()["result"]["structuredContent"].clone()
    }

    let h = Harness::new(false).await;
    let crlf = "first\r\n中文\nlast\r\n";
    let lf = "first\n中文\nlast\n";
    for name in ["save_draft", "send_email", "reply_email", "forward_email"] {
        let mut args = json!({"to":["recipient@example.test"],"body":crlf});
        if matches!(name, "save_draft" | "send_email") {
            args["subject"] = json!("synthetic");
        } else {
            args["uid"] = json!(7);
            args["uid_validity"] = json!(9);
        }
        if name != "save_draft" {
            args["operation_key"] = json!(name);
            let fingerprint = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&json!([name, args])).unwrap())
            );
            h.journal
                .claim("owner", name, &fingerprint, "<fixture@example.test>")
                .unwrap();
            h.journal.finish("owner", name, "accepted", &[]).unwrap();
        }

        let before = h.connections();
        for (field, value) in [
            ("body", json!("first\rlast")),
            ("body", json!("first\r")),
            ("body", json!("first\0last")),
            ("to", json!(["recipient@example.test\r\n"])),
        ] {
            let mut bad = args.clone();
            bad[field] = value;
            assert_eq!(
                write(&h, name, &bad).await["error"]["code"],
                "INVALID_INPUT",
                "{name} {field}"
            );
        }
        assert_eq!(h.connections(), before);
        let result = write(&h, name, &args).await;
        args["body"] = json!(lf);
        if name == "save_draft" {
            assert_eq!(result["data"]["status"], "saved", "{result}");
            let draft = h.backends[0].draft.lock().unwrap().take().unwrap();
            assert_eq!(
                mailparse::parse_mail(&draft).unwrap().get_body().unwrap(),
                "first\r\n中文\r\nlast\r\n"
            );
            assert_eq!(write(&h, name, &args).await["data"]["status"], "saved");
            let draft = h.backends[0].draft.lock().unwrap().take().unwrap();
            assert_eq!(
                mailparse::parse_mail(&draft).unwrap().get_body().unwrap(),
                "first\r\n中文\r\nlast\r\n"
            );
        } else {
            assert_eq!(
                result["data"]["operation"]["status"], "accepted",
                "{name}: {result}"
            );
            assert_eq!(
                write(&h, name, &args).await["error"]["code"],
                "IDEMPOTENCY_CONFLICT"
            );
            assert_eq!(h.connections(), before);
        }
    }
}

#[tokio::test]
async fn http_auth_and_rpc_id_gate_each_request_before_mail() {
    let h = Harness::new(false).await;
    let params = json!({"name":"get_email","arguments":{"uid":7,"uid_validity":9}});
    let response = h.rpc(None, json!(1), "tools/call", params.clone()).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let oversized = h
        .rpc(Some("owner"), json!("x".repeat(1023)), "tools/call", params)
        .await;
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(h.connections(), [0, 0]);
}

#[tokio::test]
async fn signed_target_attachment_get_rechecks_subject_and_returns_exact_binary() {
    let h = Harness::with_cache(Some("full")).await;
    let listed = h
        .call(
            "owner",
            "list_attachments",
            json!({"uid":7,"uid_validity":9}),
        )
        .await;
    assert_eq!(
        listed["data"]["read_diagnostics"]["source"], "bodystructure",
        "{listed}"
    );
    let descriptor = &listed["data"]["attachments"][0];
    assert_eq!(descriptor["filename"], "part.bin", "{listed}");
    let attachment_id = &descriptor["attachment_id"];
    for mailbox in ["INBOX", "Inbox", "inbox"] {
        let listed = h
            .call(
                "owner",
                "list_attachments",
                json!({"mailbox":mailbox,"uid":7,"uid_validity":9}),
            )
            .await;
        assert_eq!(
            listed["data"]["attachments"][0]["attachment_id"], *attachment_id,
            "{listed}"
        );
    }
    let prepared = h
        .call(
            "owner",
            "prepare_attachment_download",
            json!({"mailbox":"Inbox","uid":7,"uid_validity":9,"attachment_id":attachment_id}),
        )
        .await;
    let url = prepared["data"]["download_url"]
        .as_str()
        .unwrap_or_else(|| panic!("{prepared}"));
    let before = h.connections();
    let other = format!("Bearer {}", h.credentials.token("billing"));
    assert_eq!(
        h.attachment(&other, url).await.status(),
        StatusCode::NOT_FOUND
    );
    let token = h.credentials.token("owner");
    let expired = h.credentials.token_with_expiry("owner", -120);
    for (case, authorization) in [
        ("wrong scheme", format!("Basic {token}")),
        ("missing token", "Bearer".into()),
        ("empty token", "Bearer  ".into()),
        ("tab separator", format!("Bearer\t{token}")),
        ("tab after space", format!("Bearer \t{token}")),
        ("space before scheme", format!(" Bearer {token}")),
        ("space after token", format!("Bearer {token} ")),
        ("invalid token", "Bearer invalid-token".into()),
        ("expired token", format!("bEaReR {expired}")),
    ] {
        let rpc = h
            .rpc_with_authorization(Some(&authorization), json!(1), "tools/list", json!({}))
            .await;
        let download = h.attachment(&authorization, url).await;
        for response in [rpc, download] {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{case}");
            assert!(
                response.headers().contains_key(header::WWW_AUTHENTICATE),
                "{case}"
            );
        }
    }
    assert_eq!(h.connections(), before);

    for (scheme, spaces) in [
        ("Bearer", " "),
        ("bearer", " "),
        ("BEARER", " "),
        ("bEaReR", "   "),
    ] {
        let authorization = format!("{scheme}{spaces}{token}");
        let rpc = h
            .rpc_with_authorization(Some(&authorization), json!(1), "tools/list", json!({}))
            .await;
        let response = h.attachment(&authorization, url).await;
        assert_eq!(
            (rpc.status(), response.status()),
            (StatusCode::OK, StatusCode::OK),
            "{scheme}"
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap().as_ref(),
            ATTACHMENT
        );
    }

    let inline = h.call("owner", "download_attachment", json!({"mailbox":"inbox","uid":7,"uid_validity":9,"attachment_id":attachment_id,"attachment_index":descriptor["attachment_index"]})).await;
    assert_eq!(
        inline["data"]["base64"],
        STANDARD.encode(ATTACHMENT),
        "{inline}"
    );
    let before = h.connections();
    let cached = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = h.call("owner", "download_attachment", json!({"read_source":"cache","mailbox":"Inbox","uid":7,"uid_validity":9,"attachment_id":attachment_id})).await;
            if result["error"]["code"] != "CACHE_MISS" {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    assert_eq!(cached["data"]["read_source"], "cache", "{cached}");
    assert_eq!(cached["data"]["attachment_id"], *attachment_id, "{cached}");
    assert_eq!(cached["data"]["base64"], STANDARD.encode(ATTACHMENT));
    assert_eq!(h.connections(), before);
    drop(h.app);
    h.cache.finish().await;
}

#[tokio::test]
async fn metadata_cache_status_reports_live_evidence_without_an_imap_read() {
    let h = Harness::new(true).await;
    let live = h
        .call("owner", "get_email", json!({"uid":7,"uid_validity":9}))
        .await;
    assert_eq!(live["data"]["body"], "第一个邮箱", "{live}");
    let before = h.connections();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = h.call("owner", "get_cache_status", json!({})).await;
            assert_eq!(
                status["data"]["write_health"]["failed_writes"], 0,
                "{status}"
            );
            if status["data"]["storage"]["folders"][0]["verified_parts"] == 1 {
                assert_eq!(status["data"]["storage"]["folders"][0]["uid_validity"], 9);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.connections(), before);
    drop(h.app);
    h.cache.finish().await;
}

#[tokio::test]
async fn offline_body_search_publishes_projection_coverage_through_its_schema() {
    let h = Harness::with_cache(Some("full")).await;
    let live = h
        .call("owner", "get_email", json!({"uid":7,"uid_validity":9}))
        .await;
    assert_eq!(live["data"]["body"], "第一个邮箱");
    let before = h.connections();
    let searched = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let page = h
                .call(
                    "owner",
                    "search_emails",
                    json!({"query":"第一个","search_source":"cache","uid_validity":9}),
                )
                .await;
            if page["data"]["emails"][0]["cache_match"]["body"] == true {
                break page;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let index = &searched["data"]["cache"]["scopes"][0]["coverage"]["search_index"];
    assert_eq!(index["indexed_body"], 1);
    assert_eq!(index["version"], 2);
    assert_eq!(index["rebuild_pending"], false);
    assert_eq!(h.connections(), before);
    drop(h.app);
    h.cache.finish().await;
}
