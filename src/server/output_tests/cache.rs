//! Signed MCP acceptance of the metadata cache in the real account router.
use super::conversation::fixture::Fault;
use super::{
    conversation::fixture::{Folder, Harness as CacheHarness, message},
    *,
};
use std::path::{Path, PathBuf};

fn configuration(root: &Path, accounts: &[(&str, &[&str])]) -> PathBuf {
    let path = root.join("cache.json");
    let accounts = accounts
        .iter()
        .map(|(account, folders)| {
            json!({
                "account_id":account,"mailboxes":folders,"max_bytes":8 * 1024 * 1024
            })
        })
        .collect::<Vec<_>>();
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,"mode":"metadata","directory":"metadata","max_bytes":32 * 1024 * 1024,
            "accounts":accounts
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

fn synthetic(uid: u32, subject: &str) -> Vec<u8> {
    message(
        Some(&format!("message-{uid}@example.test")),
        &[],
        None,
        subject,
        "synthetic content",
    )
}

fn full_configuration(root: &Path, accounts: &[(&str, &[&str])]) -> PathBuf {
    let path = configuration(root, accounts);
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    settings["mode"] = json!("full");
    settings["max_bytes"] = json!(96 * 1024 * 1024);
    for account in settings["accounts"].as_array_mut().unwrap() {
        account["max_bytes"] = json!(40 * 1024 * 1024);
    }
    std::fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
    path
}

const CACHE_OBSERVATION_WAIT: Duration = Duration::from_secs(2);
const CACHE_WRITE_WAIT: Duration = Duration::from_secs(15);

async fn wait_cached_body(h: &CacheHarness, app: &axum::Router, uid: u32, expected: &str) {
    let status = wait_cache_barrier(h, app).await;
    let body = h
        .call_tool(
            Some(app),
            "alice",
            "get_email",
            json!({"uid":uid,"uid_validity":9,"read_source":"cache"}),
        )
        .await;
    assert_eq!(
        body["data"]["read_source"], "cache",
        "UID {uid}: {body}, {status}"
    );
    assert_eq!(body["data"]["uid"], uid, "{body}");
    assert_eq!(body["data"]["body"], expected, "{body}");
}

async fn wait_cache_barrier(h: &CacheHarness, app: &axum::Router) -> Value {
    let mut last = Value::Null;
    tokio::time::timeout(CACHE_WRITE_WAIT, async {
        loop {
            last = h
                .call_tool(Some(app), "alice", "get_cache_status", json!({}))
                .await;
            assert_eq!(last["data"]["write_health"]["failed_writes"], 0, "{last}");
            match last["data"]["status"].as_str() {
                Some("ready") if last["data"]["content_pending"] == 0 => break,
                Some("ready" | "unavailable") => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                _ => panic!("unexpected cache status: {last}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("cache queue did not confirm readiness: {last}"));
    last
}

#[tokio::test]
async fn live_missing_future_uid_does_not_disable_cache_or_tombstone_later_arrival() {
    let root = tempfile::tempdir().unwrap();
    let path = full_configuration(root.path(), &[("default", &["INBOX"])]);
    let h = CacheHarness::new(
        [(
            "INBOX",
            Folder::new("", 9, [(7, synthetic(7, "already assigned"))]),
        )],
        512 * 1024,
    )
    .await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let call = |name, args| h.call_tool(Some(&app), "alice", name, args);
    let local = |uid| {
        call(
            "get_email",
            json!({"uid":uid,"uid_validity":9,"read_source":"cache"}),
        )
    };
    let _ = call("get_email", json!({"uid":7,"uid_validity":9})).await;
    let _ = call("get_cache_status", json!({})).await;
    assert_eq!(local(7).await["data"]["subject"], "already assigned");
    let future = call("get_email_status", json!({"uid":100,"uid_validity":9})).await;
    assert_eq!(future["error"]["code"], "NOT_FOUND");
    let next = call("get_email_status", json!({"uid":8,"uid_validity":9})).await;
    assert_eq!(next["error"]["code"], "NOT_FOUND");
    h.update("INBOX", |folder| folder.report_uid_next = false);
    h.fault("INBOX", "UID FETCH 7 (UID FLAGS)", Fault::NoDataOk);
    let unknown = call("get_email_status", json!({"uid":7,"uid_validity":9})).await;
    assert_eq!(unknown["error"]["code"], "NOT_FOUND");
    let _ = call("get_cache_status", json!({})).await;
    assert_eq!(local(7).await["data"]["subject"], "already assigned");

    h.update("INBOX", |folder| {
        folder.report_uid_next = true;
        folder.messages.insert(8, synthetic(8, "later arrival"));
    });
    assert_eq!(
        call("get_email", json!({"uid":8,"uid_validity":9})).await["data"]["subject"],
        "later arrival"
    );
    let _ = call("get_cache_status", json!({})).await;
    let before = h.connections();
    assert_eq!(local(7).await["data"]["subject"], "already assigned");
    assert_eq!(local(8).await["data"]["subject"], "later arrival");
    assert_eq!(h.connections(), before);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn live_listing_and_preview_tombstone_only_confirmed_missing_uids() {
    let root = tempfile::tempdir().unwrap();
    let path = full_configuration(root.path(), &[("default", &["INBOX"])]);
    let messages = std::array::from_fn::<_, 3, _>(|i| {
        let uid = 7 + i as u32;
        (uid, synthetic(uid, "cached listing"))
    });
    let h = CacheHarness::new([("INBOX", Folder::new("", 9, messages))], 512 * 1024).await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let call = |name, args| h.call_tool(Some(&app), "alice", name, args);
    let local = |uid| {
        call(
            "get_email",
            json!({"uid":uid,"uid_validity":9,"read_source":"cache"}),
        )
    };
    for uid in 7..=9 {
        assert_eq!(
            call("get_email", json!({"uid":uid,"uid_validity":9})).await["data"]["body"],
            "synthetic content"
        );
        // One in-flight capture at a time here. The shared write-through
        // payload pool is best-effort, so a third rapid capture can be skipped.
        wait_cached_body(&h, &app, uid, "synthetic content").await;
    }
    let summary = "UID FETCH 9 (UID FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER]<0.8192>)";
    h.fault("INBOX", summary, Fault::OnlyUid(9));
    let flags_only = call("list_emails", json!({"limit":1})).await;
    assert_eq!(flags_only["data"]["emails"], json!([]), "{flags_only}");
    assert_eq!(local(9).await["data"]["body"], "synthetic content");
    h.fault("INBOX", summary, Fault::NoDataOk);
    assert_eq!(
        call("list_emails", json!({"limit":1})).await["data"]["emails"],
        json!([])
    );

    h.fault("INBOX", "UID FETCH 8 (UID RFC822.SIZE)", Fault::NoDataOk);
    let filtered = call(
        "search_emails",
        json!({"before_uid":9,"has_attachment":false}),
    )
    .await;
    assert_eq!(filtered["error"]["code"], "NOT_FOUND", "{filtered}");

    h.fault(
        "INBOX",
        "UID FETCH 7 (UID BODY.PEEK[1]<0.17>)",
        Fault::NoDataOk,
    );
    let preview = call("list_emails", json!({"before_uid":8,"preview":true})).await;
    let item = &preview["data"]["emails"][0];
    assert_eq!(item["uid"], 7);
    assert_eq!(item["preview_status"], "error");
    assert_eq!(item["preview_error"]["code"], "MAIL_BACKEND");
    wait_cache_barrier(&h, &app).await;
    let before = h.connections();
    for uid in 7..=9 {
        assert_eq!(local(uid).await["error"]["code"], "CACHE_MISS");
    }
    assert_eq!(h.connections(), before);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn full_signed_body_listing_search_and_exact_locate_survive_clean_restart_without_imap() {
    let root = tempfile::tempdir().unwrap();
    let path = full_configuration(root.path(), &[("default", &["INBOX"])]);
    let html = "<p>中文正文独有检索词</p><p>下一行 &amp; 引号</p>";
    let raw = format!(
        "Subject: 独立主题\r\nFrom: author@example.test\r\nTo: owner@example.test\r\nMessage-ID: <full@example.test>\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{html}"
    );
    let mut inbox = Folder::new("", 9, [(7, raw.into_bytes())]);
    inbox.structures.insert(
        7,
        format!(
            "(\"TEXT\" \"HTML\" (\"CHARSET\" \"utf-8\") NIL NIL \"8BIT\" {} 1 NIL NIL)",
            html.len()
        ),
    );
    let h = CacheHarness::new([("INBOX", inbox)], 512 * 1024).await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let read = |name, args| h.call_tool(Some(&app), "alice", name, args);
    let accounts = read("list_accounts", json!({})).await;
    assert_eq!(
        accounts["data"]["accounts"][0]["capabilities"]["cache_modes"],
        json!(["off", "metadata", "full"])
    );
    assert_eq!(
        read("list_mailboxes", json!({})).await["data"]["mailboxes"][0]["name"],
        "INBOX"
    );
    assert_eq!(
        read("list_emails", json!({})).await["data"]["emails"][0]["uid"],
        7
    );
    let warm = read(
        "get_email",
        json!({"uid":7,"uid_validity":9,"html_display":"readable"}),
    )
    .await;
    assert!(
        warm["data"]["body"]
            .as_str()
            .unwrap_or_default()
            .contains("中文正文"),
        "{warm}"
    );
    let status = wait_cache_barrier(&h, &app).await;
    assert_eq!(status["data"]["content_pending"], 0, "{status}");
    assert_eq!(
        status["data"]["storage"]["folders"][0]["layers"]["full_body"]["materialized"], 1,
        "{status}"
    );
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);

    let before = h.connections();
    h.stop_imap();
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let read = |name, args| h.call_tool(Some(&app), "alice", name, args);
    let folders = read("list_mailboxes", json!({"read_source":"cache"})).await;
    assert_eq!(folders["data"]["mailboxes"][0]["name"], "INBOX");
    assert_eq!(
        folders["data"]["cache"]["remote_generation_unverified"],
        true
    );
    let list = read("list_emails", json!({"read_source":"cache","preview":true})).await;
    assert_eq!(list["data"]["emails"][0]["flags"], json!(["Seen"]));
    assert_eq!(list["data"]["emails"][0]["preview_status"], "ok");
    let single = read(
        "get_email",
        json!({"uid":7,"uid_validity":9,"read_source":"cache","html_display":"readable"}),
    )
    .await;
    assert_eq!(single["data"]["body"], warm["data"]["body"], "{single}");
    assert_eq!(single["data"]["cache"]["scopes"][0]["uid_validity"], 9);
    let batch = read(
        "get_emails",
        json!({"uids":[7,8,7],"uid_validity":9,"read_source":"cache","body_mode":"trimmed"}),
    )
    .await;
    assert_eq!(
        batch["data"]["items"][0]["email"],
        batch["data"]["items"][2]["email"]
    );
    assert_eq!(batch["data"]["items"][1]["error"]["code"], "CACHE_MISS");
    for query in ["中文正文", "检索"] {
        let found = read("search_emails", json!({"query":query,"search_source":"cache","seen":true,"since":"2025-02-07","before":"2025-02-08"})).await;
        assert_eq!(found["data"]["emails"][0]["uid"], 7, "{found}");
        assert_eq!(found["data"]["emails"][0]["cache_match"]["body"], true);
    }
    let found = read(
        "locate_message",
        json!({"message_id":"<full@example.test>","mailboxes":["INBOX"],"search_source":"cache"}),
    )
    .await;
    assert_eq!(found["data"]["matches"][0]["uid"], 7, "{found}");
    assert_eq!(h.connections(), before);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn default_live_checks_exact_uid_before_using_cached_body_and_tombstones_deletion() {
    let root = tempfile::tempdir().unwrap();
    let path = full_configuration(root.path(), &[("default", &["INBOX"])]);
    let h = CacheHarness::new(
        [(
            "INBOX",
            Folder::new("", 9, [(7, synthetic(7, "uid fence"))]),
        )],
        512 * 1024,
    )
    .await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let read = |name, args| h.call_tool(Some(&app), "alice", name, args);
    let args = json!({"uid":7,"uid_validity":9});
    assert_eq!(
        read("get_email", args.clone()).await["data"]["body"],
        "synthetic content"
    );
    wait_cached_body(&h, &app, 7, "synthetic content").await;
    let mut last = Value::Null;
    tokio::time::timeout(CACHE_OBSERVATION_WAIT, async {
        loop {
            let old_connections = h.connections();
            let first_command = h.commands().len();
            last = read("get_email", args.clone()).await;
            assert_eq!(last["data"]["body"], "synthetic content", "{last}");
            match last["data"]["read_diagnostics"]["source"].as_str() {
                Some("cache") => {
                    assert_eq!(h.connections(), old_connections + 1);
                    let trace = h.commands();
                    let commands: Vec<_> = trace[first_command..]
                        .iter()
                        .map(|command| command.text.as_str())
                        .collect();
                    assert_eq!(
                        commands,
                        ["EXAMINE \"INBOX\"", "UID FETCH 7 (UID RFC822.SIZE)"]
                    );
                    break;
                }
                // Even a materialized body may miss the optional 100 ms
                // cache probe under load and take the valid live parts path.
                Some("bodystructure") => tokio::time::sleep(Duration::from_millis(10)).await,
                _ => panic!("unexpected live body source: {last}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("live exact UID check did not reuse the ready cached body: {last}"));
    h.update("INBOX", |folder| {
        folder.next = Some(8);
        folder.messages.clear();
    });
    assert_eq!(
        read("get_email", args).await["error"]["code"],
        "MAIL_BACKEND"
    );
    let offline = json!({"uid":7,"uid_validity":9,"read_source":"cache"});
    let missing = read("get_email", offline.clone()).await;
    assert!(
        matches!(
            missing["error"]["code"].as_str(),
            Some("CACHE_MISS" | "CACHE_NEEDS_VERIFICATION")
        ),
        "{missing}"
    );
    wait_cache_barrier(&h, &app).await;
    assert_eq!(
        read("get_email", offline).await["error"]["code"],
        "CACHE_MISS"
    );
    h.update("INBOX", |folder| {
        folder.next = Some(9);
        folder.messages.insert(8, synthetic(8, "未观察头部"));
        folder.sizes.insert(8, 33 * 1024 * 1024);
    });
    h.fault(
        "INBOX",
        "UID FETCH 8 (UID BODYSTRUCTURE)",
        Fault::NoAfterData,
    );
    let oversized = read("get_email", json!({"uid":8,"uid_validity":9})).await;
    assert_eq!(oversized["error"]["code"], "LIMIT_EXCEEDED", "{oversized}");
    wait_cache_barrier(&h, &app).await;
    let local = read("list_emails", json!({"read_source":"cache"})).await;
    assert_eq!(local["data"]["emails"], json!([]), "{local}");
    assert_eq!(
        local["data"]["cache"]["scopes"][0]["coverage"]["headers_missing"],
        1
    );
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn raw_derived_attachment_keeps_binary_bytes_indices_and_acl_after_imap_stops() {
    let root = tempfile::tempdir().unwrap();
    let path = full_configuration(root.path(), &[("default", &["INBOX"])]);
    let raw = b"Subject: Synthetic\r\nContent-Type: multipart/mixed; boundary=m\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\nbody\r\n--m\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=same.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAP9h\r\n--m\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=same.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAP5i\r\n--m--\r\n".to_vec();
    let h = CacheHarness::new([("INBOX", Folder::new("", 9, [(7, raw)]))], 512 * 1024).await;
    let (app, runtime, cache, witness, catalog) = h.full_cache_router(&path);
    h.fault(
        "INBOX",
        "UID FETCH 7 (UID BODYSTRUCTURE)",
        Fault::NoAfterData,
    );
    let read = |name, args| h.call_tool(Some(&app), "alice", name, args);
    let live = read("list_attachments", json!({"uid":7,"uid_validity":9})).await;
    assert_eq!(
        live["data"]["attachments"].as_array().map(Vec::len),
        Some(2),
        "{live}"
    );
    let _ = read("get_cache_status", json!({})).await;
    let before = h.connections();
    h.stop_imap();
    let items = read(
        "list_attachments",
        json!({"uid":7,"uid_validity":9,"read_source":"cache"}),
    )
    .await;
    let items = items["data"]["attachments"]
        .as_array()
        .unwrap_or_else(|| panic!("{items}"));
    assert_eq!(items[0]["filename"], items[1]["filename"]);
    assert_ne!(items[0]["attachment_index"], items[1]["attachment_index"]);
    let args = json!({"uid":7,"uid_validity":9,"attachment_id":items[1]["attachment_id"],"read_source":"cache"});
    let inline = read("download_attachment", args.clone()).await;
    assert_eq!(inline["data"]["base64"], "AP5i");
    let prepared = read("prepare_attachment_download", args).await;
    let url = prepared["data"]["download_url"].as_str().unwrap();
    assert_eq!(
        h.attachment(&app, "bob", url).await.status(),
        axum::http::StatusCode::NOT_FOUND
    );
    let response = h.attachment(&app, "alice", url).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "3");
    assert_eq!(
        response.headers()["x-content-sha256"],
        inline["data"]["checksum"]["value"].as_str().unwrap()
    );
    assert_eq!(
        &axum::body::to_bytes(response.into_body(), 3).await.unwrap()[..],
        &[0, 254, b'b']
    );
    let unfinished = h.attachment(&app, "alice", url).await;
    assert_eq!(unfinished.status(), axum::http::StatusCode::OK);
    catalog.revoke_read_for_test("default", "alice").unwrap();
    assert_eq!(
        h.attachment(&app, "alice", url).await.status(),
        axum::http::StatusCode::NOT_FOUND
    );
    assert_eq!(h.connections(), before);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(unfinished);
    drop(runtime);
    drop(witness);
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let status = h
        .call_tool(Some(&app), "alice", "get_cache_status", json!({}))
        .await;
    assert_eq!(
        status["data"]["storage"]["startup_unclean"], true,
        "{status}"
    );
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn large_remote_raw_stays_omitted_while_small_attachment_is_materialized_and_dirty_gated() {
    let root = tempfile::tempdir().unwrap();
    let path = full_configuration(root.path(), &[("default", &["INBOX"])]);
    let raw = b"Subject: bounded part\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=payload.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAAH+f/8=".to_vec();
    let mut folder = Folder::new("", 9, [(7, raw)]);
    folder.sizes.insert(7, 33 * 1024 * 1024);
    folder.structures.insert(7, r#"("APPLICATION" "OCTET-STREAM" NIL NIL NIL "BASE64" 8 NIL ("ATTACHMENT" ("FILENAME" "payload.bin")))"#.into());
    let h = CacheHarness::new([("INBOX", folder)], 512 * 1024).await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let read = |name, args| h.call_tool(Some(&app), "alice", name, args);
    let mut args = json!({"uid":7,"uid_validity":9,"attachment_index":0});
    let prepared = read("prepare_attachment_download", args.clone()).await;
    let url = prepared["data"]["download_url"]
        .as_str()
        .unwrap_or_else(|| panic!("{prepared}"));
    let _ = read("get_cache_status", json!({})).await;
    let missing = read(
        "list_attachments",
        json!({"uid":7,"uid_validity":9,"read_source":"cache"}),
    )
    .await;
    assert_eq!(
        missing["data"]["attachments"][0]["cache_state"], "missing",
        "{missing}"
    );
    assert_eq!(
        missing["data"]["attachments"][0]["delivery"]["available_modes"],
        json!([])
    );
    let response = h.attachment(&app, "alice", url).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        &axum::body::to_bytes(response.into_body(), 5).await.unwrap()[..],
        &[0, 1, 254, 127, 255]
    );
    let status = read("get_cache_status", json!({})).await;
    let layers = &status["data"]["storage"]["folders"][0]["layers"];
    assert_eq!(layers["raw"]["omitted"], 1, "{status}");
    assert_eq!(layers["attachments"]["materialized"], 1, "{status}");
    assert!(
        !h.commands()
            .iter()
            .any(|command| command.text.contains("BODY.PEEK[]"))
    );
    let before = h.connections();
    args["read_source"] = json!("cache");
    let available = read(
        "list_attachments",
        json!({"uid":7,"uid_validity":9,"read_source":"cache"}),
    )
    .await;
    assert_eq!(
        available["data"]["attachments"][0]["cache_state"], "materialized",
        "{available}"
    );
    assert_eq!(
        available["data"]["attachments"][0]["delivery"]["available_modes"],
        json!(["inline_base64", "authenticated_http"])
    );
    let prepared = read("prepare_attachment_download", args.clone()).await;
    let url = prepared["data"]["download_url"]
        .as_str()
        .unwrap_or_else(|| panic!("{prepared}"));
    let response = h.attachment(&app, "alice", url).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        &axum::body::to_bytes(response.into_body(), 5).await.unwrap()[..],
        &[0, 1, 254, 127, 255]
    );
    assert_eq!(h.connections(), before);
    let marked = read(
        "mark_email",
        json!({"uid":7,"uid_validity":9,"flag":"seen","value":false}),
    )
    .await;
    assert_eq!(marked["data"]["status"], "updated", "{marked}");
    let before = h.connections();
    let denied = h.attachment(&app, "alice", url).await;
    assert_eq!(denied.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        read("download_attachment", args).await["error"]["code"],
        "CACHE_NEEDS_VERIFICATION"
    );
    assert_eq!(h.connections(), before);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn tagged_attachment_fetch_absence_after_size_invalidates_cached_copy_and_handle() {
    use axum::http::StatusCode;

    let root = tempfile::tempdir().unwrap();
    let path = full_configuration(root.path(), &[("default", &["INBOX"])]);
    let raw = b"Subject: disappeared\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=payload.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAAH+f/8=".to_vec();
    let mut inbox = Folder::new("", 9, [(7, raw.clone()), (8, raw.clone()), (9, raw)]);
    inbox.structures.insert(7, r#"("APPLICATION" "OCTET-STREAM" NIL NIL NIL "BASE64" 8 NIL ("ATTACHMENT" ("FILENAME" "payload.bin")))"#.into());
    let h = CacheHarness::new([("INBOX", inbox)], 512 * 1024).await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let call = |name, args| h.call_tool(Some(&app), "alice", name, args);
    assert_eq!(
        call("list_emails", json!({})).await["data"]["emails"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let live = call(
        "prepare_attachment_download",
        json!({"uid":7,"uid_validity":9,"attachment_index":0}),
    )
    .await;
    let live_url = live["data"]["download_url"].as_str().unwrap();
    let warm = h.attachment(&app, "alice", live_url).await;
    assert_eq!(warm.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(warm.into_body(), 5)
            .await
            .unwrap()
            .len(),
        5
    );
    let _ = call("get_cache_status", json!({})).await;
    let local = call(
        "prepare_attachment_download",
        json!({"uid":7,"uid_validity":9,"attachment_index":0,"read_source":"cache"}),
    )
    .await;
    let local_url = local["data"]["download_url"].as_str().unwrap();
    let command = "UID FETCH 7 (UID BODY.PEEK[1]<0.65536>)";

    h.fault("INBOX", command, Fault::NoAfterData);
    assert_eq!(
        h.attachment(&app, "alice", live_url).await.status(),
        StatusCode::BAD_GATEWAY
    );
    let still_local = h.attachment(&app, "alice", local_url).await;
    assert_eq!(still_local.status(), StatusCode::OK);
    drop(still_local);

    h.fault("INBOX", command, Fault::NoDataOk);
    assert_eq!(
        h.attachment(&app, "alice", live_url).await.status(),
        StatusCode::NOT_FOUND
    );
    let denied = h.attachment(&app, "alice", local_url).await;
    assert!(matches!(
        denied.status(),
        StatusCode::CONFLICT | StatusCode::NOT_FOUND
    ));
    let _ = call("get_cache_status", json!({})).await;
    assert_eq!(
        h.attachment(&app, "alice", local_url).await.status(),
        StatusCode::NOT_FOUND
    );

    h.fault("INBOX", "UID FETCH 8 (UID BODYSTRUCTURE)", Fault::NoDataOk);
    let body = call("get_email", json!({"uid":8,"uid_validity":9})).await;
    assert_eq!(body["error"]["code"], "MAIL_BACKEND");
    h.fault("INBOX", "UID FETCH 9 (UID BODYSTRUCTURE)", Fault::NoDataOk);
    let parts = call("list_attachments", json!({"uid":9,"uid_validity":9})).await;
    assert_eq!(parts["error"]["code"], "NOT_FOUND");
    let _ = call("get_cache_status", json!({})).await;
    assert_eq!(
        call("list_emails", json!({"read_source":"cache"})).await["data"]["emails"],
        json!([])
    );
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn signed_cache_isolates_accounts_and_keeps_full_index_after_wire_truncation_and_backoff() {
    let root = tempfile::tempdir().unwrap();
    let path = full_configuration(
        root.path(),
        &[("default", &["INBOX"]), ("second", &["INBOX"])],
    );
    let second = CacheHarness::new(
        [("INBOX", Folder::new("", 9, [(7, synthetic(7, "另一账户"))]))],
        32 * 1024,
    )
    .await;
    let long = format!("{}末尾独有检索", "引号\\\"".repeat(8000));
    let raw = message(Some("large@example.test"), &[], None, "第一账户", &long);
    let h = CacheHarness::with_second_account(
        [("INBOX", Folder::new("", 9, [(7, raw)]))],
        32 * 1024,
        &second,
    )
    .await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let read = |name, args| h.call_tool(Some(&app), "alice", name, args);
    let first = read("get_email", json!({"uid":7,"uid_validity":9})).await;
    assert_eq!(first["data"]["subject"], "第一账户", "{first}");
    assert_eq!(first["data"]["body_truncated"], true);
    let next = read(
        "get_email",
        json!({"uid":7,"uid_validity":9,"account_id":"second"}),
    )
    .await;
    assert_eq!(next["data"]["subject"], "另一账户", "{next}");
    let _ = read("get_cache_status", json!({})).await;
    let _ = read("get_cache_status", json!({"account_id":"second"})).await;
    h.stop_imap();
    let _ = read("get_email_status", json!({"uid":7,"uid_validity":9})).await;
    let live = read("get_email", json!({"uid":7,"uid_validity":9})).await;
    assert_eq!(live["error"]["code"], "BUSY", "{live}");
    second.stop_imap();
    let old = (h.connections(), second.connections());
    let (response, wire) = h
        .call_named(
            &app,
            "alice",
            "get_email",
            json!({"uid":7,"uid_validity":9,"read_source":"cache"}),
            json!("\\\"".repeat(250)),
        )
        .await;
    assert!(wire <= 32 * 1024);
    assert_eq!(response["data"]["subject"], "第一账户");
    assert_eq!(response["data"]["body_truncated"], true);
    assert_eq!(response["data"]["body_original_bytes"], long.len());
    let indexed = read(
        "search_emails",
        json!({"search_source":"cache","query":"末尾独有检索"}),
    )
    .await;
    assert_eq!(indexed["data"]["emails"][0]["uid"], 7, "{indexed}");
    let next = read(
        "get_email",
        json!({"uid":7,"uid_validity":9,"account_id":"second","read_source":"cache"}),
    )
    .await;
    assert_eq!(next["data"]["subject"], "另一账户");
    let denied = h
        .call_tool(
            Some(&app),
            "bob",
            "get_email",
            json!({"uid":7,"uid_validity":9,"account_id":"second","read_source":"cache"}),
        )
        .await;
    assert_eq!(denied["error"]["code"], "ACCOUNT_UNAVAILABLE");
    assert_eq!((h.connections(), second.connections()), old);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn metadata_quota_preserves_live_bodies_and_reports_failed_writes() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path(), &[("default", &["INBOX"])]);
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    settings["accounts"][0]["max_bytes"] = json!(crate::cache::MIN_BYTES);
    std::fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
    let messages = std::array::from_fn::<_, 16, _>(|index| {
        let uid = index as u32 + 1;
        let mut raw = synthetic(uid, "bounded quota fixture");
        let mut header = b"X-Padding: ".to_vec();
        header.extend(std::iter::repeat_n(b'x', 100 * 1024));
        header.extend_from_slice(b"\r\n");
        raw.splice(0..0, header);
        (uid, raw)
    });
    let h = CacheHarness::new([("INBOX", Folder::new("", 9, messages))], 512 * 1024).await;
    let (app, cache) = h.cache_router(&path);
    let mut blocked = false;
    for uid in 1..=16 {
        let body = h
            .call_tool(
                Some(&app),
                "alice",
                "get_email",
                json!({"uid":uid,"uid_validity":9}),
            )
            .await;
        assert_eq!(body["data"]["body"], "synthetic content");
        let status = h
            .call_tool(Some(&app), "alice", "get_cache_status", json!({}))
            .await;
        assert_eq!(status["data"]["status"], "ready");
        if status["data"]["write_health"]["quota_blocked"] == true {
            assert_eq!(status["data"]["write_health"]["last_error"], "quota");
            assert!(
                status["data"]["write_health"]["failed_writes"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert!(
                status["data"]["storage"]["folders"][0]["complete_headers"]
                    .as_u64()
                    .unwrap()
                    < uid
            );
            blocked = true;
            break;
        }
    }
    assert!(
        blocked,
        "fixture did not reach the configured metadata quota"
    );
    drop(app);
    cache.finish().await;
}

#[tokio::test]
async fn off_has_no_storage_and_metadata_records_only_verified_complete_index_entries() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path(), &[("default", &["INBOX"])]);
    let mut partial = synthetic(8, "部分邮件");
    let mut long_field = b"X-Synthetic: ".to_vec();
    long_field.extend(std::iter::repeat_n(b'x', 9000));
    long_field.extend_from_slice(b"\r\n");
    partial.splice(0..0, long_field);
    let h = CacheHarness::new(
        [(
            "INBOX",
            Folder::new("", 9, [(7, synthetic(7, "完整主题")), (8, partial)]),
        )],
        512 * 1024,
    )
    .await;

    let off = h
        .call_tool(None, "alice", "get_cache_status", json!({}))
        .await;
    assert_eq!(off["data"]["status"], "off");
    assert_eq!(off["data"]["storage"], Value::Null);
    assert_eq!(h.connections(), 0);
    assert!(!root.path().join("metadata").exists());
    let off_path = root.path().join("off.json");
    std::fs::write(&off_path, br#"{"version":1,"mode":"off"}"#).unwrap();
    let (off_app, off_cache) = h.cache_router(&off_path);
    let explicit_off = h
        .call_tool(Some(&off_app), "alice", "get_cache_status", json!({}))
        .await;
    assert_eq!(explicit_off["data"], off["data"]);
    assert!(!root.path().join("metadata").exists());
    drop(off_app);
    off_cache.finish().await;

    let (app, cache) = h.cache_router(&path);
    let accounts = h
        .call_tool(Some(&app), "alice", "list_accounts", json!({}))
        .await;
    assert_eq!(
        accounts["data"]["accounts"][0]["capabilities"]["cache_modes"],
        json!(["off", "metadata"])
    );
    let list = h
        .call_tool(Some(&app), "alice", "list_emails", json!({}))
        .await;
    assert_eq!(list["data"]["emails"].as_array().unwrap().len(), 2);
    assert_eq!(
        list["data"]["emails"][0]["classification_status"],
        "partial"
    );
    let body = h
        .call_tool(
            Some(&app),
            "alice",
            "get_email",
            json!({"uid":7,"uid_validity":9}),
        )
        .await;
    assert_eq!(body["data"]["body"], "synthetic content");

    let before = h.connections();
    let status = h
        .call_tool(Some(&app), "alice", "get_cache_status", json!({}))
        .await;
    assert_eq!(status["data"]["status"], "ready");
    let inbox = &status["data"]["storage"]["folders"][0];
    assert_eq!(inbox["uid_validity"], 9);
    assert_eq!(inbox["high_uid"], 8);
    assert_eq!(inbox["messages"], 2);
    assert_eq!(inbox["complete_headers"], 1);
    assert_eq!(inbox["indexed_headers"], 1);
    assert_eq!(inbox["verified_parts"], 1);
    assert_eq!(h.connections(), before);
    drop(app);
    cache.finish().await;
}

#[tokio::test]
async fn cache_status_obeys_current_acl_and_isolates_account_and_folder_generation() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(
        root.path(),
        &[("default", &["INBOX"]), ("second", &["INBOX"])],
    );
    let second = CacheHarness::new(
        [(
            "INBOX",
            Folder::new("", 9, [(7, synthetic(7, "第二账户主题"))]),
        )],
        512 * 1024,
    )
    .await;
    let h = CacheHarness::with_second_account(
        [(
            "INBOX",
            Folder::new("", 9, [(7, synthetic(7, "第一账户主题"))]),
        )],
        512 * 1024,
        &second,
    )
    .await;
    let (app, cache) = h.cache_router(&path);
    let read = h
        .call_tool(
            Some(&app),
            "alice",
            "get_email",
            json!({"account_id":"second","uid":7,"uid_validity":9}),
        )
        .await;
    assert_eq!(read["data"]["subject"], "第二账户主题");
    let before = (h.connections(), second.connections());
    let blocked = h
        .call_tool(
            Some(&app),
            "bob",
            "get_cache_status",
            json!({"account_id":"second"}),
        )
        .await;
    assert_eq!(blocked["error"]["code"], "ACCOUNT_UNAVAILABLE");
    assert_eq!((h.connections(), second.connections()), before);
    let default = h
        .call_tool(Some(&app), "alice", "get_cache_status", json!({}))
        .await;
    assert_eq!(default["data"]["storage"]["folders"][0]["messages"], 0);
    let status = h
        .call_tool(
            Some(&app),
            "alice",
            "get_cache_status",
            json!({"account_id":"second"}),
        )
        .await;
    assert_eq!(status["data"]["storage"]["folders"][0]["messages"], 1);

    second.update("INBOX", |folder| folder.validity = 10);
    let stale = h
        .call_tool(
            Some(&app),
            "alice",
            "get_email",
            json!({"account_id":"second","uid":7,"uid_validity":9}),
        )
        .await;
    assert_eq!(stale["error"]["code"], "STALE_UID");
    let status = h
        .call_tool(
            Some(&app),
            "alice",
            "get_cache_status",
            json!({"account_id":"second"}),
        )
        .await;
    let inbox = &status["data"]["storage"]["folders"][0];
    assert_eq!(inbox["uid_validity"], 10);
    assert_eq!(inbox["messages"], 0);
    drop(app);
    cache.finish().await;
}

#[tokio::test]
async fn durable_cursor_survives_runtime_restart_and_restore_discards_it() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path(), &[("default", &["INBOX", "Sent"])]);
    let h = CacheHarness::new(
        [
            (
                "INBOX",
                Folder::new(
                    "",
                    9,
                    [(
                        7,
                        message(Some("seed@example.test"), &[], None, "seed", "seed"),
                    )],
                ),
            ),
            (
                "Sent",
                Folder::new(
                    "\\Sent",
                    10,
                    [(
                        7,
                        message(
                            Some("reply@example.test"),
                            &["seed@example.test"],
                            None,
                            "reply",
                            "reply",
                        ),
                    )],
                ),
            ),
        ],
        512 * 1024,
    )
    .await;
    let (app, cache) = h.cache_router(&path);
    let first = h
        .call_tool(
            Some(&app),
            "alice",
            "read_conversation",
            json!({"uid":7,"uid_validity":9,"mailboxes":["INBOX","Sent"],"limit":1}),
        )
        .await;
    let cursor = first["data"]["cursor"].as_str().unwrap();
    assert_eq!(first["data"]["cursor_storage"], "persistent");
    drop(app);
    cache.finish().await;

    let (app, cache) = h.cache_router(&path);
    let resumed = h
        .call_tool(
            Some(&app),
            "alice",
            "read_conversation",
            json!({"cursor":cursor}),
        )
        .await;
    assert_eq!(
        resumed["data"]["messages"][0]["variants"][0]["representative"]["mailbox"],
        "Sent"
    );
    assert_eq!(resumed["data"]["complete"], true);
    assert_eq!(resumed["data"]["cursor_storage"], "persistent");
    drop(app);
    cache.finish().await;

    let (app, cache) = h.cache_router(&path);
    cache.mark_restored().unwrap();
    let rejected = h
        .call_tool(
            Some(&app),
            "alice",
            "read_conversation",
            json!({"cursor":cursor}),
        )
        .await;
    assert_eq!(rejected["error"]["code"], "CURSOR_UNAVAILABLE");
    let _ = h
        .call_tool(
            Some(&app),
            "alice",
            "get_email",
            json!({"uid":7,"uid_validity":9}),
        )
        .await;
    let status = h
        .call_tool(Some(&app), "alice", "get_cache_status", json!({}))
        .await;
    assert_eq!(status["data"]["storage"]["folders"][0]["dirty"], true);
    drop(app);
    cache.finish().await;
}

#[tokio::test]
async fn unavailable_metadata_storage_does_not_fail_the_live_read() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path(), &[("default", &["INBOX"])]);
    let h = CacheHarness::new(
        [
            ("INBOX", Folder::new("", 9, [(7, synthetic(7, "在线正文"))])),
            ("Sent", Folder::new("\\Sent", 10, [])),
        ],
        512 * 1024,
    )
    .await;
    let (owner_app, owner) = h.cache_router(&path);
    let (degraded_app, degraded) = h.cache_router(&path);
    let status = h
        .call_tool(Some(&degraded_app), "alice", "get_cache_status", json!({}))
        .await;
    assert_eq!(status["data"]["status"], "unavailable");
    let read = h
        .call_tool(
            Some(&degraded_app),
            "alice",
            "get_email",
            json!({"uid":7,"uid_validity":9}),
        )
        .await;
    assert_eq!(read["data"]["body"], "synthetic content");
    let rejected = h
        .call_tool(
            Some(&degraded_app),
            "alice",
            "read_conversation",
            json!({"uid":7,"uid_validity":9,"mailboxes":["INBOX"]}),
        )
        .await;
    assert_eq!(rejected["error"]["code"], "CURSOR_UNAVAILABLE");
    let mixed = h
        .call_tool(
            Some(&degraded_app),
            "alice",
            "read_conversation",
            json!({"uid":7,"uid_validity":9,"mailboxes":["INBOX","Sent"]}),
        )
        .await;
    assert_eq!(mixed["data"]["cursor_storage"], "memory");
    drop(degraded_app);
    degraded.finish().await;
    drop(owner_app);
    owner.finish().await;
}
