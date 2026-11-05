//! Independent canonical-body acceptance over signed MCP and local TLS IMAP.
use super::*;
use mailparse::MailHeaderMap;
use std::sync::atomic::Ordering;

const CHUNK: usize = 64 * 1024;

pub(super) struct Entity {
    headers: Vec<u8>,
    encoded: Vec<u8>,
    structure: String,
    children: Vec<Self>,
}
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
fn parameters(values: &BTreeMap<String, String>) -> String {
    if values.is_empty() {
        return "NIL".into();
    }
    format!(
        "({})",
        values
            .iter()
            .map(|(key, value)| format!("{} {}", quoted(key), quoted(value)))
            .collect::<Vec<_>>()
            .join(" ")
    )
}
impl Entity {
    pub(super) fn leaf(headers: &str, bytes: impl Into<Vec<u8>>) -> Self {
        let headers = format!("{headers}\r\n\r\n").into_bytes();
        let parsed = crate::mime::parse(&headers).unwrap();
        let (kind, subtype) = parsed.ctype.mimetype.split_once('/').unwrap();
        let encoding = parsed
            .headers
            .get_first_value("Content-Transfer-Encoding")
            .unwrap_or_else(|| "7bit".into());
        let cid = parsed
            .headers
            .get_first_value("Content-ID")
            .map_or_else(|| "NIL".into(), |value| quoted(&value));
        let disposition = parsed
            .headers
            .get_first_value("Content-Disposition")
            .map_or_else(
                || "NIL".into(),
                |value| {
                    let parsed = mailparse::parse_content_disposition(&value);
                    format!(
                        "({} {})",
                        quoted(value.split(';').next().unwrap().trim()),
                        parameters(&parsed.params)
                    )
                },
            );
        let encoded = bytes.into();
        let lines = if kind == "text" { " 1" } else { "" };
        let structure = format!(
            "({} {} {} {cid} NIL {} {}{lines} NIL {disposition} NIL NIL)",
            quoted(kind),
            quoted(subtype),
            parameters(&parsed.ctype.params),
            quoted(&encoding),
            encoded.len()
        );
        Self {
            headers,
            encoded,
            structure,
            children: Vec::new(),
        }
    }
    pub(super) fn text(body: &str) -> Self {
        Self::leaf(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit",
            body.as_bytes().to_vec(),
        )
    }
    pub(super) fn multipart(
        subtype: &str,
        boundary: &str,
        extra: &str,
        children: Vec<Self>,
    ) -> Self {
        let headers =
            format!("Content-Type: multipart/{subtype}; boundary={boundary}{extra}\r\n\r\n")
                .into_bytes();
        let parsed = crate::mime::parse(&headers).unwrap();
        let mut encoded = Vec::new();
        for child in &children {
            encoded.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            encoded.extend_from_slice(&child.headers);
            encoded.extend_from_slice(&child.encoded);
            encoded.extend_from_slice(b"\r\n");
        }
        encoded.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let structure = format!(
            "({} {} {} NIL NIL NIL)",
            children
                .iter()
                .map(|child| child.structure.as_str())
                .collect::<String>(),
            quoted(subtype),
            parameters(&parsed.ctype.params)
        );
        Self {
            headers,
            encoded,
            structure,
            children,
        }
    }
    pub(super) fn raw(&self, uid: u32) -> Vec<u8> {
        let mut raw = format!("Message-ID: <id{uid}@example.test>\r\nReferences: <id7@example.test>\r\nSubject: 合成正文 \\\"\r\nFrom: sender@example.test\r\nTo: owner@example.test\r\n").into_bytes();
        raw.extend_from_slice(&self.headers);
        raw.extend_from_slice(&self.encoded);
        raw
    }
    pub(super) fn install(&self, h: &Harness, uid: u32) {
        h.structure.structures.lock().unwrap().insert(
            uid,
            structure::Reply::Data(
                format!("* 1 FETCH (UID {uid} BODYSTRUCTURE {})\r\n", self.structure).into_bytes(),
            ),
        );
        let section = if self.children.is_empty() { "1" } else { "" };
        self.install_node(h, uid, section, true);
    }
    fn install_node(&self, h: &Harness, uid: u32, section: &str, root: bool) {
        if !root {
            h.structure.headers.lock().unwrap().insert(
                (uid, format!("{section}.MIME")),
                structure::Evidence::Bytes(self.headers.clone()),
            );
        }
        let selector = if section.is_empty() { "TEXT" } else { section };
        h.parts.install_body(uid, selector, &self.encoded);
        for (index, child) in self.children.iter().enumerate() {
            let child_section = if section.is_empty() {
                (index + 1).to_string()
            } else {
                format!("{section}.{}", index + 1)
            };
            child.install_node(h, uid, &child_section, false);
        }
    }
}
pub(super) fn signed(h: &Harness) -> (Router, String) {
    let credentials = attachments::Credentials::new(h.service.config().clone());
    (
        attachments::router(h.service.clone(), credentials.auth.clone()),
        credentials.token(),
    )
}
pub(super) async fn call(
    h: &Harness,
    app: &Router,
    token: &str,
    name: &str,
    mut args: Value,
) -> Value {
    args.as_object_mut()
        .unwrap()
        .entry("uid_validity")
        .or_insert(json!(9));
    let id = "synthetic-正文-\\\"";
    let response = app.clone().oneshot(Request::post("/mcp")
        .header("Host", "localhost").header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json").header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .body(Body::from(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), 3 * 1024 * 1024)
        .await
        .unwrap();
    assert!(
        bytes.len() <= h.service.config().max_output,
        "complete response bytes={}",
        bytes.len()
    );
    let wire: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(wire["id"], id);
    assert!(wire.get("error").is_none(), "{wire}");
    let content = wire["result"]["structuredContent"].clone();
    assert_eq!(content["request_id"], request_id);
    assert_contract(name, &content);
    let copy: Value =
        serde_json::from_str(wire["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(copy, content);
    content
}
pub(super) fn commands(h: &Harness) -> Vec<(usize, String)> {
    h.structure.commands.lock().unwrap().clone()
}
pub(super) fn raw_fetches(h: &Harness) -> Vec<(usize, String)> {
    commands(h)
        .into_iter()
        .filter(|(_, command)| command.contains("BODY.PEEK[]"))
        .collect()
}
fn normalize(mut value: Value) -> Value {
    // Transport provenance differs intentionally; all message/body metadata is compared.
    fn visit(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.remove("read_diagnostics");
                for child in fields.values_mut() {
                    visit(child);
                }
            }
            Value::Array(items) => {
                for child in items {
                    visit(child);
                }
            }
            _ => {}
        }
    }
    visit(&mut value);
    value
}
async fn read_raw_and_parts(
    entity: &Entity,
    name: &str,
    args: Value,
    max_output: usize,
) -> (Harness, Value, Value) {
    let h = Harness::new(BTreeMap::from([(7, entity.raw(7))]), None, max_output).await;
    let (app, token) = signed(&h);
    // Unknown BODYSTRUCTURE is an explicit tagged NO, forcing the canonical raw reader.
    let oracle = call(&h, &app, &token, name, args.clone()).await;
    assert_eq!(
        raw_fetches(&h).len(),
        1,
        "raw oracle did not execute BODY[]: {oracle}"
    );
    entity.install(&h, 7);
    h.structure.commands.lock().unwrap().clear();
    let actual = call(&h, &app, &token, name, args).await;
    assert!(
        raw_fetches(&h).is_empty(),
        "part reader fell back: {actual}"
    );
    assert!(
        commands(&h)
            .iter()
            .any(|(_, command)| command == "UID FETCH 7 (UID BODYSTRUCTURE)")
    );
    (h, oracle, actual)
}

async fn compare_raw_and_parts(
    entity: &Entity,
    name: &str,
    args: Value,
    max_output: usize,
) -> (Harness, Value) {
    let (h, oracle, actual) = read_raw_and_parts(entity, name, args, max_output).await;
    assert_eq!(
        normalize(actual["data"].clone()),
        normalize(oracle["data"].clone()),
        "{name}: {actual} vs {oracle}"
    );
    if oracle.get("error").is_some() {
        assert_eq!(actual["error"], oracle["error"]);
    } else {
        assert!(actual.get("error").is_none(), "{actual}");
    }
    (h, actual)
}

fn take_wire_prefix(message: &mut Value, full_body: &str) -> Value {
    // The serialized diagnostics differ in size, so each path has its own wire budget.
    let prefix = message["body"].take();
    assert!(full_body.starts_with(prefix.as_str().unwrap()));
    prefix
}

#[tokio::test]
async fn all_body_tools_match_raw_metadata_and_display_without_attachment_payloads() {
    let entity = Entity::multipart(
        "mixed",
        "outer",
        "",
        vec![
            Entity::multipart(
                "alternative",
                "alt",
                "",
                vec![
                    Entity::text("完整正文 😀\r\n> 引用\r\n-- \r\n签名"),
                    Entity::leaf(
                        "Content-Type: text/html; charset=utf-8",
                        b"<p>Unused HTML</p>".to_vec(),
                    ),
                ],
            ),
            Entity::leaf(
                "Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=private.bin",
                vec![b'x'; CHUNK * 2],
            ),
        ],
    );
    for name in ["get_email", "get_emails", "read_conversation"] {
        for mode in ["full", "trimmed"] {
            let args = if name == "get_emails" {
                json!({"uids":[7],"uid_validity":9,"body_mode":mode})
            } else {
                json!({"uid":7,"uid_validity":9,"body_mode":mode})
            };
            let (h, actual) = compare_raw_and_parts(&entity, name, args, 2 * 1024 * 1024).await;
            assert!(actual.get("error").is_none(), "{actual}");
            assert!(raw_fetches(&h).is_empty());
            let payloads = commands(&h)
                .into_iter()
                .filter(|(_, command)| {
                    command.contains("BODY.PEEK[")
                        && !command.contains("HEADER")
                        && !command.contains("MIME")
                })
                .collect::<Vec<_>>();
            assert!(
                payloads
                    .iter()
                    .any(|(_, command)| command.contains("BODY.PEEK[1.1]")),
                "{payloads:?}"
            );
            assert!(
                !payloads
                    .iter()
                    .any(|(_, command)| command.contains("BODY.PEEK[2]")
                        || command.contains("BODY.PEEK[1.2]")),
                "{payloads:?}"
            );
        }
    }
}

#[tokio::test]
async fn alternative_complete_plain_probes_and_rollbacks_match_the_raw_selector() {
    for tail in [
        Entity::leaf("Content-Type: text/html", b"<p>Visible HTML</p>".to_vec()),
        Entity::leaf(
            "Content-Type: text/plain\r\nContent-Transfer-Encoding: x-unknown",
            b"opaque".to_vec(),
        ),
        Entity::multipart(
            "related",
            "r",
            "",
            vec![
                Entity::leaf("Content-Type: text/html", b"<p>Related HTML</p>".to_vec()),
                Entity::text("RESOURCE"),
            ],
        ),
    ] {
        let entity = Entity::multipart(
            "alternative",
            "a",
            "",
            vec![
                Entity::text("Complete fallback"),
                Entity::multipart(
                    "mixed",
                    "m",
                    "",
                    vec![Entity::text("Leaked signature"), tail],
                ),
            ],
        );
        let (h, actual) = compare_raw_and_parts(
            &entity,
            "get_email",
            json!({"uid":7,"uid_validity":9}),
            128 * 1024,
        )
        .await;
        assert_eq!(actual["data"]["body"], "Complete fallback");
        assert!(raw_fetches(&h).is_empty());
    }
    for first in ["", "Earlier plain"] {
        let entity = Entity::multipart(
            "alternative",
            "a",
            "",
            vec![
                Entity::text(first),
                Entity::leaf("Content-Type: text/html", b"<p>Unused HTML</p>".to_vec()),
            ],
        );
        let (_, actual) =
            compare_raw_and_parts(&entity, "get_email", json!({"uid":7}), 128 * 1024).await;
        assert_eq!(actual["data"]["body"], first);
    }
    let entity = Entity::multipart(
        "alternative",
        "a",
        "",
        vec![Entity::text("older"), Entity::text("newer")],
    );
    let (_, actual) =
        compare_raw_and_parts(&entity, "get_email", json!({"uid":7}), 128 * 1024).await;
    assert_eq!(actual["data"]["body"], "newer");
}

#[tokio::test]
async fn related_start_uses_exact_cid_and_never_defaults_when_present_but_unmatched() {
    for (start, expected) in [
        ("", Some("RESOURCE")),
        ("; start=\"<root>\"", Some("Visible root")),
        ("; start=\"<missing>\"", None),
        ("; start=\"<ROOT>\"", None),
    ] {
        let entity = Entity::multipart(
            "related",
            "r",
            start,
            vec![
                Entity::leaf(
                    "Content-Type: text/plain\r\nContent-ID: <resource>",
                    b"RESOURCE".to_vec(),
                ),
                Entity::leaf(
                    "Content-Type: text/plain\r\nContent-ID: <root>",
                    b"Visible root".to_vec(),
                ),
            ],
        );
        let (h, actual) =
            compare_raw_and_parts(&entity, "get_email", json!({"uid":7}), 128 * 1024).await;
        if let Some(expected) = expected {
            assert_eq!(actual["data"]["body"], expected);
        } else {
            assert_eq!(actual["error"]["code"], "UNSUPPORTED_BODY");
            assert!(
                commands(&h)
                    .iter()
                    .any(|(_, command)| command == "UID FETCH 7 (UID BODY.PEEK[TEXT]<0.1>)")
            );
        }
    }
}

#[tokio::test]
async fn a_small_nonfirst_body_with_implicit_charset_is_read_above_the_raw_budget() {
    let mut large_attachment = Entity::leaf(
        "Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=large.bin",
        b"synthetic-placeholder".to_vec(),
    );
    // Advertise a consistent large part without materializing its unrequested payload.
    let encoded_marker = format!(" {} NIL", large_attachment.encoded.len());
    assert_eq!(
        large_attachment.structure.matches(&encoded_marker).count(),
        1
    );
    large_attachment.structure =
        large_attachment
            .structure
            .replacen(&encoded_marker, " 41938944 NIL", 1);
    let mut text = Entity::leaf("Content-Type: text/plain", b"Small selected body".to_vec());
    let plain_without_parameters = r#""plain" NIL"#;
    assert!(text.structure.contains(plain_without_parameters));
    text.structure = text.structure.replacen(
        plain_without_parameters,
        r#""plain" ("CHARSET" "US-ASCII")"#,
        1,
    );
    let entity = Entity::multipart("mixed", "large", "", vec![large_attachment, text]);
    let h = Harness::with_limits(
        BTreeMap::from([(7, entity.raw(7))]),
        None,
        128 * 1024,
        false,
        None,
        Some(config::DEFAULT_MAX_MESSAGE),
    )
    .await;
    entity.install(&h, 7);
    h.structure.sizes.lock().unwrap().insert(
        7,
        structure::Reply::Data(b"* 1 FETCH (UID 7 RFC822.SIZE 41943040)\r\n".to_vec()),
    );
    let (app, token) = signed(&h);
    let actual = call(
        &h,
        &app,
        &token,
        "get_email",
        json!({"uid":7,"uid_validity":9}),
    )
    .await;
    assert_eq!(actual["data"]["body"], "Small selected body");
    assert!(raw_fetches(&h).is_empty());
    assert!(
        commands(&h)
            .iter()
            .any(|(_, command)| command == "UID FETCH 7 (UID BODY.PEEK[2]<0.65536>)")
    );
    assert!(
        !commands(&h)
            .iter()
            .any(|(_, command)| command.contains("BODY.PEEK[1]"))
    );

    h.structure.structures.lock().unwrap().remove(&7);
    let oversized = call(&h, &app, &token, "get_email", json!({"uid":7})).await;
    assert_eq!(oversized["error"]["code"], "LIMIT_EXCEEDED");
    assert_eq!(
        oversized["error"]["details"]["limit_kind"],
        "raw_message_bytes"
    );
    assert!(raw_fetches(&h).is_empty());
}

#[tokio::test]
async fn utf7_fallback_checks_validity_and_healthy_replacement_sessions_serve_later_uids() {
    let a = Entity::leaf(
        "Content-Type: text/plain; charset=utf-7",
        b"Hello +ThZ1TA-".to_vec(),
    );
    let b = Entity::leaf(
        "Content-Type: text/plain; charset=utf-7",
        b"Second +ThZ1TA-".to_vec(),
    );
    let c = Entity::text("Last healthy body");
    let h = Harness::new(
        BTreeMap::from([(7, a.raw(7)), (8, b.raw(8)), (9, c.raw(9))]),
        None,
        128 * 1024,
    )
    .await;
    a.install(&h, 7);
    b.install(&h, 8);
    c.install(&h, 9);
    let (app, token) = signed(&h);
    let actual = call(
        &h,
        &app,
        &token,
        "get_emails",
        json!({"uids":[7,8,7,9],"uid_validity":9}),
    )
    .await;
    assert_eq!(actual["data"]["completed"], 4);
    assert_eq!(actual["data"]["items"][0], actual["data"]["items"][2]);
    assert_eq!(
        actual["data"]["items"][3]["email"]["body"],
        "Last healthy body"
    );
    for (index, entity) in [a, b].iter().enumerate() {
        assert_eq!(
            actual["data"]["items"][index]["email"]["read_diagnostics"],
            json!({"source":"raw","fallback_reason":"charset_utf7","limit_kind":null})
        );
        assert_eq!(
            actual["data"]["items"][index]["email"]["body"],
            crate::mime::parse(&entity.raw(7 + index as u32))
                .unwrap()
                .plain_body()
                .unwrap()
        );
    }
    assert_eq!(h.connections.load(Ordering::SeqCst), 3);
    let raw = raw_fetches(&h);
    assert_eq!(raw.len(), 2);
    assert_eq!(raw[0].0, 2);
    assert!(raw[0].1.starts_with("UID FETCH 7 "));
    assert_eq!(raw[1].0, 3);
    assert!(raw[1].1.starts_with("UID FETCH 8 "));
    assert!(commands(&h).iter().any(
        |(connection, command)| *connection == 2 && command == "UID FETCH 8 (UID RFC822.SIZE)"
    ));
    assert!(
        commands(&h)
            .iter()
            .any(|(connection, command)| *connection == 3
                && command.contains("UID FETCH 9 (UID BODY.PEEK[1]"))
    );

    let entity = Entity::leaf(
        "Content-Type: text/plain; charset=utf-7",
        b"Hello +ThZ1TA-".to_vec(),
    );
    let h = Harness::new(BTreeMap::from([(7, entity.raw(7))]), None, 128 * 1024).await;
    entity.install(&h, 7);
    h.structure.reconnect_validity.store(10, Ordering::SeqCst);
    let (app, token) = signed(&h);
    let actual = call(
        &h,
        &app,
        &token,
        "get_email",
        json!({"uid":7,"uid_validity":9}),
    )
    .await;
    assert_eq!(actual["error"]["code"], "STALE_UID");
    assert!(raw_fetches(&h).is_empty());
    assert_eq!(h.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn completed_local_codec_failures_leave_batch_sessions_usable_even_with_zero_display() {
    for entity in [
        Entity::leaf(
            "Content-Type: text/plain; charset=x-unknown",
            b"nonempty".to_vec(),
        ),
        Entity::leaf(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64",
            b"YWJ!".to_vec(),
        ),
        Entity::leaf(
            "Content-Type: text/html; charset=utf-8",
            b"<svg><text>Rejected foreign</text></svg>".to_vec(),
        ),
    ] {
        for name in ["get_emails", "read_conversation"] {
            let healthy = Entity::text("Healthy later UID");
            let h = Harness::new(
                BTreeMap::from([(7, entity.raw(7)), (8, healthy.raw(8))]),
                None,
                128 * 1024,
            )
            .await;
            entity.install(&h, 7);
            healthy.install(&h, 8);
            let (app, token) = signed(&h);
            let args = if name == "get_emails" {
                json!({"uids":[7,8],"uid_validity":9,"html_display":"readable"})
            } else {
                json!({"uid":7,"uid_validity":9,"limit":2,"html_display":"readable"})
            };
            let actual = call(&h, &app, &token, name, args).await;
            if name == "get_emails" {
                assert_eq!(actual["data"]["items"][0]["ok"], false);
                assert_eq!(
                    actual["data"]["items"][1]["email"]["body"],
                    "Healthy later UID"
                );
                assert_eq!(actual["data"]["completed"], 2);
            } else {
                assert_eq!(
                    actual["data"]["messages"].as_array().unwrap().len(),
                    1,
                    "{actual}"
                );
                assert_eq!(actual["data"]["messages"][0]["uid"], 8);
                assert_eq!(actual["data"]["messages"][0]["body"], "Healthy later UID");
                assert_eq!(actual["data"]["message_errors"][0]["uid"], 7);
                assert_eq!(actual["data"]["omitted_uids"], json!([7]));
                assert_eq!(actual["data"]["complete"], false);
            }
            assert_eq!(h.connections.load(Ordering::SeqCst), 1);
            assert!(raw_fetches(&h).is_empty());
            let zero = call(
                &h,
                &app,
                &token,
                "get_email",
                json!({"uid":7,"max_body_length":0,"html_display":"readable"}),
            )
            .await;
            assert!(
                zero.get("error").is_some(),
                "zero display skipped body validation: {zero}"
            );
        }
    }
}

#[tokio::test]
async fn raw_fallback_requires_matching_complete_bytes_and_never_retries_failure() {
    for fault in [
        parts::Fault::WrongOrigin,
        parts::Fault::Duplicate,
        parts::Fault::Short,
        parts::Fault::Extra,
        parts::Fault::NoAfterData,
        parts::Fault::WrongTagAfterData,
    ] {
        let entity = Entity::text("Raw fallback fixture");
        let later = Entity::text("Must remain unprocessed");
        let raw = entity.raw(7);
        let h = Harness::new(
            BTreeMap::from([(7, raw.clone()), (8, later.raw(8))]),
            None,
            128 * 1024,
        )
        .await;
        later.install(&h, 8);
        h.parts.install_body(7, "", &raw);
        h.parts.fail_at(0, fault);
        let (app, token) = signed(&h);
        let result = call(&h, &app, &token, "get_emails", json!({"uids":[7,8]})).await;
        let items = result["data"]["items"].as_array().unwrap();
        assert!(items[0].get("error").is_some(), "{fault:?}: {result}");
        assert_eq!(items[1]["status"], "unprocessed", "{fault:?}: {result}");
        assert_eq!(h.connections.load(Ordering::SeqCst), 2);
        let fetches = raw_fetches(&h);
        assert_eq!(fetches.len(), 1);
        assert_eq!(fetches[0].0, 2);
    }
}

#[tokio::test]
async fn conversation_candidate_header_protocol_errors_stop_before_body_selection() {
    let seed = Entity::text("Seed");
    let related = Entity::text("Candidate");
    let related_raw = related.raw(8);
    let candidate_header = header(&related_raw).to_vec();
    for evidence in [
        structure::Evidence::WrongOrigin(candidate_header.clone()),
        structure::Evidence::Duplicate(candidate_header),
    ] {
        let h = Harness::new(
            BTreeMap::from([(7, seed.raw(7)), (8, related_raw.clone())]),
            None,
            128 * 1024,
        )
        .await;
        seed.install(&h, 7);
        related.install(&h, 8);
        h.structure
            .headers
            .lock()
            .unwrap()
            .insert((8, "HEADER".into()), evidence);
        let (app, token) = signed(&h);
        let result = call(&h, &app, &token, "read_conversation", json!({"uid":7})).await;
        assert_eq!(result["error"]["code"], "MAIL_BACKEND");
        assert_eq!(h.connections.load(Ordering::SeqCst), 1);
        let commands = commands(&h);
        assert!(
            commands
                .iter()
                .any(|(_, command)| command.starts_with("UID FETCH 8 (UID BODY.PEEK[HEADER]<0."))
        );
        assert!(
            !commands
                .iter()
                .any(|(_, command)| command.contains("BODYSTRUCTURE"))
        );
        assert!(raw_fetches(&h).is_empty());
    }
}

#[tokio::test]
async fn partial_body_command_failures_stop_batches_and_conversations_without_raw_retry() {
    for fault in [
        parts::Fault::No,
        parts::Fault::Bad,
        parts::Fault::Eof,
        parts::Fault::WrongTag,
        parts::Fault::NoAfterData,
        parts::Fault::BadAfterData,
        parts::Fault::ByeAfterData,
        parts::Fault::EofAfterData,
        parts::Fault::WrongTagAfterData,
        parts::Fault::WrongOrigin,
        parts::Fault::WrongSection,
        parts::Fault::Duplicate,
        parts::Fault::Missing,
        parts::Fault::Short,
        parts::Fault::Extra,
        parts::Fault::Oversized,
    ] {
        for name in ["get_emails", "read_conversation"] {
            let entity = Entity::text(&"x".repeat(CHUNK + 9));
            let later = Entity::text("Must remain unprocessed");
            let h = Harness::new(
                BTreeMap::from([(7, entity.raw(7)), (8, later.raw(8))]),
                None,
                128 * 1024,
            )
            .await;
            entity.install(&h, 7);
            later.install(&h, 8);
            h.parts.fail_at(CHUNK, fault);
            let (app, token) = signed(&h);
            let args = if name == "get_emails" {
                json!({"uids":[7,8],"uid_validity":9})
            } else {
                json!({"uid":7,"uid_validity":9,"limit":2})
            };
            let actual = call(&h, &app, &token, name, args).await;
            if name == "get_emails" {
                assert_eq!(
                    actual["data"]["items"][0]["ok"], false,
                    "{fault:?}: {actual}"
                );
                assert_eq!(
                    actual["data"]["items"][1]["status"], "unprocessed",
                    "{fault:?}: {actual}"
                );
                assert_eq!(actual["data"]["completed"], 1);
            } else {
                assert_eq!(actual["data"]["complete"], false, "{fault:?}: {actual}");
                assert!(
                    !actual["data"]["message_errors"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    actual["data"]["omitted_uids"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(7))
                );
                assert!(
                    actual["data"]["omitted_uids"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(8)),
                    "{fault:?}: {actual}"
                );
            }
            assert_eq!(h.connections.load(Ordering::SeqCst), 1);
            assert!(raw_fetches(&h).is_empty());
            assert!(
                !commands(&h)
                    .iter()
                    .any(|(_, command)| command.contains("UID FETCH 8 (UID BODY.PEEK[1]")),
                "later body fetched after protocol failure"
            );
        }
    }
}

#[tokio::test]
async fn readable_html_trimmed_and_complete_wire_prefixes_match_raw_display() {
    let entity = Entity::multipart("mixed", "display", "", vec![Entity::leaf("Content-Type: text/html; charset=utf-8", b"<blockquote><p>Before <a href='https://example.test/order'>Order</a></p><ul><li>One</li><li>Two</li></ul></blockquote>".to_vec()), Entity::leaf("Content-Type: text/html; charset=utf-8", b"<p>After</p>".to_vec())]);
    for display in ["legacy", "readable"] {
        for mode in ["full", "trimmed"] {
            let (_, actual) = compare_raw_and_parts(
                &entity,
                "get_email",
                json!({"uid":7,"html_display":display,"body_mode":mode}),
                128 * 1024,
            )
            .await;
            assert!(actual.get("error").is_none(), "{actual}");
        }
    }
    let full_body = "\\\"😀\u{0001}".repeat(20_000);
    let entity = Entity::text(&full_body);
    let (_, mut oracle, mut actual) =
        read_raw_and_parts(&entity, "get_email", json!({"uid":7}), 16 * 1024).await;
    assert_eq!(actual["data"]["body_truncated"], true);
    for value in [&mut oracle, &mut actual] {
        assert!(
            !take_wire_prefix(&mut value["data"], &full_body)
                .as_str()
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        normalize(actual["data"].clone()),
        normalize(oracle["data"].clone())
    );
}

#[tokio::test]
async fn charset_sequences_split_by_partial_fetches_match_the_whole_message_codec() {
    for (charset, prefix, suffix) in [
        ("utf-8", CHUNK - 1, "世😀".as_bytes()),
        ("gb18030", CHUNK - 1, b"\xd6\xd0\xce\xc4".as_slice()),
        ("iso-2022-jp", CHUNK - 1, b"\x1b$BF|K\\\x1b(B".as_slice()),
    ] {
        let mut encoded = vec![b'a'; prefix];
        encoded.extend_from_slice(suffix);
        let entity = Entity::leaf(
            &format!("Content-Type: text/plain; charset={charset}"),
            encoded,
        );
        let (h, actual) = compare_raw_and_parts(
            &entity,
            "get_email",
            json!({"uid":7,"max_body_length":100_000}),
            256 * 1024,
        )
        .await;
        assert!(actual.get("error").is_none(), "{charset}: {actual}");
        assert_eq!(actual["data"]["body_truncated"], false);
        assert!(raw_fetches(&h).is_empty());
        assert!(
            commands(&h)
                .iter()
                .any(|(_, command)| command.contains("BODY.PEEK[1]<65536.65536>"))
        );
    }
}

#[tokio::test]
async fn readable_formatter_budgets_are_shared_across_every_selected_html_leaf() {
    let links = |start: usize, end: usize| {
        (start..end)
            .map(|index| format!("<a href='https://example.test/{index}'>item{index}</a> "))
            .collect::<String>()
    };
    let entity = Entity::multipart(
        "mixed",
        "links",
        "",
        vec![
            Entity::leaf(
                "Content-Type: text/html; charset=utf-8",
                links(0, 200).into_bytes(),
            ),
            Entity::leaf(
                "Content-Type: text/html; charset=utf-8",
                links(200, 300).into_bytes(),
            ),
        ],
    );
    let (_, actual) = compare_raw_and_parts(
        &entity,
        "get_email",
        json!({"uid":7,"html_display":"readable"}),
        256 * 1024,
    )
    .await;
    assert_eq!(
        actual["data"]["body"]
            .as_str()
            .unwrap()
            .matches(" (https://")
            .count(),
        256
    );
    assert_eq!(actual["data"]["html_display_metadata"]["html_parts"], 2);
    assert!(
        actual["data"]["html_display_metadata"]["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value["reason"] == "link_limit" && value["count"] == 44)
    );
    for (first, second) in [
        ("<b></b>".repeat(9000), "<b></b>".repeat(9000)),
        ("a".repeat(600_000), "b".repeat(600_000)),
    ] {
        let entity = Entity::multipart(
            "mixed",
            "shared",
            "",
            vec![
                Entity::leaf("Content-Type: text/html; charset=utf-8", first.into_bytes()),
                Entity::leaf(
                    "Content-Type: text/html; charset=utf-8",
                    second.into_bytes(),
                ),
            ],
        );
        let (_, actual) = compare_raw_and_parts(
            &entity,
            "get_email",
            json!({"uid":7,"html_display":"readable","max_body_length":0}),
            128 * 1024,
        )
        .await;
        assert_eq!(actual["error"]["code"], "LIMIT_EXCEEDED");
    }
}

#[tokio::test]
async fn an_already_selected_plain_charset_error_does_not_choose_an_html_alternative() {
    let entity = Entity::multipart(
        "alternative",
        "charset",
        "",
        vec![
            Entity::leaf(
                "Content-Type: text/html",
                b"<p>Must not replace selected plain</p>".to_vec(),
            ),
            Entity::leaf(
                "Content-Type: text/plain; charset=x-unknown",
                b"Unsupported chosen plain".to_vec(),
            ),
        ],
    );
    let (h, actual) =
        compare_raw_and_parts(&entity, "get_email", json!({"uid":7}), 128 * 1024).await;
    assert_eq!(actual["error"]["code"], "UNSUPPORTED_BODY");
    assert!(raw_fetches(&h).is_empty());
    assert!(
        !commands(&h)
            .iter()
            .any(|(_, command)| command.contains("BODY.PEEK[1]"))
    );
}

#[tokio::test]
async fn collection_body_prefixes_obey_the_complete_wire_budget_with_escaped_text() {
    let full_body = "\\\"😀\u{0001}".repeat(20_000);
    let entity = Entity::text(&full_body);
    for name in ["get_emails", "read_conversation"] {
        let h = Harness::new(
            BTreeMap::from([(7, entity.raw(7)), (8, entity.raw(8))]),
            None,
            16 * 1024,
        )
        .await;
        let (app, token) = signed(&h);
        let args = if name == "get_emails" {
            json!({"uids":[7,8],"uid_validity":9})
        } else {
            json!({"uid":7,"uid_validity":9,"limit":2})
        };
        let mut oracle = call(&h, &app, &token, name, args.clone()).await;
        assert_eq!(raw_fetches(&h).len(), 2);
        entity.install(&h, 7);
        entity.install(&h, 8);
        h.structure.commands.lock().unwrap().clear();
        let mut actual = call(&h, &app, &token, name, args).await;
        assert!(actual.get("error").is_none(), "{actual}");
        for result in [&mut oracle, &mut actual] {
            let (key, subkey) = if name == "get_emails" {
                ("items", Some("email"))
            } else {
                ("messages", None)
            };
            let items = result["data"][key].as_array_mut().unwrap();
            for (index, item) in items.iter_mut().enumerate() {
                let message = match subkey {
                    Some(key) => &mut item[key],
                    None => item,
                };
                let prefix = take_wire_prefix(message, &full_body);
                if index == 0 {
                    assert!(!prefix.as_str().unwrap().is_empty());
                }
            }
        }
        assert_eq!(
            normalize(actual["data"].clone()),
            normalize(oracle["data"].clone())
        );
        if name == "get_emails" {
            let items = actual["data"]["items"].as_array().unwrap();
            assert_eq!(items.len(), 2);
            assert!(items.iter().all(|item| item["ok"] == true));
            assert!(
                items
                    .iter()
                    .any(|item| item["email"]["body_truncated"] == true)
            );
        } else {
            assert_eq!(actual["data"]["messages"].as_array().unwrap().len(), 2);
            assert!(
                actual["data"]["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["body_truncated"] == true)
            );
        }
        assert!(raw_fetches(&h).is_empty());
    }
}
