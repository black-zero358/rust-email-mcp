//! Contract checks run the real HTTP MCP handler against an isolated TLS IMAP
//! fixture, then validate returned structured content with the standard engine.
use super::*;
use crate::{config, journal::Journal};
use axum::{Router, body::Body, http::Request, middleware};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        RootCertStore, ServerConfig,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};
use tower::ServiceExt;

mod attachments;

fn validator(name: &str) -> jsonschema::Validator {
    let tool = tools("full").into_iter().find(|t| t.name == name).unwrap();
    let schema = serde_json::to_value(tool.output_schema.unwrap()).unwrap();
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap()
}
fn assert_contract(name: &str, content: &Value) {
    let validator = validator(name);
    let errors = validator
        .iter_errors(content)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "{name}: {}\n{content}",
        errors.join("\n")
    );
}

fn raw(uid: u32, body: &str) -> Vec<u8> {
    format!("Message-ID: <id{uid}@example.test>\r\nReferences: <id7@example.test>\r\nSubject: 合成 \\\"\r\nFrom: sender@example.test\r\nTo: owner@example.test\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}").into_bytes()
}
fn multipart() -> Vec<u8> {
    b"Message-ID: <id7@example.test>\r\nSubject: synthetic\r\nFrom: sender@example.test\r\nContent-Type: multipart/mixed; boundary=m\r\n\r\n--m\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello \"quoted\" \\ escaped\r\n--m\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=same.txt\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--m\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=same.txt\r\n\r\nxyz\r\n--m--\r\n".to_vec()
}
fn header(raw: &[u8]) -> &[u8] {
    raw.windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map_or(raw, |i| &raw[..i + 4])
}

struct Harness {
    app: Router,
    service: Arc<MailService>,
    imap: tokio::task::JoinHandle<()>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.imap.abort();
    }
}
impl Harness {
    async fn new(
        messages: BTreeMap<u32, Vec<u8>>,
        fatal_uid: Option<u32>,
        max_output: usize,
    ) -> Self {
        Self::configured(messages, fatal_uid, max_output, false).await
    }
    async fn configured(
        messages: BTreeMap<u32, Vec<u8>>,
        fatal_uid: Option<u32>,
        max_output: usize,
        stall_greeting: bool,
    ) -> Self {
        Self::with_resource(messages, fatal_uid, max_output, stall_greeting, None).await
    }
    async fn with_resource(
        messages: BTreeMap<u32, Vec<u8>>,
        fatal_uid: Option<u32>,
        max_output: usize,
        stall_greeting: bool,
        resource: Option<String>,
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
        let mut configuration = (*config::fixture()).clone();
        configuration.imap_host = "localhost".into();
        configuration.imap_port = listener.local_addr().unwrap().port();
        configuration.max_output = max_output;
        if let Some(resource) = resource {
            configuration.resource = resource;
        }
        configuration.timeout = if stall_greeting {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(5)
        };
        let service = Arc::new(
            MailService::new(
                Arc::new(configuration),
                Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap()),
            )
            .unwrap()
            .with_test_imap_roots(roots),
        );
        let messages = Arc::new(messages);
        let imap = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(tls));
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let messages = messages.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut stream = BufReader::new(stream);
                    if stall_greeting {
                        // Diagnose waits for the server greeting until its
                        // deadline closes this isolated connection.
                        let _ = stream.read_u8().await;
                        return;
                    }
                    if stream
                        .write_all(b"* OK isolated fixture\r\n")
                        .await
                        .is_err()
                    {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        let Some((tag, command)) = line.trim_end_matches("\r\n").split_once(' ')
                        else {
                            panic!("invalid fixture command");
                        };
                        let mut response = Vec::new();
                        if command.starts_with("LOGIN ") {
                        } else if command.starts_with("EXAMINE ") {
                            let next = messages.keys().next_back().copied().unwrap_or(0) + 1;
                            response.extend_from_slice(format!("* {} EXISTS\r\n* OK [UIDVALIDITY 9] stable\r\n* OK [UIDNEXT {next}] next\r\n",messages.len()).as_bytes());
                        } else if command.starts_with("LIST ") {
                            response.extend_from_slice(b"* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\NoSelect) NIL \"Group\"\r\n* LIST (\\Sent) \"/\" \"Sent\"\r\n");
                        } else if command.starts_with("STATUS ") {
                            let next = messages.keys().next_back().copied().unwrap_or(0) + 1;
                            response.extend_from_slice(format!("* STATUS INBOX (MESSAGES {} UNSEEN 0 RECENT 0 UIDNEXT {next} UIDVALIDITY 9)\r\n",messages.len()).as_bytes());
                        } else if command == "CAPABILITY" {
                            response.extend_from_slice(b"* CAPABILITY IMAP4rev1 UIDPLUS MOVE\r\n");
                        } else if command.starts_with("UID SEARCH ") {
                            if let Some(size) = command
                                .strip_suffix('}')
                                .and_then(|s| s.rsplit_once('{'))
                                .and_then(|(_, n)| n.parse::<usize>().ok())
                            {
                                stream.write_all(b"+ literal\r\n").await.unwrap();
                                let mut literal = vec![0; size + 2];
                                stream.read_exact(&mut literal).await.unwrap();
                            }
                            let range = command
                                .split_whitespace()
                                .skip_while(|s| *s != "UID")
                                .nth(1)
                                .unwrap_or("");
                            // The first UID belongs to UID SEARCH; obtain the
                            // final numerical range independently of text filters.
                            let range = command
                                .split_whitespace()
                                .find(|s| {
                                    s.contains(':')
                                        && s.bytes().all(|b| b.is_ascii_digit() || b == b':')
                                })
                                .unwrap_or(range);
                            let (start, end) = range
                                .split_once(':')
                                .and_then(|(a, b)| {
                                    Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?))
                                })
                                .unwrap_or((1, u32::MAX));
                            let ids = messages
                                .keys()
                                .filter(|uid| **uid >= start && **uid <= end)
                                .map(u32::to_string)
                                .collect::<Vec<_>>()
                                .join(" ");
                            response.extend_from_slice(format!("* SEARCH {ids}\r\n").as_bytes());
                        } else if let Some(fetch) = command.strip_prefix("UID FETCH ") {
                            let (set, attrs) = fetch.split_once(' ').unwrap();
                            for uid in set.split(',').map(|id| id.parse::<u32>().unwrap()) {
                                let Some(raw) = messages.get(&uid) else {
                                    continue;
                                };
                                if fatal_uid == Some(uid) && attrs.contains("BODY.PEEK[]") {
                                    stream
                                        .write_all(
                                            format!("{tag} NO synthetic failure\r\n").as_bytes(),
                                        )
                                        .await
                                        .unwrap();
                                    return;
                                }
                                if attrs == "(UID FLAGS)" {
                                    response.extend_from_slice(
                                        format!("* 1 FETCH (UID {uid} FLAGS ())\r\n").as_bytes(),
                                    );
                                } else if attrs == "(UID RFC822.SIZE)" {
                                    response.extend_from_slice(
                                        format!(
                                            "* 1 FETCH (UID {uid} RFC822.SIZE {})\r\n",
                                            raw.len()
                                        )
                                        .as_bytes(),
                                    );
                                } else {
                                    let is_header = attrs.contains("HEADER");
                                    let bytes = if is_header { header(raw) } else { raw };
                                    let section = if is_header { "HEADER" } else { "" };
                                    response.extend_from_slice(format!("* 1 FETCH (UID {uid} FLAGS () RFC822.SIZE {} BODY[{section}] {{{}}}\r\n",raw.len(),bytes.len()).as_bytes());
                                    response.extend_from_slice(bytes);
                                    response.extend_from_slice(b")\r\n");
                                }
                            }
                        } else {
                            panic!("unexpected or writable fixture command: {command}");
                        }
                        response.extend_from_slice(format!("{tag} OK complete\r\n").as_bytes());
                        if stream.write_all(&response).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let mail = service.clone();
        let transport = StreamableHttpService::new(
            move || Ok(EmailServer(mail.clone())),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true),
        );
        let app = Router::new()
            .nest_service("/mcp", transport)
            .layer(middleware::from_fn(
                |mut request: axum::extract::Request, next: middleware::Next| async move {
                    request.extensions_mut().insert(Principal("owner".into()));
                    request
                        .extensions_mut()
                        .insert(HttpRequestId("synthetic-request".into()));
                    next.run(request).await
                },
            ));
        Self { app, service, imap }
    }
    async fn call(&self, name: &str, args: Value) -> Value {
        self.call_id(name, args, json!(1)).await
    }
    async fn call_id(&self, name: &str, args: Value, id: Value) -> Value {
        let response = self.app.clone().oneshot(Request::post("/mcp")
            .header("Host","localhost")
            .header("Content-Type","application/json").header("Accept","application/json, text/event-stream")
            .header("MCP-Protocol-Version","2025-06-18")
            .body(Body::from(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}).to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        assert!(
            status.is_success(),
            "HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        let wire: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(wire.get("error").is_none(), "{wire}");
        assert_eq!(wire["id"], id);
        let content = wire["result"]["structuredContent"].clone();
        assert_contract(name, &content);
        let text: Value =
            serde_json::from_str(wire["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(content, text);
        assert!(
            bytes.len() <= self.service.config.max_output,
            "actual HTTP JSON exceeds complete response budget"
        );
        content
    }
}

#[tokio::test]
async fn every_read_output_schema_validates_real_handler_success_and_empty_results() {
    let h = Harness::new(BTreeMap::from([(7, multipart())]), None, 64 * 1024).await;
    for (name, args) in [
        ("list_accounts", json!({})),
        ("list_mailboxes", json!({})),
        ("list_emails", json!({})),
        ("search_emails", json!({"subject":"中文"})),
        ("get_email", json!({"uid":7,"uid_validity":9})),
        ("get_email_status", json!({"uid":7,"uid_validity":9})),
        ("get_emails", json!({"uids":[7],"uid_validity":9})),
        (
            "download_attachment",
            json!({"uid":7,"uid_validity":9,"attachment_index":1}),
        ),
        ("list_attachments", json!({"uid":7,"uid_validity":9})),
        (
            "prepare_attachment_download",
            json!({"uid":7,"uid_validity":9,"attachment_index":2}),
        ),
        ("get_operation", json!({"operation_key":"missing"})),
        ("read_conversation", json!({"uid":7,"uid_validity":9})),
        (
            "locate_message",
            json!({"message_id":"<id7@example.test>","mailboxes":["INBOX"]}),
        ),
        (
            "get_mailbox_statistics",
            json!({"date_start":"2024-02-28","date_end":"2024-03-01"}),
        ),
        ("diagnose_connectivity", json!({})),
    ] {
        let content = h.call(name, args).await;
        assert!(content.get("data").is_some(), "{name}: {content}");
    }
    let empty = Harness::new(BTreeMap::new(), None, 64 * 1024).await;
    for name in ["list_emails", "search_emails", "get_mailbox_statistics"] {
        let content = empty.call(name, json!({})).await;
        assert!(content.get("data").is_some());
    }
    let accounts = h.call("list_accounts", json!({})).await;
    assert_eq!(
        accounts["data"]["accounts"][0]["capabilities"]["cross_mailbox_conversation"],
        false
    );
    assert_eq!(
        accounts["data"]["accounts"][0]["limits"]["text_utf8_bytes"],
        2048
    );
    let folders = h.call("list_mailboxes", json!({})).await;
    assert_eq!(folders["data"]["mailboxes"][0]["role_source"], "protocol");
    assert_eq!(folders["data"]["mailboxes"][1]["selectable"], false);
    assert!(folders["data"]["mailboxes"][1]["delimiter"].is_null());
    assert_eq!(
        folders["data"]["mailboxes"][2]["special_use"],
        json!(["sent"])
    );
    let page = h.call("search_emails", json!({"before_uid":1001})).await;
    assert_eq!(page["data"]["emails"], json!([]));
    assert_eq!(page["data"]["next_before_uid"], 501);
    let locate = h
        .call(
            "locate_message",
            json!({"message_id":"<missing@example.test>","mailboxes":["INBOX"]}),
        )
        .await;
    assert_eq!(locate["data"]["matches"], json!([]));
    assert_eq!(locate["data"]["complete"], false);
    assert_eq!(
        h.call("diagnose_connectivity", json!({})).await["data"]["ok"],
        true
    );
    assert!(
        h.call("get_operation", json!({"operation_key":"missing"}))
            .await["data"]["operation"]
            .is_null()
    );
    for status in ["sending", "accepted", "rejected", "unknown"] {
        h.service
            .journal
            .claim("owner", status, "hash", "<legacy@example.test>")
            .unwrap();
        if status != "sending" {
            h.service
                .journal
                .finish("owner", status, status, &["legacy-warning".into()])
                .unwrap();
        }
        let content = h
            .call("get_operation", json!({"operation_key":status}))
            .await;
        assert_eq!(content["data"]["operation"]["status"], status);
    }
}

#[tokio::test]
async fn handler_html_display_metadata_and_single_message_errors_match_contracts() {
    let html = b"Message-ID: <id7@example.test>\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p><a href='https://example.test/'>good</a><a href='javascript:bad'>bad</a><img src='https://tracking.test/' alt='picture'></p>".to_vec();
    let h = Harness::new(BTreeMap::from([(7, html)]), None, 64 * 1024).await;
    for (name, args, path) in [
        (
            "get_email",
            json!({"uid":7,"uid_validity":9,"html_display":"readable"}),
            "/data",
        ),
        (
            "get_emails",
            json!({"uids":[7],"uid_validity":9,"html_display":"readable"}),
            "/data/items/0/email",
        ),
        (
            "read_conversation",
            json!({"uid":7,"uid_validity":9,"html_display":"readable"}),
            "/data/messages/0",
        ),
    ] {
        let content = h.call(name, args).await;
        let message = content.pointer(path).unwrap();
        assert_eq!(
            message["html_display_metadata"]["format_version"],
            "readable-v1"
        );
        assert_eq!(message["html_display_metadata"]["html_parts"], 1);
        assert_eq!(message["html_display_metadata"]["omitted"], true);
        assert!(
            message["body"]
                .as_str()
                .unwrap()
                .contains("https://example.test/")
        );
        assert!(!message["body"].as_str().unwrap().contains("tracking.test"));
    }
    let no_attachments = h
        .call("list_attachments", json!({"uid":7,"uid_validity":9}))
        .await;
    assert_eq!(no_attachments["data"]["attachments"], json!([]));
    assert!(no_attachments["data"]["next_attachment_offset"].is_null());
    assert_eq!(
        h.call("get_email", json!({"uid":7,"uid_validity":8})).await["error"]["code"],
        "STALE_UID"
    );
    assert_eq!(
        h.call("get_email_status", json!({"uid":99,"uid_validity":9}))
            .await["error"]["code"],
        "NOT_FOUND"
    );
    let huge = raw(7, &"body".repeat(100));
    let mut subject = b"Subject: ".to_vec();
    subject.extend_from_slice(&vec![b'x'; 40000]);
    subject.extend_from_slice(b"\r\n");
    subject.extend_from_slice(&huge);
    let small = Harness::new(BTreeMap::from([(7, subject)]), None, 8192).await;
    let fallback = small
        .call("get_email", json!({"uid":7,"uid_validity":9}))
        .await;
    assert_eq!(
        fallback["error"]["details"]["limit_kind"],
        "wire_response_bytes"
    );
}

#[tokio::test]
async fn handler_contracts_cover_item_errors_unprocessed_and_conversation_omissions() {
    let bad = b"Message-ID: <id8@example.test>\r\nReferences: <id7@example.test>\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: unsupported\r\n\r\nbody".to_vec();
    let data = BTreeMap::from([
        (7, raw(7, "text")),
        (8, bad),
        (9, raw(9, "later")),
        (10, raw(10, "unprocessed")),
    ]);
    let h = Harness::new(data.clone(), Some(9), 64 * 1024).await;
    let result = h
        .call("get_emails", json!({"uids":[7,8,9,10],"uid_validity":9}))
        .await;
    assert_eq!(result["data"]["items"][0]["ok"], true);
    assert_eq!(
        result["data"]["items"][1]["error"]["code"],
        "UNSUPPORTED_BODY"
    );
    assert_eq!(result["data"]["items"][2]["error"]["code"], "MAIL_BACKEND");
    assert_eq!(result["data"]["items"][3]["status"], "unprocessed");
    let h = Harness::new(data, None, 64 * 1024).await;
    let result = h
        .call(
            "read_conversation",
            json!({"uid":7,"uid_validity":9,"limit":2}),
        )
        .await;
    assert!(
        !result["data"]["message_errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(result["data"]["complete"], false);
    assert!(
        !result["data"]["omitted_uids"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut seed = b"Subject: ".to_vec();
    seed.extend_from_slice(&vec![b's'; 2048]);
    seed.extend_from_slice(b"\r\n");
    seed.extend_from_slice(&raw(7, "seed"));
    let mut too_large_header = b"Subject: ".to_vec();
    too_large_header.extend_from_slice(&vec![b'h'; 128 * 1024]);
    too_large_header.extend_from_slice(b"\r\n");
    too_large_header.extend_from_slice(&raw(8, "candidate"));
    let h = Harness::new(
        BTreeMap::from([(7, seed), (8, too_large_header)]),
        None,
        64 * 1024,
    )
    .await;
    let result = h
        .call("read_conversation", json!({"uid":7,"uid_validity":9}))
        .await;
    assert_eq!(
        result["data"]["skipped_candidates"][0]["error"]["details"]["limit_kind"],
        "header_bytes"
    );
    assert_eq!(
        result["data"]["messages"][0]["metadata_fields_omitted"],
        json!(["subject"])
    );
    assert!(result["data"]["messages"][0]["subject"].is_null());
    assert_eq!(result["data"]["complete"], false);
}

#[tokio::test]
async fn handler_contracts_cover_errors_details_downgrade_and_status_preserving_fallback() {
    let h = Harness::new(
        BTreeMap::from([(7, raw(7, &"中😀\"\\\n".repeat(16000)))]),
        None,
        8192,
    )
    .await;
    let id = json!("\\\"中".repeat(60));
    let result = h
        .call_id("get_email", json!({"uid":7,"uid_validity":9}), id.clone())
        .await;
    assert_eq!(result["data"]["body_truncated"], true);
    assert!(!result["data"]["body"].as_str().unwrap().is_empty());
    for tool in tools("read") {
        let result = h
            .call_id(
                &tool.name,
                json!({"private-unknown-key":"private-value"}),
                id.clone(),
            )
            .await;
        assert_eq!(result["error"]["code"], "INVALID_INPUT");
        assert!(result["error"].get("details").is_some());
        assert!(!result.to_string().contains("private-unknown-key"));
        assert!(!result.to_string().contains("private-value"));
    }
    let compact = Harness::new(BTreeMap::new(), None, 1800).await;
    let result = compact
        .call(
            "get_emails",
            json!({"uids":vec!["wrong";20],"uid_validity":0}),
        )
        .await;
    assert_eq!(result["error"]["code"], "INVALID_INPUT");
    assert!(result["error"].get("details").is_none());
    h.service
        .journal
        .claim("owner", "large", "hash", "<legacy@example.test>")
        .unwrap();
    h.service
        .journal
        .finish("owner", "large", "accepted", &["large".repeat(10000)])
        .unwrap();
    let result = h
        .call_id("get_operation", json!({"operation_key":"large"}), id)
        .await;
    assert_eq!(result["error"]["code"], "LIMIT_EXCEEDED");
    assert_eq!(result["operation"]["status"], "accepted");
    assert_eq!(result["retry_allowed"], false);
    assert_eq!(
        result["error"]["details"]["limit_kind"],
        "wire_response_bytes"
    );
    let no_record = h
        .call(
            "get_operation",
            json!({"operation_key":"large-as-other-principal"}),
        )
        .await;
    assert!(no_record["data"]["operation"].is_null());
}

#[tokio::test]
async fn handler_contracts_cover_diagnostic_failures_and_collection_wire_compression() {
    let mut closed = Harness::new(BTreeMap::new(), None, 8192).await;
    closed.imap.abort();
    let _ = (&mut closed.imap).await;
    let failed = closed.call("diagnose_connectivity", json!({})).await;
    assert_eq!(failed["data"]["ok"], false);
    assert_eq!(failed["data"]["failure"], "connection_check_failed");
    assert_eq!(
        failed["data"]["stages"].as_array().unwrap().last().unwrap()["status"],
        "failed"
    );
    let stalled = Harness::configured(BTreeMap::new(), None, 8192, true).await;
    let timeout = stalled.call("diagnose_connectivity", json!({})).await;
    assert_eq!(timeout["data"]["ok"], false);
    assert_eq!(timeout["data"]["failure"], "timeout");
    let body = "中😀\"\\\n".repeat(12000);
    let h = Harness::new(
        BTreeMap::from([(7, raw(7, &body)), (8, raw(8, &body)), (9, raw(9, &body))]),
        None,
        12000,
    )
    .await;
    for (name, args, path) in [
        (
            "get_emails",
            json!({"uids":[7,8,9],"uid_validity":9,"html_display":"readable"}),
            "/data/items",
        ),
        (
            "read_conversation",
            json!({"uid":7,"uid_validity":9,"html_display":"readable"}),
            "/data/messages",
        ),
    ] {
        let content = h.call_id(name, args, json!("\\\"中".repeat(60))).await;
        let items = content.pointer(path).unwrap().as_array().unwrap();
        assert_eq!(items.len(), 3);
        for (index, item) in items.iter().enumerate() {
            let message = item.get("email").unwrap_or(item);
            assert_eq!(message["body_truncated"], true);
            if name == "get_emails" || index == 0 {
                assert!(!message["body"].as_str().unwrap().is_empty());
                assert!(
                    message["body"].as_str().unwrap().len()
                        < message["body_byte_limit"].as_u64().unwrap() as usize
                );
            } else {
                // The conversation's shared display budget was already spent
                // on its first body; wire fitting keeps these omission flags.
                assert_eq!(message["body_omitted"], true);
            }
            assert!(body.starts_with(message["body"].as_str().unwrap()));
            assert_eq!(
                message["html_display_metadata"]["format_version"],
                "readable-v1"
            );
            assert_eq!(message["html_display_metadata"]["html_parts"], 0);
        }
    }
}

#[test]
fn read_output_schemas_are_closed_and_reject_broken_discriminants_and_nullability() {
    for tool in tools("full") {
        assert_eq!(tool.output_schema.is_some(), is_read(&tool.name));
        if is_read(&tool.name) {
            validator(&tool.name);
        }
    }
    let batch = validator("get_emails");
    for broken in [
        json!({"data":{"items":[{"uid":7,"ok":true,"status":"unprocessed"}],"requested":1,"completed":0},"request_id":"r"}),
        json!({"data":{"items":[{"uid":7,"ok":false,"email":{}}],"requested":1,"completed":0},"request_id":"r"}),
        json!({"data":{},"request_id":"r"}),
        json!({"data":null,"request_id":"r"}),
    ] {
        assert!(!batch.is_valid(&broken), "{broken}");
    }
    let operations = validator("get_operation");
    assert!(operations.is_valid(&json!({"data":{"operation":null},"request_id":"r"})));
    assert!(
        !operations.is_valid(&json!({"data":{"operation":{"status":"accepted"}},"request_id":"r"}))
    );
    assert!(
        !operations.is_valid(&json!({"data":{"operation":null},"request_id":"r","extra":true}))
    );
}
