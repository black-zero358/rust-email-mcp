//! Two isolated mailboxes share names and UIDs; real JWTs select their current ACL.
use super::attachments::{Credentials, assert_http_error, get_attachment, router};
use super::*;
use crate::{
    accounts::{DEFAULT_ACCOUNT_ID, LEGACY_REGISTRATION_KEY},
    config::{AccountConfigSource, ValidatedAccountConfig},
};
use axum::http::{StatusCode, header};
use std::sync::atomic::Ordering;

async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fixture state did not settle");
}

fn account_message(marker: &str) -> Vec<u8> {
    let subject = if marker.len() <= 128 && !marker.contains(['\r', '\n']) {
        marker
    } else {
        "synthetic-account"
    };
    format!("Message-ID: <same@example.test>\r\nSubject: {subject}\r\nContent-Type: multipart/mixed; boundary=account\r\n\r\n--account\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{marker}\r\n--account\r\nContent-Type: text/csv\r\nContent-Disposition: attachment; filename=same.csv\r\n\r\n{marker}\r\n--account--\r\n").into_bytes()
}
struct MultiAccount {
    a: Harness,
    b: Harness,
    catalog: Arc<AccountCatalog>,
    runtime: Arc<MailRuntime>,
    journal: Arc<Journal>,
    credentials: Credentials,
    app: Router,
}
impl MultiAccount {
    async fn new(max_output: usize) -> Self {
        Self::with_bodies(max_output, "account-a", "account-b").await
    }
    async fn with_bodies(max_output: usize, a_body: &str, b_body: &str) -> Self {
        Self::with_default_read(max_output, a_body, b_body, DEFAULT_ACCOUNT_ID).await
    }
    async fn with_default_read(
        max_output: usize,
        a_body: &str,
        b_body: &str,
        default_read: &str,
    ) -> Self {
        Self::configured(max_output, a_body, b_body, default_read, None).await
    }
    async fn configured(
        max_output: usize,
        a_body: &str,
        b_body: &str,
        default_read: &str,
        fatal_a_uid: Option<u32>,
    ) -> Self {
        let a = Harness::new(
            BTreeMap::from([(7, account_message(a_body))]),
            fatal_a_uid,
            max_output,
        )
        .await;
        let b = Harness::new(
            BTreeMap::from([(7, account_message(b_body))]),
            None,
            max_output,
        )
        .await;
        let mut configuration = (**a.service.config()).clone();
        configuration.subjects = ["alice", "bob", "both", "journal_only"]
            .map(str::to_owned)
            .to_vec();
        let configuration = Arc::new(configuration);
        let mut second = (**b.service.config()).clone();
        second.mode = "read".into();
        second.address = "second@example.test".into();
        let set = ValidatedAccountSet {
            service_config: configuration.clone(),
            default_read_account: default_read.into(),
            source: AccountConfigSource::ExtendLegacy,
            accounts: vec![
                ValidatedAccountConfig {
                    registration_key: LEGACY_REGISTRATION_KEY.into(),
                    alias: DEFAULT_ACCOUNT_ID.into(),
                    display_name: "Original".into(),
                    config: configuration.clone(),
                    read_subjects: vec!["alice".into(), "both".into()],
                    write_subjects: vec!["alice".into()],
                    is_legacy: true,
                },
                ValidatedAccountConfig {
                    registration_key: "billing-registration".into(),
                    alias: "billing-B".into(),
                    display_name: "Billing".into(),
                    config: Arc::new(second),
                    read_subjects: vec!["bob".into(), "both".into()],
                    write_subjects: vec![],
                    is_legacy: false,
                },
            ],
        };
        let catalog = AccountCatalog::from_test_accounts(set).unwrap();
        let journal = Arc::new(Journal::open(std::path::Path::new(":memory:")).unwrap());
        let mut roots = a.roots.clone();
        roots.roots.extend(b.roots.roots.clone());
        let runtime = Arc::new(
            MailRuntime::new(catalog.clone(), journal.clone())
                .unwrap()
                .with_test_imap_roots(roots),
        );
        let credentials = Credentials::new(configuration);
        let app = router(runtime.clone(), credentials.auth.clone());
        Self {
            a,
            b,
            catalog,
            runtime,
            journal,
            credentials,
            app,
        }
    }
    fn token(&self, principal: &str) -> String {
        self.credentials.changed("sub", json!(principal))
    }
    fn connections(&self) -> (usize, usize) {
        (
            self.a.connections.load(Ordering::SeqCst),
            self.b.connections.load(Ordering::SeqCst),
        )
    }
    async fn rpc(&self, principal: &str, method: &str, params: Value) -> Value {
        let response = self.app.clone().oneshot(Request::post("/mcp").header("Host","localhost")
            .header(header::AUTHORIZATION,format!("Bearer {}",self.token(principal))).header(header::CONTENT_TYPE,"application/json")
            .header(header::ACCEPT,"application/json, text/event-stream").header("MCP-Protocol-Version","2025-06-18")
            .body(Body::from(json!({"jsonrpc":"2.0","id":"\\\"中".repeat(60),"method":method,"params":params}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let wire: Value = serde_json::from_slice(&bytes).unwrap();
        if let Some(content) = wire.pointer("/result/structuredContent") {
            assert_eq!(content["request_id"], request_id);
            assert!(bytes.len() <= self.runtime.config().max_output);
            let text: Value =
                serde_json::from_str(wire["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(&text, content);
        }
        wire
    }
    async fn call(&self, principal: &str, name: &str, args: Value) -> Value {
        let wire = self
            .rpc(
                principal,
                "tools/call",
                json!({"name":name,"arguments":args}),
            )
            .await;
        assert!(wire.get("error").is_none(), "{wire}");
        let content = wire["result"]["structuredContent"].clone();
        if is_read(name) {
            assert_contract(name, &content);
        }
        content
    }
}

#[tokio::test]
async fn cancelled_http_reads_release_admission_backend_slots_and_connections() {
    let h = MultiAccount::new(64 * 1024).await;
    h.a.stall_greeting.store(true, Ordering::SeqCst);
    let mut requests = Vec::new();
    for _ in 0..20 {
        let app = h.app.clone();
        let token = h.token("alice");
        requests.push(tokio::spawn(async move {
            attachments::call(
                &app,
                &token,
                "get_email_status",
                json!({"uid":7,"uid_validity":9}),
            )
            .await
        }));
    }
    wait_for(|| h.a.active_connections.load(Ordering::SeqCst) == 4).await;
    // All four connections are blocked at the greeting. The remaining sixteen
    // calls must occupy queue admission before the overflow call is rejected.
    let overflow = tokio::time::timeout(
        Duration::from_secs(1),
        h.call(
            "alice",
            "get_email_status",
            json!({"uid":7,"uid_validity":9}),
        ),
    )
    .await
    .expect("all twenty calls should already be admitted");
    assert_eq!(overflow["error"]["code"], "BUSY");
    assert_eq!(h.connections(), (4, 0));
    for request in &requests {
        request.abort();
    }
    for request in requests {
        assert!(request.await.unwrap_err().is_cancelled());
    }
    wait_for(|| h.a.active_connections.load(Ordering::SeqCst) == 0).await;
    // Cancelled waiters must never start a replacement connection after one of
    // the original running calls releases a backend slot.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.connections(), (4, 0));
    h.a.stall_greeting.store(false, Ordering::SeqCst);
    let next = h
        .call(
            "alice",
            "get_email_status",
            json!({"uid":7,"uid_validity":9}),
        )
        .await;
    assert_eq!(next["data"]["uid"], 7);
    assert_eq!(h.connections(), (5, 0));
}

#[tokio::test]
async fn in_flight_attachment_gets_stop_on_disconnect_revocation_and_account_removal() {
    let h = MultiAccount::new(64 * 1024).await;
    let prepared = h
        .call(
            "alice",
            "prepare_attachment_download",
            json!({"uid":7,"uid_validity":9,"attachment_index":1}),
        )
        .await;
    let url = prepared["data"]["download_url"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(|| h.a.active_connections.load(Ordering::SeqCst) == 0).await;
    h.a.stall_greeting.store(true, Ordering::SeqCst);
    let start_get = || {
        let app = h.app.clone();
        let token = h.token("alice");
        let url = url.clone();
        tokio::spawn(async move { get_attachment(&app, Some(&token), &url).await })
    };
    let disconnected = start_get();
    wait_for(|| h.a.active_connections.load(Ordering::SeqCst) == 1).await;
    disconnected.abort();
    assert!(disconnected.await.unwrap_err().is_cancelled());
    wait_for(|| h.a.active_connections.load(Ordering::SeqCst) == 0).await;
    let revoked = start_get();
    wait_for(|| h.a.active_connections.load(Ordering::SeqCst) == 1).await;
    h.catalog.revoke_read_for_test("default", "alice").unwrap();
    assert_http_error(
        tokio::time::timeout(Duration::from_secs(1), revoked)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::NOT_FOUND,
        "ATTACHMENT_HANDLE_NOT_FOUND",
        "handle_lookup",
    )
    .await;
    wait_for(|| h.a.active_connections.load(Ordering::SeqCst) == 0).await;
    h.a.stall_greeting.store(false, Ordering::SeqCst);
    assert_eq!(
        h.call(
            "both",
            "get_email_status",
            json!({"uid":7,"uid_validity":9})
        )
        .await["data"]["uid"],
        7
    );

    let prepared = h
        .call(
            "bob",
            "prepare_attachment_download",
            json!({"account_id":"billing-B","uid":7,"uid_validity":9,"attachment_index":1}),
        )
        .await;
    let url = prepared["data"]["download_url"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(|| h.b.active_connections.load(Ordering::SeqCst) == 0).await;
    h.b.stall_greeting.store(true, Ordering::SeqCst);
    let app = h.app.clone();
    let token = h.token("bob");
    let removed = tokio::spawn(async move { get_attachment(&app, Some(&token), &url).await });
    wait_for(|| h.b.active_connections.load(Ordering::SeqCst) == 1).await;
    h.catalog.remove_account_for_test("billing-B").unwrap();
    assert_http_error(
        tokio::time::timeout(Duration::from_secs(1), removed)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::NOT_FOUND,
        "ATTACHMENT_HANDLE_NOT_FOUND",
        "handle_lookup",
    )
    .await;
    wait_for(|| h.b.active_connections.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn discovery_is_principal_scoped_and_journal_remains_visible_without_mailbox_acl() {
    let h = MultiAccount::new(64 * 1024).await;
    for (principal, expected, write) in [
        ("alice", vec!["default"], true),
        ("bob", vec!["billing-B"], false),
        ("both", vec!["billing-B", "default"], false),
        ("journal_only", vec![], false),
    ] {
        let accounts = h.call(principal, "list_accounts", json!({})).await;
        let mut aliases = accounts["data"]["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|account| account["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        aliases.sort_unstable();
        assert_eq!(aliases, expected);
        assert_eq!(
            accounts["data"]["default_account_id"].is_null(),
            matches!(principal, "bob" | "journal_only")
        );
        let tools = h.rpc(principal, "tools/list", json!({})).await;
        let names = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(names.contains(&"list_accounts") && names.contains(&"get_operation"));
        assert_eq!(names.contains(&"get_email"), principal != "journal_only");
        assert_eq!(names.contains(&"mark_email"), write);
        if principal == "journal_only" {
            assert_eq!(names.len(), 2);
        }
    }
    h.journal
        .claim("journal_only", "old", "hash", "<old@example.test>")
        .unwrap();
    h.journal
        .finish("journal_only", "old", "unknown", &["legacy-warning".into()])
        .unwrap();
    assert_eq!(
        h.call(
            "journal_only",
            "get_operation",
            json!({"operation_key":"old"})
        )
        .await["data"]["operation"]["status"],
        "unknown"
    );
    assert!(
        h.call("bob", "get_operation", json!({"operation_key":"old"}))
            .await["data"]["operation"]
            .is_null()
    );
    assert_eq!(h.connections(), (0, 0));
}

#[tokio::test]
async fn duplicate_folder_uid_and_validity_reads_stay_in_the_authorized_account() {
    let h = MultiAccount::new(64 * 1024).await;
    for (principal, account, marker) in [
        ("alice", "default", "account-a"),
        ("bob", "billing-B", "account-b"),
        ("both", "default", "account-a"),
        ("both", "billing-B", "account-b"),
    ] {
        let value = h
            .call(
                principal,
                "get_email",
                json!({"account_id":account,"mailbox":"INBOX","uid":7,"uid_validity":9}),
            )
            .await;
        assert_eq!(value["data"]["account_id"], account);
        assert_eq!(value["data"]["mailbox"], "INBOX");
        assert!(value["data"]["body"].as_str().unwrap().contains(marker));
        let list = h
            .call(principal, "list_emails", json!({"account_id":account}))
            .await;
        assert_eq!(list["data"]["emails"][0]["account_id"], account);
        assert_eq!(list["data"]["emails"][0]["uid_validity"], 9);
        assert_eq!(list["data"]["emails"][0]["subject"], marker);
    }
    assert!(
        h.call("alice", "get_email", json!({"uid":7,"uid_validity":9}))
            .await["data"]["body"]
            .as_str()
            .unwrap()
            .contains("account-a")
    );
    let before = h.connections();
    for (principal, account) in [
        ("alice", "billing-B"),
        ("bob", "default"),
        ("both", "private-missing"),
        ("journal_only", "default"),
    ] {
        let error = h
            .call(
                principal,
                "get_email",
                json!({"account_id":account,"uid":7,"uid_validity":9}),
            )
            .await;
        assert_eq!(error["error"]["code"], "ACCOUNT_UNAVAILABLE");
        assert!(!error.to_string().contains(account));
    }
    assert_eq!(
        h.call("bob", "get_email", json!({"uid":7,"uid_validity":9}))
            .await["error"]["code"],
        "ACCOUNT_UNAVAILABLE"
    );
    assert_eq!(h.connections(), before);
}

#[tokio::test]
async fn configured_read_default_changes_omitted_reads_while_legacy_writes_remain_original() {
    let h = MultiAccount::with_default_read(64 * 1024, "account-a", "account-b", "billing-B").await;
    let value = h
        .call("bob", "get_email", json!({"uid":7,"uid_validity":9}))
        .await;
    assert_eq!(value["data"]["account_id"], "billing-B");
    let before = h.connections();
    assert_eq!(
        h.call("alice", "get_email", json!({"uid":7,"uid_validity":9}))
            .await["error"]["code"],
        "ACCOUNT_UNAVAILABLE"
    );
    assert_eq!(h.connections(), before);
    assert!(
        h.call("alice", "list_accounts", json!({})).await["data"]["default_account_id"].is_null()
    );
    h.a.allow_writes.store(true, Ordering::SeqCst);
    assert_eq!(
        h.call(
            "alice",
            "mark_email",
            json!({"uid":7,"uid_validity":9,"flag":"seen","value":false})
        )
        .await["data"]["status"],
        "updated"
    );
    assert_eq!(h.a.writes.load(Ordering::SeqCst), 2);
    assert_eq!(h.b.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn attachment_handles_and_cached_reads_recheck_revocation_and_legacy_writes_stay_default() {
    let h = MultiAccount::new(64 * 1024).await;
    let a = h
        .call(
            "both",
            "list_attachments",
            json!({"account_id":"default","uid":7,"uid_validity":9}),
        )
        .await;
    let b = h
        .call(
            "both",
            "list_attachments",
            json!({"account_id":"billing-B","uid":7,"uid_validity":9}),
        )
        .await;
    assert_ne!(
        a["data"]["attachments"][0]["attachment_id"],
        b["data"]["attachments"][0]["attachment_id"]
    );
    let prepared = h
        .call(
            "alice",
            "prepare_attachment_download",
            json!({"uid":7,"uid_validity":9,"attachment_index":1}),
        )
        .await;
    let url = prepared["data"]["download_url"].as_str().unwrap();
    let cross = h.call("both","download_attachment",json!({"account_id":"billing-B","uid":7,"uid_validity":9,"attachment_id":a["data"]["attachments"][0]["attachment_id"]})).await;
    assert_eq!(cross["error"]["code"], "ATTACHMENT_NOT_FOUND");
    h.a.allow_writes.store(true, Ordering::SeqCst);
    let writes = h
        .call(
            "alice",
            "mark_email",
            json!({"uid":7,"uid_validity":9,"flag":"seen","value":false}),
        )
        .await;
    assert_eq!(writes["data"]["status"], "updated");
    assert_eq!(h.a.writes.load(Ordering::SeqCst), 2);
    assert_eq!(h.b.writes.load(Ordering::SeqCst), 0);
    let explicit = h
        .call(
            "alice",
            "mark_email",
            json!({"account_id":"billing-B","uid":7,"uid_validity":9,"flag":"seen","value":false}),
        )
        .await;
    assert_eq!(explicit["error"]["code"], "INVALID_INPUT");
    let disabled = h.rpc("bob","tools/call",json!({"name":"mark_email","arguments":{"uid":7,"uid_validity":9,"flag":"seen","value":false}})).await;
    assert!(disabled.get("error").is_some());
    h.call(
        "alice",
        "read_conversation",
        json!({"uid":7,"uid_validity":9}),
    )
    .await;
    h.catalog.revoke_read_for_test("default", "alice").unwrap();
    let before = h.connections();
    assert_eq!(
        h.call(
            "alice",
            "read_conversation",
            json!({"uid":7,"uid_validity":9})
        )
        .await["error"]["code"],
        "ACCOUNT_UNAVAILABLE"
    );
    assert_http_error(
        get_attachment(&h.app, Some(&h.token("alice")), url).await,
        StatusCode::NOT_FOUND,
        "ATTACHMENT_HANDLE_NOT_FOUND",
        "handle_lookup",
    )
    .await;
    assert_eq!(h.connections(), before);
    assert!(
        h.call("both", "get_email", json!({"uid":7,"uid_validity":9}))
            .await
            .get("data")
            .is_some()
    );
    h.catalog.remove_account_for_test("billing-B").unwrap();
    assert_eq!(
        h.call(
            "both",
            "get_email",
            json!({"account_id":"billing-B","uid":7,"uid_validity":9})
        )
        .await["error"]["code"],
        "ACCOUNT_UNAVAILABLE"
    );
}

#[tokio::test]
async fn backend_failure_backoff_is_local_to_one_account_and_never_retries() {
    let h = MultiAccount::configured(
        64 * 1024,
        "account-a",
        "account-b",
        DEFAULT_ACCOUNT_ID,
        None,
    )
    .await;
    h.a.reject_login.store(true, Ordering::SeqCst);
    assert_eq!(
        h.call(
            "both",
            "get_email",
            json!({"account_id":"default","uid":7,"uid_validity":9})
        )
        .await["error"]["code"],
        "MAIL_BACKEND"
    );
    let before = h.connections();
    let busy = h
        .call(
            "both",
            "get_email_status",
            json!({"account_id":"default","uid":7,"uid_validity":9}),
        )
        .await;
    assert_eq!(busy["error"]["code"], "BUSY");
    assert_eq!(busy["error"]["retryable"], true);
    assert_eq!(h.connections(), before);
    let other = h
        .call(
            "both",
            "get_email",
            json!({"account_id":"billing-B","uid":7,"uid_validity":9}),
        )
        .await;
    assert!(
        other["data"]["body"]
            .as_str()
            .unwrap()
            .contains("account-b")
    );
    assert_eq!(h.a.connections.load(Ordering::SeqCst), before.0);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        h.call(
            "both",
            "get_email_status",
            json!({"account_id":"default","uid":7,"uid_validity":9})
        )
        .await["data"]["uid"],
        7
    );

    let local = MultiAccount::with_bodies(
        64 * 1024,
        "account-a\r\n--account\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: x-unknown\r\n\r\nopaque",
        "account-b",
    ).await;
    let malformed = local
        .call("alice", "get_email", json!({"uid":7,"uid_validity":9}))
        .await;
    assert_eq!(malformed["error"]["code"], "UNSUPPORTED_BODY");
    let after_local_error = local
        .call(
            "alice",
            "get_email_status",
            json!({"uid":7,"uid_validity":9}),
        )
        .await;
    assert_eq!(after_local_error["data"]["uid"], 7);
}

#[tokio::test]
async fn account_references_survive_full_wire_body_fitting_and_principal_journal_fallback() {
    let body = "中😀\"\\\n".repeat(12000);
    let h = MultiAccount::with_bodies(12000, &body, &body).await;
    for (name, args, path) in [
        (
            "get_emails",
            json!({"account_id":"billing-B","uids":[7],"uid_validity":9}),
            "/data/items/0/email",
        ),
        (
            "read_conversation",
            json!({"account_id":"billing-B","uid":7,"uid_validity":9}),
            "/data/messages/0",
        ),
    ] {
        let value = h.call("bob", name, args).await;
        assert_eq!(value["data"]["account_id"], "billing-B");
        assert_eq!(value.pointer(path).unwrap()["account_id"], "billing-B");
        assert_eq!(value.pointer(path).unwrap()["mailbox"], "INBOX");
        assert_eq!(value.pointer(path).unwrap()["body_truncated"], true);
    }
    h.journal
        .claim("journal_only", "big", "hash", "id")
        .unwrap();
    h.journal
        .finish(
            "journal_only",
            "big",
            "accepted",
            &["warning".repeat(10000)],
        )
        .unwrap();
    let value = h
        .call(
            "journal_only",
            "get_operation",
            json!({"operation_key":"big"}),
        )
        .await;
    assert_eq!(value["operation"]["status"], "accepted");
    assert_eq!(value["error"]["code"], "LIMIT_EXCEEDED");
}
