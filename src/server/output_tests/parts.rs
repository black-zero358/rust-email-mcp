//! Target-part reads through signed HTTP with local TLS IMAP and synthetic bytes.
use super::*;
use axum::http::{StatusCode, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::sync::{Mutex, atomic::Ordering};

const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub(super) enum Fault {
    WrongOrigin,
    WrongSection,
    Duplicate,
    Missing,
    Short,
    Extra,
    Oversized,
    No,
    Bad,
    Eof,
    WrongTag,
    EofAfterData,
    NoAfterData,
    BadAfterData,
    WrongTagAfterData,
    ByeAfterData,
}

#[derive(Default)]
pub(super) struct Protocol {
    bodies: Mutex<BTreeMap<(u32, String), Vec<u8>>>,
    eof_bytes: Mutex<BTreeMap<(u32, String), u8>>,
    faults: Mutex<BTreeMap<usize, Fault>>,
    requests: Mutex<Vec<(usize, usize)>>,
}

impl Protocol {
    pub(super) fn fail_at(&self, offset: usize, fault: Fault) {
        self.faults.lock().unwrap().insert(offset, fault);
    }
    pub(super) fn install_body(&self, uid: u32, section: &str, bytes: &[u8]) {
        self.bodies
            .lock()
            .unwrap()
            .insert((uid, section.into()), bytes.to_vec());
    }
    pub(super) fn install_eof_byte(&self, uid: u32, section: &str, byte: u8) {
        self.eof_bytes
            .lock()
            .unwrap()
            .insert((uid, section.into()), byte);
    }
    pub(super) fn completion(&self, fetch: &str, tag: &str) -> Option<Vec<u8>> {
        let offset = fetch
            .rsplit_once('<')?
            .1
            .split_once('.')?
            .0
            .parse::<usize>()
            .ok()?;
        match self.faults.lock().unwrap().get(&offset)? {
            Fault::EofAfterData => Some(Vec::new()),
            Fault::NoAfterData => {
                Some(format!("{tag} NO synthetic refusal after literal\r\n").into_bytes())
            }
            Fault::BadAfterData => {
                Some(format!("{tag} BAD synthetic refusal after literal\r\n").into_bytes())
            }
            Fault::WrongTagAfterData => {
                Some(b"wrong-tag OK synthetic completion after literal\r\n".to_vec())
            }
            Fault::ByeAfterData => Some(b"* BYE synthetic disconnect after literal\r\n".to_vec()),
            _ => None,
        }
    }
    pub(super) fn fetch(&self, fetch: &str) -> Option<structure::Reply> {
        let (uid, attrs) = fetch.split_once(' ')?;
        let uid = uid.parse::<u32>().ok()?;
        let (section, range) = attrs.strip_prefix("(UID BODY.PEEK[")?.split_once("]<")?;
        if section != "TEXT"
            && !section
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        {
            return None;
        }
        let (offset, count) = range.strip_suffix(">)")?.split_once('.')?;
        let offset = offset.parse::<usize>().ok()?;
        let count = count.parse::<usize>().ok()?;
        let key = (uid, section.into());
        let bodies = self.bodies.lock().unwrap();
        let body = bodies.get(&key)?;
        self.requests.lock().unwrap().push((offset, count));
        let fault = self.faults.lock().unwrap().get(&offset).copied();
        match fault {
            Some(Fault::No) => return Some(structure::Reply::No),
            Some(Fault::Bad) => return Some(structure::Reply::Bad),
            Some(Fault::Eof) => return Some(structure::Reply::Eof),
            Some(Fault::WrongTag) => return Some(structure::Reply::WrongTag),
            _ => {}
        }
        let mut bytes = body
            .get(offset..)
            .unwrap_or_default()
            .iter()
            .take(count)
            .copied()
            .collect::<Vec<_>>();
        if offset == body.len()
            && count == 1
            && let Some(byte) = self.eof_bytes.lock().unwrap().get(&key)
        {
            bytes.push(*byte);
        }
        match fault {
            Some(Fault::Short) => {
                bytes.pop();
            }
            Some(Fault::Extra) => bytes.push(b'x'),
            Some(Fault::Oversized) => bytes = vec![b'x'; count + 1],
            _ => {}
        }
        let origin = offset + usize::from(matches!(fault, Some(Fault::WrongOrigin)));
        let section = if matches!(fault, Some(Fault::WrongSection)) {
            "99"
        } else {
            section
        };
        let mut reply = b"* 9 FETCH (UID 999 FLAGS (\\Seen))\r\n".to_vec();
        reply.extend_from_slice(b"* 9 FETCH (UID 999 BODY[HEADER.FIELDS (SUBJECT)]<0> \"\" BODY[HEADER.FIELDS.NOT (DATE)]<0> \"\" BODY[1.HEADER.FIELDS (FROM TO)]<0> \"\" BODY[1.HEADER.FIELDS.NOT (\"X-SYNTHETIC\")]<0> \"\")\r\n");
        reply.extend_from_slice(format!("* 1 FETCH (UID {uid} FLAGS ())\r\n").as_bytes());
        if !matches!(fault, Some(Fault::Missing)) {
            let copies = if matches!(fault, Some(Fault::Duplicate)) {
                2
            } else {
                1
            };
            for _ in 0..copies {
                reply.extend_from_slice(
                    format!(
                        "* 1 FETCH (UID {uid} BODY[{section}]<{origin}> {{{}}}\r\n",
                        bytes.len()
                    )
                    .as_bytes(),
                );
                reply.extend_from_slice(&bytes);
                reply.extend_from_slice(b")\r\n");
            }
        }
        Some(structure::Reply::Data(reply))
    }
}

fn raw_message(encoding: &str, bytes: &[u8]) -> Vec<u8> {
    let mut raw = b"Message-ID: <r07@example.test>\r\nContent-Type: multipart/mixed; boundary=r07\r\n\r\n--r07\r\nContent-Type: text/plain\r\n\r\nhello\r\n--r07\r\n".to_vec();
    raw.extend_from_slice(&part_header(encoding));
    raw.extend_from_slice(bytes);
    raw.extend_from_slice(b"\r\n--r07--\r\n");
    raw
}

fn part_header(encoding: &str) -> Vec<u8> {
    format!("Content-Type: application/octet-stream; name=part.bin\r\nContent-Disposition: attachment; filename=part.bin\r\nContent-Transfer-Encoding: {encoding}\r\n\r\n").into_bytes()
}

fn install(h: &Harness, encoding: &str, encoded: &[u8], whole_size: Option<usize>) {
    let structure = format!(
        r#"(("TEXT" "PLAIN" NIL NIL NIL "7BIT" 5 1 NIL NIL NIL NIL)("APPLICATION" "OCTET-STREAM" ("NAME" "part.bin") NIL NIL "{encoding}" {} NIL NIL NIL NIL) "MIXED" ("BOUNDARY" "r07") NIL NIL NIL)"#,
        encoded.len()
    );
    h.structure.structures.lock().unwrap().insert(
        7,
        structure::Reply::Data(
            format!("* 1 FETCH (UID 7 BODYSTRUCTURE {structure})\r\n").into_bytes(),
        ),
    );
    h.structure.headers.lock().unwrap().extend([
        (
            (7, "1.MIME".into()),
            structure::Evidence::Bytes(b"Content-Type: text/plain\r\n\r\n".to_vec()),
        ),
        (
            (7, "2.MIME".into()),
            structure::Evidence::Bytes(part_header(encoding)),
        ),
    ]);
    if let Some(size) = whole_size {
        h.structure.sizes.lock().unwrap().insert(
            7,
            structure::Reply::Data(
                format!("* 1 FETCH (UID 7 RFC822.SIZE {size})\r\n").into_bytes(),
            ),
        );
    }
    h.parts
        .bodies
        .lock()
        .unwrap()
        .insert((7, "2".into()), encoded.to_vec());
}

fn signed(h: &Harness) -> (Router, String) {
    let credentials = attachments::Credentials::new(h.service.config().clone());
    (
        attachments::router(h.service.clone(), credentials.auth.clone()),
        credentials.token(),
    )
}

async fn prepare(app: &Router, token: &str) -> Value {
    attachments::call(
        app,
        token,
        "prepare_attachment_download",
        json!({"uid":7,"uid_validity":9,"attachment_index":1}),
    )
    .await
}

fn raw_fetches(h: &Harness) -> usize {
    h.structure
        .commands
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, cmd)| cmd.contains("BODY.PEEK[]"))
        .count()
}

async fn bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), 33 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}

async fn wait_files(h: &Harness, expected: usize) {
    let root = h._staging.path().join("mail-downloads");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let files = std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("attachment-v1-")
                        && (name.ends_with(".part") || name.ends_with(".ready"))
                })
                .count();
            if files == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("attachment file ownership did not settle");
}

#[test]
fn attachment_output_schema_accepts_specific_new_error_classes_and_rejects_unknown_limits() {
    for kind in [
        "connection_bytes",
        "part_fetch_bytes",
        "decoded_attachment_bytes",
    ] {
        assert_contract(
            "prepare_attachment_download",
            &json!({"error":{"code":"LIMIT_EXCEEDED","message":"bounded","retryable":false,"details":{"limit_kind":kind}},"request_id":"synthetic"}),
        );
    }
    assert_contract(
        "prepare_attachment_download",
        &json!({"error":{"code":"ATTACHMENT_STORAGE_UNAVAILABLE","message":"unavailable","retryable":false},"request_id":"synthetic"}),
    );
    let schema = validator("prepare_attachment_download");
    assert!(!schema.is_valid(&json!({"error":{"code":"LIMIT_EXCEEDED","message":"bounded","retryable":false,"details":{"limit_kind":"encoded_part_bytes"}},"request_id":"synthetic"})));
}

#[tokio::test]
async fn signed_target_parts_preserve_bytes_at_chunk_boundaries_and_empty_eof() {
    for length in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, CHUNK * 2 + 13] {
        for encoding in ["7bit", "base64", "quoted-printable"] {
            let plain = vec![b'x'; length];
            let encoded = match encoding {
                "base64" => STANDARD.encode(&plain).into_bytes(),
                // Place escapes directly across multiple 64 KiB boundaries.
                "quoted-printable" => plain.iter().flat_map(|_| *b"=78").collect(),
                _ => plain.clone(),
            };
            let h = Harness::new(
                BTreeMap::from([(7, raw_message(encoding, &encoded))]),
                None,
                64 * 1024,
            )
            .await;
            install(&h, encoding, &encoded, None);
            let (app, token) = signed(&h);
            let prepared = prepare(&app, &token).await;
            assert_eq!(
                prepared["data"]["read_diagnostics"]["source"], "bodystructure",
                "{prepared}"
            );
            let response = attachments::get_attachment(
                &app,
                Some(&token),
                prepared["data"]["download_url"].as_str().unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{encoding}/{length}");
            assert_eq!(
                response.headers()[header::CONTENT_LENGTH].to_str().unwrap(),
                plain.len().to_string()
            );
            assert_eq!(
                response.headers()["x-content-sha256"].to_str().unwrap(),
                format!("{:x}", Sha256::digest(&plain))
            );
            assert_eq!(bytes(response).await, plain, "{encoding}/{length}");
            assert_eq!(raw_fetches(&h), 0);
            let requests = h.parts.requests.lock().unwrap();
            assert_eq!(requests.last(), Some(&(encoded.len(), 1)));
            assert!(
                requests[..requests.len() - 1]
                    .iter()
                    .all(|(_, count)| *count == CHUNK)
            );
        }
    }
}

#[tokio::test]
async fn a_small_part_downloads_from_a_message_above_the_whole_message_budget() {
    let h = Harness::new(
        BTreeMap::from([(7, raw_message("base64", b"YWJj"))]),
        None,
        64 * 1024,
    )
    .await;
    install(&h, "base64", b"YWJj", Some(40 * 1024 * 1024));
    let (app, token) = signed(&h);
    let prepared = prepare(&app, &token).await;
    assert_eq!(
        prepared["data"]["metadata"]["delivery"]["available_modes"],
        json!(["authenticated_http"])
    );
    let response = attachments::get_attachment(
        &app,
        Some(&token),
        prepared["data"]["download_url"].as_str().unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, b"abc");
    assert_eq!(raw_fetches(&h), 0);
}

#[tokio::test]
async fn target_part_protocol_failures_are_private_and_never_fall_back_to_raw() {
    for fault in [
        Fault::WrongOrigin,
        Fault::WrongSection,
        Fault::Duplicate,
        Fault::Missing,
        Fault::Short,
        Fault::Extra,
        Fault::No,
        Fault::Bad,
        Fault::Eof,
        Fault::WrongTag,
        Fault::EofAfterData,
        Fault::NoAfterData,
        Fault::BadAfterData,
        Fault::WrongTagAfterData,
    ] {
        let h = Harness::new(
            BTreeMap::from([(7, raw_message("7bit", b"synthetic-private-content"))]),
            None,
            64 * 1024,
        )
        .await;
        install(&h, "7bit", b"synthetic-private-content", None);
        let (app, token) = signed(&h);
        let prepared = prepare(&app, &token).await;
        h.parts.faults.lock().unwrap().insert(0, fault);
        let before = h.connections.load(Ordering::SeqCst);
        let response = attachments::get_attachment(
            &app,
            Some(&token),
            prepared["data"]["download_url"].as_str().unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.headers().get("x-content-sha256").is_none());
        let body = bytes(response).await;
        assert!(body.len() < 2048);
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "MAIL_BACKEND");
        let text = String::from_utf8(body).unwrap();
        assert!(!text.contains("synthetic-private-content"));
        assert!(!text.contains("BODY["));
        assert_eq!(raw_fetches(&h), 0);
        assert_eq!(h.connections.load(Ordering::SeqCst), before + 1);
    }
}

#[tokio::test]
async fn eof_probe_refuses_extra_bytes_and_per_command_oversize_is_a_typed_limit() {
    for (offset, fault, status, code) in [
        (3, Fault::Extra, StatusCode::BAD_GATEWAY, "MAIL_BACKEND"),
        (
            0,
            Fault::Oversized,
            StatusCode::PAYLOAD_TOO_LARGE,
            "LIMIT_EXCEEDED",
        ),
    ] {
        let h = Harness::new(
            BTreeMap::from([(7, raw_message("7bit", b"abc"))]),
            None,
            64 * 1024,
        )
        .await;
        install(&h, "7bit", b"abc", None);
        let (app, token) = signed(&h);
        let prepared = prepare(&app, &token).await;
        h.parts.faults.lock().unwrap().insert(offset, fault);
        let response = attachments::get_attachment(
            &app,
            Some(&token),
            prepared["data"]["download_url"].as_str().unwrap(),
        )
        .await;
        assert_eq!(response.status(), status);
        let error: Value = serde_json::from_slice(&bytes(response).await).unwrap();
        assert_eq!(error["error"]["code"], code);
        if code == "LIMIT_EXCEEDED" {
            assert_eq!(error["error"]["limit_kind"], "part_fetch_bytes");
        }
        assert_eq!(raw_fetches(&h), 0);
    }
}

#[tokio::test]
async fn target_get_rechecks_uidvalidity_and_subject_before_reading_any_part() {
    let h = Harness::new(
        BTreeMap::from([(7, raw_message("7bit", b"abc"))]),
        None,
        64 * 1024,
    )
    .await;
    install(&h, "7bit", b"abc", None);
    let credentials = attachments::Credentials::new(h.service.config().clone());
    let app = attachments::router(h.service.clone(), credentials.auth.clone());
    let token = credentials.token();
    let prepared = prepare(&app, &token).await;
    let url = prepared["data"]["download_url"].as_str().unwrap();
    let other = credentials.changed("sub", json!("other"));
    let before = h.connections.load(Ordering::SeqCst);
    let response = attachments::get_attachment(&app, Some(&other), url).await;
    // The configured principal allowlist rejects this identity at authentication.
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(h.connections.load(Ordering::SeqCst), before);
    h.structure.reconnect_validity.store(10, Ordering::SeqCst);
    let response = attachments::get_attachment(&app, Some(&token), url).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error: Value = serde_json::from_slice(&bytes(response).await).unwrap();
    assert_eq!(error["error"]["code"], "STALE_UID");
    assert!(h.parts.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn four_unconsumed_target_downloads_release_files_and_capacity_on_drop() {
    let encoded = vec![b'x'; CHUNK * 2 + 1];
    let h = Harness::new(
        BTreeMap::from([(7, raw_message("7bit", &encoded))]),
        None,
        64 * 1024,
    )
    .await;
    install(&h, "7bit", &encoded, None);
    let (app, token) = signed(&h);
    let prepared = prepare(&app, &token).await;
    let url = prepared["data"]["download_url"].as_str().unwrap();
    let mut responses = Vec::new();
    for _ in 0..4 {
        let response = attachments::get_attachment(&app, Some(&token), url).await;
        assert_eq!(response.status(), StatusCode::OK);
        responses.push(response);
    }
    let response = attachments::get_attachment(&app, Some(&token), url).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    drop(responses.pop());
    wait_files(&h, 3).await;
    let response = attachments::get_attachment(&app, Some(&token), url).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, encoded);
    drop(responses);
    wait_files(&h, 0).await;
    let response = attachments::get_attachment(&app, Some(&token), url).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, encoded);
}

#[tokio::test]
async fn a_late_command_failure_discards_previously_decoded_staged_bytes() {
    let encoded = vec![b'x'; CHUNK + 3];
    for fault in [
        Fault::No,
        Fault::Bad,
        Fault::Eof,
        Fault::WrongTag,
        Fault::EofAfterData,
        Fault::NoAfterData,
        Fault::BadAfterData,
        Fault::WrongTagAfterData,
    ] {
        let h = Harness::new(
            BTreeMap::from([(7, raw_message("7bit", &encoded))]),
            None,
            64 * 1024,
        )
        .await;
        install(&h, "7bit", &encoded, None);
        let (app, token) = signed(&h);
        let prepared = prepare(&app, &token).await;
        h.parts.faults.lock().unwrap().insert(CHUNK, fault);
        let response = attachments::get_attachment(
            &app,
            Some(&token),
            prepared["data"]["download_url"].as_str().unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.headers().get("x-content-sha256").is_none());
        let body = bytes(response).await;
        assert!(body.len() < 2048);
        assert_eq!(
            h.parts.requests.lock().unwrap().as_slice(),
            &[(0, CHUNK), (CHUNK, CHUNK)]
        );
        assert_eq!(raw_fetches(&h), 0);
        wait_files(&h, 0).await;
    }
}

#[tokio::test]
async fn malformed_transfer_and_decoded_overflow_return_private_complete_errors() {
    for (encoding, encoded, status, code) in [
        (
            "base64",
            b"YWJ!".to_vec(),
            StatusCode::BAD_GATEWAY,
            "ATTACHMENT_DECODE_FAILED",
        ),
        (
            "quoted-printable",
            vec![b'x'; 128 * 1024 + 1],
            StatusCode::PAYLOAD_TOO_LARGE,
            "LIMIT_EXCEEDED",
        ),
    ] {
        let h = Harness::with_limits(
            BTreeMap::from([(7, raw_message(encoding, &encoded))]),
            None,
            64 * 1024,
            false,
            None,
            Some(128 * 1024),
        )
        .await;
        install(&h, encoding, &encoded, None);
        let (app, token) = signed(&h);
        let prepared = prepare(&app, &token).await;
        let response = attachments::get_attachment(
            &app,
            Some(&token),
            prepared["data"]["download_url"].as_str().unwrap(),
        )
        .await;
        assert_eq!(response.status(), status);
        let body = bytes(response).await;
        assert!(body.len() < 2048);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], code);
        if code == "LIMIT_EXCEEDED" {
            assert_eq!(value["error"]["limit_kind"], "decoded_attachment_bytes");
        }
        assert_eq!(raw_fetches(&h), 0);
    }
}

#[tokio::test]
async fn replaced_staging_root_returns_the_fixed_storage_error_without_a_file() {
    let h = Harness::new(
        BTreeMap::from([(7, raw_message("7bit", b"abc"))]),
        None,
        64 * 1024,
    )
    .await;
    install(&h, "7bit", b"abc", None);
    let (app, token) = signed(&h);
    let prepared = prepare(&app, &token).await;
    let root = h._staging.path().join("mail-downloads");
    std::fs::rename(&root, h._staging.path().join("previous-root")).unwrap();
    std::fs::write(&root, b"synthetic replacement").unwrap();
    let response = attachments::get_attachment(
        &app,
        Some(&token),
        prepared["data"]["download_url"].as_str().unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let value: Value = serde_json::from_slice(&bytes(response).await).unwrap();
    assert_eq!(value["error"]["code"], "ATTACHMENT_STORAGE_UNAVAILABLE");
    assert_eq!(value["error"]["phase"], "staging");
    assert_eq!(value["error"]["action"], "contact_operator");
    assert!(h.parts.requests.lock().unwrap().is_empty());
}
