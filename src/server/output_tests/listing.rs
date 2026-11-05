//! Signed MCP acceptance for list and search over isolated TLS IMAP.
use super::{
    body_parts::{Entity, call, commands, raw_fetches, signed},
    *,
};
use std::sync::atomic::Ordering;

#[derive(Clone, Copy)]
pub(super) enum SummaryFault {
    WrongOrigin,
    Duplicate,
    Nil,
    Missing,
    NonTarget,
    NonTargetOversized,
}

pub(super) fn summary_response(
    response: &mut Vec<u8>,
    uid: u32,
    size: usize,
    bytes: &[u8],
    requested: usize,
    flags: &str,
    fault: Option<SummaryFault>,
) {
    response.extend_from_slice(format!("* 1 FETCH (UID {uid} FLAGS ({flags}))\r\n").as_bytes());
    match fault {
        Some(SummaryFault::Missing) => return,
        Some(SummaryFault::Nil) => {
            response.extend_from_slice(
                format!("* 1 FETCH (UID {uid} BODY[HEADER]<0> NIL)\r\n").as_bytes(),
            );
            return;
        }
        Some(SummaryFault::NonTarget) => {
            response.extend_from_slice(b"* 2 FETCH (UID 99999 BODY[HEADER]<1> {5}\r\nnoise)\r\n");
        }
        Some(SummaryFault::NonTargetOversized) => {
            // The declaration alone must fail before the parser waits for bytes.
            response.extend_from_slice(
                format!(
                    "* 2 FETCH (UID 99999 BODY[HEADER]<0> {{{}}}\r\n",
                    requested + 1
                )
                .as_bytes(),
            );
            return;
        }
        _ => {}
    }
    let origin = usize::from(matches!(fault, Some(SummaryFault::WrongOrigin)));
    let copies = if matches!(fault, Some(SummaryFault::Duplicate)) {
        2
    } else {
        1
    };
    for _ in 0..copies {
        response.extend_from_slice(
            format!(
                "* 1 FETCH (UID {uid} RFC822.SIZE {size} BODY[HEADER]<{origin}> {{{}}}\r\n",
                bytes.len()
            )
            .as_bytes(),
        );
        response.extend_from_slice(bytes);
        response.extend_from_slice(b")\r\n");
    }
}

fn attachment() -> Entity {
    Entity::leaf(
        "Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=synthetic.bin",
        b"ATTACHMENT_BODY_MUST_NOT_BE_FETCHED".to_vec(),
    )
}

fn mixed() -> Entity {
    Entity::multipart(
        "mixed",
        "preview-fixture",
        "",
        vec![attachment(), Entity::text(" \t正文\r\n\t 😀   第二段 ")],
    )
}

fn body_fetches(h: &Harness) -> Vec<String> {
    commands(h)
        .into_iter()
        .map(|(_, command)| command)
        .filter(|command| {
            command
                .split_once("BODY.PEEK[")
                .and_then(|(_, selector)| selector.split_once(']'))
                .is_some_and(|(section, _)| {
                    section == "TEXT"
                        || !section.is_empty()
                            && section
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || byte == b'.')
                })
        })
        .collect()
}

fn items(result: &Value) -> &[Value] {
    result["data"]["emails"]
        .as_array()
        .unwrap_or_else(|| panic!("missing email list: {result}"))
}

#[tokio::test]
async fn preview_defaults_off_and_classification_uses_bounded_complete_header_fields() {
    let full = b"Subject: weekly fixture\r\nList-Id: Synthetic list <weekly.example.test>\r\nList-Unsubscribe: <https://example.test/private-unsubscribe-token>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nAuto-Submitted: auto-generated\r\nContent-Type: text/plain\r\n\r\nprivate body".to_vec();
    let partial = format!(
        "Subject: partial fixture\r\nPrecedence: list\r\nList-Unsubscribe: <https://example.test/private-partial-token>\r\n {}\r\nAuto-Submitted: auto-generated\r\n\r\nprivate body",
        "x".repeat(9000),
    );
    let h = Harness::new(
        BTreeMap::from([
            (
                6,
                b"Subject: bodyless\r\nAuto-Submitted: auto-replied\r\n".to_vec(),
            ),
            (7, full),
            (8, partial.into_bytes()),
        ]),
        None,
        16 * 1024,
    )
    .await;
    let (app, token) = signed(&h);
    for name in ["list_emails", "search_emails"] {
        let tool = tools("full")
            .into_iter()
            .find(|tool| tool.name == name)
            .unwrap();
        let schema = serde_json::to_value(tool.input_schema).unwrap();
        assert_eq!(schema["properties"]["preview"]["type"], "boolean");
        assert_eq!(schema["properties"]["preview"]["default"], false);
        let default = call(&h, &app, &token, name, json!({})).await;
        let disabled = call(&h, &app, &token, name, json!({"preview": false})).await;
        assert_eq!(default["data"], disabled["data"]);
        let rows = items(&default);
        assert_eq!(rows[0]["classification"], json!(["newsletter"]));
        assert_eq!(rows[0]["classification_status"], "partial");
        assert_eq!(
            rows[0]["classification_signals"],
            json!([{"field":"precedence","category":"newsletter"}])
        );
        assert_eq!(rows[0]["unsubscribe_available"], false);
        assert_eq!(
            rows[1]["classification"],
            json!(["newsletter", "automated"])
        );
        assert_eq!(rows[1]["classification_status"], "complete");
        assert_eq!(
            rows[1]["classification_signals"].as_array().unwrap().len(),
            4
        );
        assert_eq!(rows[1]["unsubscribe_available"], true);
        assert_eq!(rows[1]["classification_rule_version"], "headers-v1");
        assert_eq!(rows[2]["classification"], json!(["automated"]));
        assert_eq!(rows[2]["classification_status"], "complete");
        for row in rows {
            for field in [
                "preview",
                "preview_status",
                "preview_truncated",
                "preview_error",
            ] {
                assert!(row.get(field).is_none(), "{row}");
            }
        }
        assert!(!default.to_string().contains("private-"));
        let invalid = call(&h, &app, &token, name, json!({"preview":"true"})).await;
        assert_eq!(invalid["error"]["code"], "INVALID_INPUT");
    }
    assert!(body_fetches(&h).is_empty());
    assert!(
        !commands(&h)
            .iter()
            .any(|(_, command)| command.contains("BODYSTRUCTURE"))
    );
    assert_eq!(h.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn listing_checks_target_header_origin_uniqueness_and_literal_budget() {
    use SummaryFault::*;
    for (fault, code, limit_kind, summaries) in [
        (WrongOrigin, Some("MAIL_BACKEND"), None, 0),
        (Duplicate, Some("MAIL_BACKEND"), None, 0),
        (Nil, Some("MAIL_BACKEND"), None, 0),
        (Missing, None, None, 0),
        (NonTarget, None, None, 1),
        (
            NonTargetOversized,
            Some("LIMIT_EXCEEDED"),
            Some("part_fetch_bytes"),
            0,
        ),
    ] {
        let h = Harness::new(
            BTreeMap::from([(7, raw(7, "synthetic body"))]),
            None,
            16 * 1024,
        )
        .await;
        h.summary_faults.lock().unwrap().insert(7, fault);
        let (app, token) = signed(&h);
        let result = call(&h, &app, &token, "list_emails", json!({})).await;
        if let Some(code) = code {
            assert_eq!(result["error"]["code"], code);
            assert_eq!(
                result["error"]["details"]["limit_kind"].as_str(),
                limit_kind
            );
        } else {
            assert_eq!(items(&result).len(), summaries);
            if summaries != 0 {
                assert_eq!(items(&result)[0]["uid"], 7);
            }
        }
        assert!(body_fetches(&h).is_empty());
    }
}

#[tokio::test]
async fn list_and_search_preview_choose_the_nonfirst_leaf_and_keep_summary_flags() {
    let entity = mixed();
    for name in ["list_emails", "search_emails"] {
        let h = Harness::new(BTreeMap::from([(7, entity.raw(7))]), None, 16 * 1024).await;
        entity.install(&h, 7);
        h.summary_flags
            .lock()
            .unwrap()
            .insert(7, "\\Seen \\Flagged");
        let (app, token) = signed(&h);
        let mut args = json!({"preview":true});
        if name == "search_emails" {
            args["subject"] = json!("合成");
        }
        let result = call(&h, &app, &token, name, args).await;
        let row = &items(&result)[0];
        assert_eq!(row["preview"], "正文 😀 第二段");
        assert_eq!(row["preview_status"], "ok");
        assert_eq!(row["preview_truncated"], false);
        assert_eq!(row["flags"], json!(["Seen", "Flagged"]));
        assert_eq!(row["uid_validity"], 9);
        assert_eq!(row["account_id"], "default");
        assert_eq!(row["classification"], json!(["unclassified"]));
        let bodies = body_fetches(&h);
        assert_eq!(bodies.len(), 2);
        assert!(
            bodies
                .iter()
                .all(|command| command.contains("BODY.PEEK[2]"))
        );
        assert!(raw_fetches(&h).is_empty());
        let log = commands(&h);
        let summary = log
            .iter()
            .position(|(_, command)| command.contains("UID FLAGS RFC822.SIZE"))
            .unwrap();
        let manifest = log
            .iter()
            .position(|(_, command)| command.contains("BODYSTRUCTURE"))
            .unwrap();
        assert!(summary < manifest);
        assert_eq!(h.writes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn html_and_unicode_sampling_boundaries_preserve_complete_visible_prefixes() {
    let html_head = "<p>HTML &amp; 😀</p><script>";
    let html = Entity::leaf(
        "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit",
        format!(
            "{html_head}{}SECRET</script><p>outside</p>",
            "x".repeat(8192)
        )
        .into_bytes(),
    );
    let text = Entity::text(&format!("首{}😀尾", " ".repeat(8187)));
    let blank = Entity::text(" \t\r\n ");
    let h = Harness::new(
        BTreeMap::from([(7, blank.raw(7)), (8, text.raw(8)), (9, html.raw(9))]),
        None,
        16 * 1024,
    )
    .await;
    for (uid, entity) in [(7, blank), (8, text), (9, html)] {
        entity.install(&h, uid);
    }
    let (app, token) = signed(&h);
    let result = call(&h, &app, &token, "list_emails", json!({"preview":true})).await;
    let rows = items(&result);
    for (row, expected) in rows[..2].iter().zip(["HTML & 😀", "首"]) {
        assert_eq!(row["preview"], expected);
        assert_eq!(row["preview_status"], "ok");
        assert_eq!(row["preview_truncated"], true);
    }
    assert_eq!(rows[2]["preview"], "");
    assert_eq!(rows[2]["preview_status"], "empty");
    assert_eq!(rows[2]["preview_truncated"], false);
    assert!(!result.to_string().contains("SECRET"));
    assert!(raw_fetches(&h).is_empty());
}

#[tokio::test]
async fn page_sample_budget_skips_older_previews_without_changing_cursor_or_order() {
    let entity = Entity::text(&"😀".repeat(3000));
    let uids = (500..=517).collect::<Vec<_>>();
    let h = Harness::new(
        uids.iter().map(|&uid| (uid, entity.raw(uid))).collect(),
        None,
        256 * 1024,
    )
    .await;
    for &uid in &uids {
        entity.install(&h, uid);
    }
    let (app, token) = signed(&h);
    let baseline = call(&h, &app, &token, "list_emails", json!({"limit":18})).await;
    let preview = call(
        &h,
        &app,
        &token,
        "list_emails",
        json!({"limit":18,"preview":true}),
    )
    .await;
    let rows = items(&preview);
    assert_eq!(rows.len(), 18);
    assert_eq!(
        baseline["data"]["next_before_uid"],
        preview["data"]["next_before_uid"]
    );
    assert_eq!(
        baseline["data"]["scanned_uid_range"],
        preview["data"]["scanned_uid_range"]
    );
    for (index, (row, baseline)) in rows.iter().zip(items(&baseline)).enumerate() {
        assert_eq!(row["uid"], baseline["uid"]);
        assert_eq!(row["subject"], baseline["subject"]);
        if index < 16 {
            assert_eq!(row["preview_status"], "ok");
            assert_eq!(row["preview"].as_str().unwrap().chars().count(), 200);
            assert_eq!(row["preview_truncated"], true);
        } else {
            assert_eq!(row["preview_status"], "budget_skipped");
            assert!(row.get("preview").is_none());
        }
    }
    let bodies = body_fetches(&h);
    assert_eq!(bodies.len(), 16);
    assert!(
        bodies
            .iter()
            .all(|command| command.ends_with("[1]<0.8192>)"))
    );
    assert_eq!(
        commands(&h)
            .iter()
            .filter(|(_, command)| command.contains("BODYSTRUCTURE"))
            .count(),
        16,
    );
}

#[tokio::test]
async fn preview_local_errors_continue_and_failed_commands_keep_all_summaries() {
    let local = Entity::leaf("Content-Type: text/plain; charset=utf-7", b"+Ti0-".to_vec());
    let good = Entity::text("healthy");
    let refused = Entity::text("oops");
    let later = Entity::text("later item");
    let fixtures = [(9, local), (8, good), (7, refused), (6, later)];
    let h = Harness::new(
        fixtures
            .iter()
            .map(|(uid, entity)| (*uid, entity.raw(*uid)))
            .collect(),
        None,
        16 * 1024,
    )
    .await;
    for (uid, entity) in fixtures {
        entity.install(&h, uid);
    }
    h.parts.fail_at(4, parts::Fault::NoAfterData);
    let (app, token) = signed(&h);
    let result = call(&h, &app, &token, "list_emails", json!({"preview":true})).await;
    let rows = items(&result);
    assert_eq!(
        rows.iter()
            .map(|row| row["uid"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [9, 8, 7, 6]
    );
    assert_eq!(rows[0]["preview_status"], "error");
    assert_eq!(rows[0]["preview_error"]["code"], "UNSUPPORTED_BODY");
    assert_eq!(rows[1]["preview_status"], "ok");
    assert_eq!(rows[1]["preview"], "healthy");
    assert_eq!(rows[2]["preview_status"], "error");
    assert_eq!(rows[2]["preview_error"]["code"], "MAIL_BACKEND");
    assert!(rows[2].get("preview").is_none());
    assert_eq!(rows[3]["preview_status"], "unprocessed");
    let log = commands(&h);
    assert!(
        !log.iter()
            .any(|(_, command)| command == "UID FETCH 6 (UID BODYSTRUCTURE)")
    );
    assert!(raw_fetches(&h).is_empty());
    assert_eq!(h.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn preview_subdeadline_keeps_completed_summaries_and_stops_later_fetches() {
    let h = Harness::new(
        [504, 505, 506]
            .into_iter()
            .map(|uid| (uid, raw(uid, "synthetic body")))
            .collect(),
        None,
        16 * 1024,
    )
    .await;
    h.stall_structure_uid.store(506, Ordering::SeqCst);
    let (app, token) = signed(&h);
    let result = call(
        &h,
        &app,
        &token,
        "list_emails",
        json!({"preview":true,"limit":2}),
    )
    .await;
    let rows = items(&result);
    assert_eq!(rows.len(), 2);
    assert_eq!(result["data"]["next_before_uid"], 505);
    assert_eq!(rows[0]["uid"], 506);
    assert_eq!(rows[0]["preview_status"], "error");
    assert_eq!(rows[0]["preview_error"]["code"], "TIMEOUT");
    assert_eq!(rows[1]["uid"], 505);
    assert_eq!(rows[1]["preview_status"], "unprocessed");
    assert!(rows.iter().all(|row| row["subject"].is_string()));
    assert_eq!(
        commands(&h)
            .into_iter()
            .filter(|(_, command)| command.contains("BODYSTRUCTURE"))
            .map(|(_, command)| command)
            .collect::<Vec<_>>(),
        ["UID FETCH 506 (UID BODYSTRUCTURE)"]
    );
    assert!(body_fetches(&h).is_empty());
    assert!(raw_fetches(&h).is_empty());
}

#[tokio::test]
async fn wire_budget_removes_escaped_preview_without_losing_summary() {
    let entity = Entity::text(&"\u{1b}".repeat(200));
    let h = Harness::new(BTreeMap::from([(7, entity.raw(7))]), None, 2000).await;
    entity.install(&h, 7);
    let (app, token) = signed(&h);
    let result = call(&h, &app, &token, "search_emails", json!({"preview":true})).await;
    let row = &items(&result)[0];
    assert_eq!(row["uid"], 7);
    assert_eq!(row["preview_status"], "wire_budget_skipped");
    assert!(row.get("preview").is_none());
    assert!(row.get("preview_truncated").is_none());
    assert!(!body_fetches(&h).is_empty());
}

#[test]
fn wire_budget_keeps_summaries_even_when_preview_statuses_do_not_fit() {
    let summaries = (7..=10)
        .rev()
        .map(|uid| {
            json!({
                "uid":uid,"account_id":"default","mailbox":"INBOX","uid_validity":9,
                "subject":"正文 \"标题\" \\ 转义","from":"fixture@example.test","date":null,
                "size":128,"flags":[],"classification":["unclassified"],"classification_signals":[],
                "classification_rule_version":"headers-v1","classification_status":"complete",
                "unsubscribe_available":false
            })
        })
        .collect::<Vec<_>>();
    let summary = json!({
        "account_id":"default","mailbox":"INBOX","emails":summaries,
        "uid_validity":9,"next_before_uid":7,"scanned_uid_range":[1,10]
    });
    let request_id = "synthetic-wire-budget";
    let id = RequestId::String("\n\"\\中".repeat(110).into());
    assert!((900..=MAX_RPC_ID_BYTES).contains(&serde_json::to_vec(&id).unwrap().len()));
    let encode = |response| {
        serde_json::to_vec(&JsonRpcResponse {
            jsonrpc: JsonRpcVersion2_0,
            id: id.clone(),
            result: ServerResult::CallToolResult(response),
        })
        .unwrap()
    };
    let wire_len = |data: &Value| {
        encode(CallToolResult::structured(
            json!({"data":data,"request_id":request_id}),
        ))
        .len()
    };
    let check = |response: CallToolResult, limit| {
        let wire_bytes = encode(response);
        assert!(wire_bytes.len() <= limit);
        let wire: Value = serde_json::from_slice(&wire_bytes).unwrap();
        assert_eq!(wire["id"], serde_json::to_value(&id).unwrap());
        let content = wire["result"]["structuredContent"].clone();
        let copy: Value =
            serde_json::from_str(wire["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(copy, content);
        assert_contract("list_emails", &content);
        (content, wire_bytes.len())
    };

    let mut statuses = summary.clone();
    for (row, status) in statuses["emails"].as_array_mut().unwrap().iter_mut().zip([
        "budget_skipped",
        "unprocessed",
        "budget_skipped",
        "unprocessed",
    ]) {
        row["preview_status"] = json!(status);
    }
    let mut mixed = statuses.clone();
    mixed["emails"][0]["preview_status"] = json!("ok");
    mixed["emails"][0]["preview"] = json!("中\"\\\n".repeat(50));
    mixed["emails"][0]["preview_truncated"] = json!(true);
    mixed["emails"][1]["preview_status"] = json!("error");
    mixed["emails"][1]["preview_error"] = json!(MailError::backend());

    let mut tail_trimmed = mixed.clone();
    let row = tail_trimmed["emails"][1].as_object_mut().unwrap();
    row.remove("preview_error");
    row.insert("preview_status".into(), json!("wire_budget_skipped"));
    let threshold = wire_len(&tail_trimmed);
    let response = bounded_listing_result(Ok(mixed.clone()), request_id, &id, threshold).unwrap();
    let (content, actual) = check(response, threshold);
    assert_eq!(actual, threshold);
    assert_eq!(content["data"], tail_trimmed);
    let response =
        bounded_listing_result(Ok(mixed.clone()), request_id, &id, threshold - 1).unwrap();
    let (content, _) = check(response, threshold - 1);
    assert_eq!(
        content["data"]["emails"][0]["preview_status"],
        "wire_budget_skipped"
    );
    assert_eq!(
        content["data"]["emails"][2]["preview_status"],
        "budget_skipped"
    );

    let threshold = wire_len(&summary);
    for data in [statuses, mixed] {
        assert!(wire_len(&data) > threshold);
        let response =
            bounded_listing_result(Ok(data.clone()), request_id, &id, threshold).unwrap();
        let (content, actual) = check(response, threshold);
        assert_eq!(actual, threshold);
        assert_eq!(content["data"], summary);

        let response = bounded_listing_result(Ok(data), request_id, &id, threshold - 1).unwrap();
        let (content, _) = check(response, threshold - 1);
        assert_eq!(content["error"]["code"], "LIMIT_EXCEEDED");
        assert_eq!(
            content["error"]["details"]["limit_kind"],
            "wire_response_bytes"
        );
    }
}

#[tokio::test]
async fn attachment_filter_uses_verified_metadata_and_preserves_search_cursor() {
    let attached = Entity::multipart(
        "mixed",
        "large-attachment",
        "",
        vec![
            Entity::text("small body"),
            Entity::leaf(
                "Content-Type: application/octet-stream\r\nContent-Disposition: attachment",
                vec![b'x'; 128 * 1024],
            ),
        ],
    );
    let plain = Entity::text("plain fixture");
    let fixtures = [(506, &plain), (505, &attached), (504, &attached)];
    let h = Harness::with_limits(
        fixtures
            .iter()
            .map(|(uid, entity)| (*uid, entity.raw(*uid)))
            .collect(),
        None,
        16 * 1024,
        false,
        None,
        Some(8 * 1024),
    )
    .await;
    for (uid, entity) in fixtures {
        entity.install(&h, uid);
    }
    let (app, token) = signed(&h);
    let first = call(
        &h,
        &app,
        &token,
        "search_emails",
        json!({"limit":1,"has_attachment":true}),
    )
    .await;
    assert_eq!(items(&first)[0]["uid"], 505);
    assert_eq!(first["data"]["next_before_uid"], 505);
    let second = call(
        &h,
        &app,
        &token,
        "search_emails",
        json!({"limit":1,"has_attachment":true,"before_uid":505}),
    )
    .await;
    assert_eq!(items(&second)[0]["uid"], 504);
    assert!(body_fetches(&h).is_empty());
    assert!(raw_fetches(&h).is_empty());
    assert!(
        commands(&h)
            .iter()
            .any(|(_, command)| command.contains("BODY.PEEK[1.MIME]"))
    );
    assert_eq!(h.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn attachment_filter_fallback_short_page_resumes_after_last_verified_uid() {
    let attached = mixed();
    let plain = Entity::text("plain fixture");
    let h = Harness::new(
        BTreeMap::from([(9, attached.raw(9)), (8, plain.raw(8)), (7, plain.raw(7))]),
        None,
        16 * 1024,
    )
    .await;
    // The fixture rejects BODYSTRUCTURE only for 9.
    for uid in [7, 8] {
        plain.install(&h, uid);
    }
    let (app, token) = signed(&h);
    let first = call(
        &h,
        &app,
        &token,
        "search_emails",
        json!({"has_attachment":false}),
    )
    .await;
    assert!(items(&first).is_empty());
    assert_eq!(first["data"]["next_before_uid"], 9);
    let raw = raw_fetches(&h);
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].0, 2);
    assert!(raw[0].1.starts_with("UID FETCH 9 "));
    assert_eq!(h.connections.load(Ordering::SeqCst), 2);
    assert!(
        !commands(&h)
            .iter()
            .any(|(_, command)| command == "UID FETCH 8 (UID BODYSTRUCTURE)")
    );
    let second = call(
        &h,
        &app,
        &token,
        "search_emails",
        json!({"has_attachment":false,"before_uid":9}),
    )
    .await;
    assert_eq!(
        items(&second)
            .iter()
            .map(|row| row["uid"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [8, 7]
    );
    assert_eq!(raw_fetches(&h).len(), 1);
}

#[tokio::test]
async fn preview_reuses_the_single_validated_attachment_filter_fallback() {
    let raw = mixed();
    let structured = mixed();
    let h = Harness::new(
        BTreeMap::from([(9, raw.raw(9)), (10, structured.raw(10))]),
        None,
        16 * 1024,
    )
    .await;
    structured.install(&h, 10);
    let (app, token) = signed(&h);
    let result = call(
        &h,
        &app,
        &token,
        "search_emails",
        json!({"has_attachment":true,"preview":true}),
    )
    .await;
    let rows = items(&result);
    assert_eq!(rows.len(), 2);
    for (row, uid) in rows.iter().zip([10, 9]) {
        assert_eq!(row["uid"], uid);
        assert_eq!(row["preview_status"], "ok");
        assert_eq!(row["preview"], "正文 😀 第二段");
        assert_eq!(row["preview_truncated"], false);
    }
    let raw = raw_fetches(&h);
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].0, 2);
    assert!(raw[0].1.starts_with("UID FETCH 9 "));
    assert_eq!(
        commands(&h)
            .iter()
            .filter(|(_, command)| command == "UID FETCH 9 (UID BODYSTRUCTURE)")
            .count(),
        1
    );
    let bodies = body_fetches(&h);
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies
            .iter()
            .all(|command| command.starts_with("UID FETCH 10 "))
    );
    assert_eq!(h.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn attachment_filter_stops_on_stale_or_failed_fallback_and_bounds_raw_bytes() {
    let entity = Entity::text(&"x".repeat(10 * 1024));
    let messages = || BTreeMap::from([(7, entity.raw(7))]);
    let h = Harness::with_limits(messages(), None, 16 * 1024, false, None, Some(1024)).await;
    let (app, token) = signed(&h);
    let result = call(
        &h,
        &app,
        &token,
        "search_emails",
        json!({"has_attachment":true}),
    )
    .await;
    assert_eq!(result["error"]["code"], "LIMIT_EXCEEDED");
    assert_eq!(
        result["error"]["details"]["limit_kind"],
        "raw_message_bytes"
    );
    assert!(raw_fetches(&h).is_empty());
    assert_eq!(h.connections.load(Ordering::SeqCst), 1);

    let h = Harness::new(messages(), None, 16 * 1024).await;
    h.structure.reconnect_validity.store(10, Ordering::SeqCst);
    let (app, token) = signed(&h);
    let result = call(
        &h,
        &app,
        &token,
        "search_emails",
        json!({"has_attachment":true}),
    )
    .await;
    assert_eq!(result["error"]["code"], "STALE_UID");
    assert!(raw_fetches(&h).is_empty());
    assert_eq!(h.connections.load(Ordering::SeqCst), 2);

    let h = Harness::new(messages(), Some(7), 16 * 1024).await;
    let (app, token) = signed(&h);
    let result = call(
        &h,
        &app,
        &token,
        "search_emails",
        json!({"has_attachment":false}),
    )
    .await;
    assert_eq!(result["error"]["code"], "MAIL_BACKEND");
    assert_eq!(raw_fetches(&h).len(), 1);
    assert_eq!(h.connections.load(Ordering::SeqCst), 2);
}
