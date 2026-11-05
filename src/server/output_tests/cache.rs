//! Signed MCP acceptance of the metadata cache in the real account router.
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
