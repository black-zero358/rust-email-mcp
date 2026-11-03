//! Controlled BODYSTRUCTURE/MIME evidence for the real signed HTTP handler.
use super::*;
use std::sync::Mutex;

#[derive(Clone)]
pub(super) enum Reply {
    Data(Vec<u8>),
    No,
    Bad,
    Eof,
    WrongTag,
}
#[derive(Clone)]
pub(super) enum Evidence {
    Bytes(Vec<u8>),
    No,
    Eof,
    WrongTag,
    Duplicate(Vec<u8>),
    WrongOrigin(Vec<u8>),
    Missing,
}
#[derive(Default)]
pub(super) struct Protocol {
    pub(super) structures: Mutex<BTreeMap<u32, Reply>>,
    pub(super) headers: Mutex<BTreeMap<(u32, String), Evidence>>,
    pub(super) sizes: Mutex<BTreeMap<u32, Reply>>,
    pub(super) commands: Mutex<Vec<(usize, String)>>,
    pub(super) reconnect_validity: std::sync::atomic::AtomicU32,
    noise: std::sync::atomic::AtomicBool,
}
impl Protocol {
    pub(super) fn validity(&self, connection: usize) -> u32 {
        let changed = self
            .reconnect_validity
            .load(std::sync::atomic::Ordering::SeqCst);
        if connection > 1 && changed != 0 {
            changed
        } else {
            9
        }
    }
    pub(super) fn record(&self, connection: usize, command: &str) {
        if !command.starts_with("LOGIN ") {
            self.commands
                .lock()
                .unwrap()
                .push((connection, command.into()));
        }
    }
    pub(super) fn fetch(&self, fetch: &str, messages: &BTreeMap<u32, Vec<u8>>) -> Option<Reply> {
        let (set, attrs) = fetch.split_once(' ')?;
        let uid = set.parse::<u32>().ok()?;
        if attrs.contains("BODYSTRUCTURE") {
            return Some(
                self.structures
                    .lock()
                    .unwrap()
                    .get(&uid)
                    .cloned()
                    .unwrap_or(Reply::No),
            );
        }
        if attrs == "(UID RFC822.SIZE)" {
            return self.sizes.lock().unwrap().get(&uid).cloned();
        }
        if !attrs.contains("BODY.PEEK[") || !attrs.contains("<0.") || attrs.contains("BODY.PEEK[]")
        {
            return None;
        }
        let raw = messages.get(&uid)?;
        let mut reply = Vec::new();
        if self.noise.load(std::sync::atomic::Ordering::SeqCst) {
            reply.extend_from_slice(b"* 9 FETCH (UID 99 FLAGS (\\Seen))\r\n");
            reply.extend_from_slice(format!("* 1 FETCH (UID {uid} FLAGS ())\r\n").as_bytes());
        }
        reply.extend_from_slice(format!("* 1 FETCH (UID {uid}").as_bytes());
        for request in attrs.split("BODY.PEEK[").skip(1) {
            let section = request.split_once(']')?.0;
            let evidence = self
                .headers
                .lock()
                .unwrap()
                .get(&(uid, section.into()))
                .cloned()
                .unwrap_or_else(|| {
                    if section == "HEADER" {
                        Evidence::Bytes(header(raw).into())
                    } else {
                        Evidence::Missing
                    }
                });
            let (bytes, copies, origin) = match evidence {
                Evidence::No => return Some(Reply::No),
                Evidence::Eof => return Some(Reply::Eof),
                Evidence::WrongTag => return Some(Reply::WrongTag),
                Evidence::Missing => continue,
                Evidence::Bytes(bytes) => (bytes, 1, 0),
                Evidence::Duplicate(bytes) => (bytes, 2, 0),
                Evidence::WrongOrigin(bytes) => (bytes, 1, 1),
            };
            for _ in 0..copies {
                reply.extend_from_slice(
                    format!(" BODY[{section}]<{origin}> {{{}}}\r\n", bytes.len()).as_bytes(),
                );
                reply.extend_from_slice(&bytes);
            }
        }
        reply.extend_from_slice(b")\r\n");
        Some(Reply::Data(reply))
    }
    fn commands(&self) -> Vec<(usize, String)> {
        self.commands.lock().unwrap().clone()
    }
}

fn raw_fetches(h: &Harness) -> usize {
    h.structure
        .commands()
        .iter()
        .filter(|(_, command)| command.contains("BODY.PEEK[]"))
        .count()
}
fn structure_fetches(h: &Harness) -> usize {
    h.structure
        .commands()
        .iter()
        .filter(|(_, command)| command.contains("BODYSTRUCTURE"))
        .count()
}
fn signed(h: &Harness) -> (Router, String) {
    let credentials = attachments::Credentials::new(h.service.config().clone());
    (
        attachments::router(h.service.clone(), credentials.auth.clone()),
        credentials.token(),
    )
}

fn message() -> Vec<u8> {
    b"Message-ID: <structure@example.test>\r\nContent-Type: multipart/mixed; boundary=r06\r\n\r\n--r06\r\nContent-Type: text/plain\r\n\r\nhello\r\n--r06\r\nContent-Type: application/octet-stream; name=same.txt\r\nContent-Disposition: attachment; filename=same.txt\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--r06--\r\n".to_vec()
}
fn attachment_header() -> Vec<u8> {
    b"Content-Type: application/octet-stream; name=same.txt\r\nContent-Disposition: attachment; filename=same.txt\r\nContent-Transfer-Encoding: base64\r\n\r\n".to_vec()
}
fn body_structure() -> String {
    r#"(("TEXT" "PLAIN" NIL NIL NIL "7BIT" 5 1 NIL NIL NIL NIL)("APPLICATION" "OCTET-STREAM" ("NAME" "same.txt") NIL NIL "BASE64" 4 NIL NIL NIL NIL) "MIXED" ("BOUNDARY" "r06") NIL NIL NIL)"#.into()
}
fn install(h: &Harness) {
    h.parts.install_body(7, "2", b"YWJj");
    h.structure.structures.lock().unwrap().insert(
        7,
        Reply::Data(
            format!("* 1 FETCH (UID 7 BODYSTRUCTURE {})\r\n", body_structure()).into_bytes(),
        ),
    );
    h.structure.headers.lock().unwrap().extend([
        (
            (7, "1.MIME".into()),
            Evidence::Bytes(b"Content-Type: text/plain\r\n\r\n".to_vec()),
        ),
        ((7, "2.MIME".into()), Evidence::Bytes(attachment_header())),
    ]);
}

#[tokio::test]
async fn diagnostics_schema_is_optional_closed_and_specific_to_list_and_prepare() {
    let h = Harness::new(BTreeMap::from([(7, message())]), None, 64 * 1024).await;
    for (name, args) in [
        ("list_attachments", json!({"uid":7,"uid_validity":9})),
        (
            "prepare_attachment_download",
            json!({"uid":7,"uid_validity":9,"attachment_index":1}),
        ),
    ] {
        let mut result = h.call(name, args).await;
        result["data"]
            .as_object_mut()
            .unwrap()
            .remove("read_diagnostics");
        assert_contract(name, &result);
        for reason in [
            Value::Null,
            json!("structure_limit"),
            json!("structure_rejected"),
            json!("mapping_unverified"),
            json!("metadata_conflict"),
            json!("whole_multipart_attachment"),
            json!("header_budget"),
        ] {
            result["data"]["read_diagnostics"] =
                json!({"source":"raw","fallback_reason":reason,"limit_kind":"mime_structure"});
            assert_contract(name, &result);
        }
        for bad in [
            json!({}),
            json!({"source":"other","fallback_reason":null,"limit_kind":null}),
            json!({"source":"raw","fallback_reason":"private-value","limit_kind":null}),
            json!({"source":"raw","fallback_reason":null,"limit_kind":"raw_message_bytes"}),
            json!({"source":"raw","fallback_reason":null,"limit_kind":null,"extra":true}),
        ] {
            result["data"]["read_diagnostics"] = bad;
            assert!(!validator(name).is_valid(&result));
        }
    }
    let mut inline = h
        .call(
            "download_attachment",
            json!({"uid":7,"uid_validity":9,"attachment_index":1}),
        )
        .await;
    inline["data"]["read_diagnostics"] =
        json!({"source":"raw","fallback_reason":null,"limit_kind":null});
    assert!(!validator("download_attachment").is_valid(&inline));
}

#[tokio::test]
async fn signed_structure_fast_path_reads_only_metadata_and_keeps_raw_attachment_identity() {
    let h = Harness::new(BTreeMap::from([(7, message())]), None, 64 * 1024).await;
    install(&h);
    h.structure
        .noise
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (app, token) = signed(&h);
    let listed = attachments::call(
        &app,
        &token,
        "list_attachments",
        json!({"uid":7,"uid_validity":9}),
    )
    .await;
    assert_eq!(
        listed["data"]["read_diagnostics"],
        json!({"source":"bodystructure","fallback_reason":null,"limit_kind":null})
    );
    let prepared = attachments::call(&app, &token, "prepare_attachment_download", json!({"uid":7,"uid_validity":9,"attachment_id":listed["data"]["attachments"][0]["attachment_id"]})).await;
    assert_eq!(
        prepared["data"]["read_diagnostics"],
        listed["data"]["read_diagnostics"]
    );
    assert_eq!(raw_fetches(&h), 0);
    let commands = h.structure.commands();
    assert!(
        commands
            .iter()
            .any(|(_, command)| command.contains("BODY.PEEK[HEADER]"))
    );
    assert!(
        commands
            .iter()
            .any(|(_, command)| command.contains("BODY.PEEK[2.MIME]"))
    );
    assert_eq!(structure_fetches(&h), 2);
    let inline = attachments::call(
        &app,
        &token,
        "download_attachment",
        json!({"uid":7,"uid_validity":9,"attachment_index":1}),
    )
    .await;
    assert_eq!(
        inline["data"]["attachment_id"],
        listed["data"]["attachments"][0]["attachment_id"]
    );
    assert_eq!(inline["data"]["base64"], "YWJj");
    assert_eq!(
        inline["data"]["checksum"]["value"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let get = attachments::get_attachment(
        &app,
        Some(&token),
        prepared["data"]["download_url"].as_str().unwrap(),
    )
    .await;
    assert_eq!(get.status(), axum::http::StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(get.into_body(), 1024).await.unwrap(),
        b"abc".as_slice()
    );
    assert_eq!(raw_fetches(&h), 1);
    let commands = h.structure.commands();
    assert!(
        commands
            .iter()
            .any(|(_, command)| command == "UID FETCH 7 (UID BODY.PEEK[2]<0.65536>)")
    );
    assert!(
        commands
            .iter()
            .any(|(_, command)| command == "UID FETCH 7 (UID BODY.PEEK[2]<4.1>)")
    );
    assert_eq!(
        h.call("list_accounts", json!({})).await["data"]["accounts"][0]["capabilities"]["part_fetch"],
        false
    );
}

#[tokio::test]
async fn rejected_or_over_budget_structure_falls_back_once_on_a_new_connection() {
    let mut deep = r#"("TEXT" "PLAIN" NIL NIL NIL "7BIT" 1 1)"#.to_owned();
    for _ in 0..32 {
        deep = format!("({deep} \"MIXED\")");
    }
    for (reply, reason, limit) in [
        (Reply::No, "structure_rejected", Value::Null),
        (Reply::Bad, "structure_rejected", Value::Null),
        (
            Reply::Data(format!("* 1 FETCH (UID 7 BODYSTRUCTURE {deep})\r\n").into_bytes()),
            "structure_limit",
            json!("mime_structure"),
        ),
    ] {
        let h = Harness::new(BTreeMap::from([(7, message())]), None, 64 * 1024).await;
        h.structure.structures.lock().unwrap().insert(7, reply);
        let (app, token) = signed(&h);
        let listed = attachments::call(
            &app,
            &token,
            "list_attachments",
            json!({"uid":7,"uid_validity":9}),
        )
        .await;
        assert_eq!(
            listed["data"]["read_diagnostics"],
            json!({"source":"raw","fallback_reason":reason,"limit_kind":limit})
        );
        assert_eq!(h.connections.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(structure_fetches(&h), 1);
        assert_eq!(raw_fetches(&h), 1);
        let commands = h.structure.commands();
        let raw_connection = commands
            .iter()
            .find(|(_, command)| command.contains("BODY.PEEK[]"))
            .unwrap()
            .0;
        let structure_connection = commands
            .iter()
            .find(|(_, command)| command.contains("BODYSTRUCTURE"))
            .unwrap()
            .0;
        assert_ne!(raw_connection, structure_connection);
    }
}

#[tokio::test]
async fn protocol_failures_and_unknown_or_oversized_size_never_fall_back() {
    for reply in [Reply::Eof, Reply::WrongTag] {
        let h = Harness::new(BTreeMap::from([(7, message())]), None, 64 * 1024).await;
        h.structure.structures.lock().unwrap().insert(7, reply);
        let result = h
            .call("list_attachments", json!({"uid":7,"uid_validity":9}))
            .await;
        assert!(result.get("error").is_some());
        assert_eq!(raw_fetches(&h), 0);
        assert_eq!(h.connections.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    for size in [None, Some(32 * 1024 * 1024 + 1)] {
        let h = Harness::new(BTreeMap::from([(7, message())]), None, 64 * 1024).await;
        h.structure.sizes.lock().unwrap().insert(
            7,
            Reply::Data(match size {
                Some(size) => format!("* 1 FETCH (UID 7 RFC822.SIZE {size})\r\n").into_bytes(),
                None => b"* 1 FETCH (UID 7)\r\n".to_vec(),
            }),
        );
        let result = h
            .call("list_attachments", json!({"uid":7,"uid_validity":9}))
            .await;
        assert!(result.get("error").is_some());
        assert_eq!(structure_fetches(&h), usize::from(size.is_some()));
        if size.is_some() {
            assert_eq!(
                result["error"]["details"]["limit_kind"],
                "raw_message_bytes"
            );
        }
        assert_eq!(raw_fetches(&h), 0);
        assert_eq!(h.connections.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn header_evidence_requires_matching_origin_presence_and_tagged_success() {
    for evidence in [
        Evidence::No,
        Evidence::Eof,
        Evidence::WrongTag,
        Evidence::Missing,
        Evidence::Duplicate(attachment_header()),
        Evidence::WrongOrigin(attachment_header()),
    ] {
        let h = Harness::new(BTreeMap::from([(7, message())]), None, 64 * 1024).await;
        install(&h);
        h.structure
            .headers
            .lock()
            .unwrap()
            .insert((7, "2.MIME".into()), evidence);
        let result = h
            .call(
                "prepare_attachment_download",
                json!({"uid":7,"uid_validity":9,"attachment_index":1}),
            )
            .await;
        assert!(result.get("error").is_some());
        assert_eq!(raw_fetches(&h), 0);
        assert_eq!(h.connections.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn header_budget_successful_fallback_retains_reason_and_raw_failure_is_not_retried() {
    let h = Harness::new(BTreeMap::from([(7, message())]), None, 64 * 1024).await;
    install(&h);
    h.structure.headers.lock().unwrap().insert(
        (7, "2.MIME".into()),
        Evidence::Bytes(vec![b'x'; 128 * 1024 + 1]),
    );
    let result = h
        .call(
            "prepare_attachment_download",
            json!({"uid":7,"uid_validity":9,"attachment_index":1}),
        )
        .await;
    assert_eq!(
        result["data"]["read_diagnostics"],
        json!({"source":"raw","fallback_reason":"header_budget","limit_kind":null})
    );
    assert_eq!(h.connections.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(raw_fetches(&h), 1);
    let h = Harness::new(BTreeMap::from([(7, message())]), Some(7), 64 * 1024).await;
    let result = h
        .call("list_attachments", json!({"uid":7,"uid_validity":9}))
        .await;
    assert!(result.get("error").is_some());
    assert_eq!(h.connections.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(raw_fetches(&h), 1);
}

#[tokio::test]
async fn raw_fallback_rechecks_uidvalidity_before_fetching_the_body() {
    let h = Harness::new(BTreeMap::from([(7, message())]), None, 64 * 1024).await;
    h.structure
        .reconnect_validity
        .store(10, std::sync::atomic::Ordering::SeqCst);
    let result = h
        .call("list_attachments", json!({"uid":7,"uid_validity":9}))
        .await;
    assert_eq!(result["error"]["code"], "STALE_UID");
    assert_eq!(h.connections.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(structure_fetches(&h), 1);
    assert_eq!(raw_fetches(&h), 0);
}

#[tokio::test]
async fn duplicate_original_mime_headers_preserve_raw_indices_and_download_bytes() {
    let raw = b"Content-Type: multipart/mixed; boundary=i\r\nContent-Type: application/octet-stream\r\nContent-Disposition: inline\r\nContent-Disposition: attachment; filename=wrong.bin\r\n\r\n--i\r\nContent-Type: text/plain\r\n\r\nNOT_ATTACHMENT\r\n--i\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=real.bin\r\n\r\nREAL_ATTACHMENT\r\n--i--\r\n".to_vec();
    let body_size = raw.len() - header(&raw).len();
    let h = Harness::new(BTreeMap::from([(7, raw)]), None, 64 * 1024).await;
    h.structure.structures.lock().unwrap().insert(7, Reply::Data(format!("* 1 FETCH (UID 7 BODYSTRUCTURE (\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"7BIT\" {body_size} NIL (\"ATTACHMENT\" (\"FILENAME\" \"wrong.bin\"))))\r\n").into_bytes()));
    let (app, token) = signed(&h);
    let listed = attachments::call(
        &app,
        &token,
        "list_attachments",
        json!({"uid":7,"uid_validity":9}),
    )
    .await;
    assert_eq!(
        listed["data"]["read_diagnostics"],
        json!({"source":"raw","fallback_reason":"metadata_conflict","limit_kind":null})
    );
    assert_eq!(listed["data"]["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"]["attachments"][0]["attachment_index"], 1);
    assert_eq!(listed["data"]["attachments"][0]["filename"], "real.bin");
    assert_eq!(h.connections.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(raw_fetches(&h), 1);
    let id = listed["data"]["attachments"][0]["attachment_id"].clone();
    let prepared = attachments::call(
        &app,
        &token,
        "prepare_attachment_download",
        json!({"uid":7,"uid_validity":9,"attachment_id":id}),
    )
    .await;
    assert_eq!(prepared["data"]["attachment_index"], 1);
    assert_eq!(prepared["data"]["attachment_id"], id);
    let inline = attachments::call(
        &app,
        &token,
        "download_attachment",
        json!({"uid":7,"uid_validity":9,"attachment_id":id}),
    )
    .await;
    let get = attachments::get_attachment(
        &app,
        Some(&token),
        prepared["data"]["download_url"].as_str().unwrap(),
    )
    .await;
    assert_eq!(get.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(get.into_body(), 1024).await.unwrap();
    assert_eq!(bytes, b"REAL_ATTACHMENT".as_slice());
    use base64::Engine;
    assert_eq!(
        inline["data"]["base64"],
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
}
