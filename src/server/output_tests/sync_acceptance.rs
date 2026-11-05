//! Signed, offline acceptance of the production background synchronization loop.
use super::{
    conversation::fixture::{Folder, Harness, Search, message},
    *,
};
use std::path::{Path, PathBuf};

const MIB: u64 = 1024 * 1024;
const WAIT: Duration = Duration::from_secs(25);

fn configuration(
    root: &Path,
    enabled: bool,
    max_bytes: u64,
    scopes: &[(&str, &[&str])],
) -> PathBuf {
    let path = root.join("cache.json");
    let accounts = scopes
        .iter()
        .map(|(account, folders)| {
            json!({"account_id":account,"mailboxes":folders,"max_bytes":max_bytes})
        })
        .collect::<Vec<_>>();
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,"mode":"full","directory":"cache","max_bytes":max_bytes,
            "accounts":accounts,"sync":{"enabled":enabled,"interval_seconds":1,"reconcile_interval_seconds":1}
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

fn synthetic(uid: u32, body: &str) -> Vec<u8> {
    message(
        Some(&format!("sync-{uid}@example.test")),
        &[],
        None,
        &format!("synthetic {uid}"),
        body,
    )
}

async fn status(h: &Harness, app: &axum::Router, account: &str) -> Value {
    let value = h
        .call_tool(
            Some(app),
            "alice",
            "get_cache_status",
            json!({"account_id":account}),
        )
        .await;
    value
        .get("data")
        .unwrap_or_else(|| panic!("cache status failed: {value}"))
        .clone()
}

fn scope_row<'a>(status: &'a Value, mailbox: &str) -> &'a Value {
    status["sync"]["folders"]
        .as_array()
        .and_then(|scopes| scopes.iter().find(|scope| scope["mailbox"] == mailbox))
        .unwrap_or_else(|| panic!("missing synchronization scope {mailbox}: {status}"))
}

fn scope<'a>(status: &'a Value, mailbox: &str) -> &'a Value {
    &scope_row(status, mailbox)["progress"]
}

async fn wait_scope(
    h: &Harness,
    app: &axum::Router,
    account: &str,
    mailbox: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = status(h, app, account).await;
        // A newly observed generation can fence out the old status snapshot
        // before the matching storage transaction completes.
        if status["status"] != "unavailable" {
            let scope = scope(&status, mailbox);
            if predicate(scope) {
                return scope.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{account}/{mailbox}: {status}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn cached_body(
    h: &Harness,
    app: &axum::Router,
    mailbox: &str,
    uid_validity: u32,
    uid: u32,
) -> Value {
    h.call_tool(
        Some(app), "alice", "get_email",
        json!({"mailbox":mailbox,"uid_validity":uid_validity,"uid":uid,"read_source":"cache","max_body_length":32}),
    ).await
}

fn staged_bytes(root: &Path) -> u64 {
    std::fs::read_dir(root.join("cache"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("blobs"))
        .filter(|path| path.is_dir())
        .flat_map(|path| std::fs::read_dir(path).unwrap())
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".stage-"))
        .map(|entry| entry.metadata().unwrap().len())
        .sum()
}

#[tokio::test]
async fn resumes_dense_and_sparse_history_without_absorbing_the_independent_new_uid() {
    const FROZEN: u32 = u32::MAX - 8;
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path(), true, 64 * MIB, &[("default", &["INBOX"])]);
    let mut folder = Folder::new("", 9, []);
    folder.generate(1..=501, |uid| synthetic(uid, "historical body"));
    folder
        .messages
        .insert(FROZEN, synthetic(FROZEN, "historical body"));
    folder.next = Some(FROZEN + 1);
    let h = Harness::new([("INBOX", folder)], 256 * 1024).await;
    let first = h.hold("INBOX", "UID SEARCH UID 1:200");
    let interrupted = h.hold("INBOX", "UID SEARCH UID 201:400");
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    first.entered().await;
    let incoming = FROZEN + 1;
    h.update("INBOX", |folder| {
        folder
            .messages
            .insert(incoming, synthetic(incoming, "new body"));
        folder.next = Some(incoming + 1);
    });
    first.release();
    interrupted.entered().await;
    let stopped = wait_scope(&h, &app, "default", "INBOX", |scope| {
        scope["history"]["next_low"] == 201
    })
    .await;
    assert_eq!(stopped["history"]["high_uid"], FROZEN);
    let scan_id = stopped["history"]["scan_id"].clone();
    drop(app);
    runtime.finish_cache(cache).await;
    interrupted.disconnected().await;
    drop(runtime);
    drop(witness);

    let before = h.commands().len();
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    let complete = wait_scope(&h, &app, "default", "INBOX", |scope| {
        scope["history"]["complete"] == true
            && scope["tail"]["complete"] == true
            && scope["tail"]["high_uid"] == incoming
    })
    .await;
    assert_eq!(complete["history"]["scan_id"], scan_id);
    assert_eq!(complete["history"]["high_uid"], FROZEN);
    assert_eq!(complete["history_coverage"]["known_messages"], 502);
    assert_eq!(complete["current_coverage"]["known_messages"], 503);
    let commands = h.commands();
    let windows = commands[before..]
        .iter()
        .filter_map(|command| Search::parse(&command.text))
        .collect::<Vec<_>>();
    assert!(
        windows
            .iter()
            .any(|window| window.start == 201 && window.end == 400)
    );
    assert!(windows.iter().all(|window| window.start != 1));
    assert_eq!(
        commands
            .iter()
            .filter_map(|command| Search::parse(&command.text))
            .filter(|window| window.start == incoming && window.end == incoming)
            .count(),
        1,
        "the independent tail may run before or after the history restart"
    );
    assert!(
        windows.len() < 40,
        "sparse discovery issued {} windows",
        windows.len()
    );
    assert!(
        commands
            .iter()
            .all(|command| !command.text.starts_with("SELECT "))
    );
    let metrics = h.metrics();
    assert!(metrics.commands >= commands.len() && metrics.completions <= metrics.commands);
    assert!(metrics.received_bytes > 0 && metrics.sent_bytes > metrics.received_bytes);
    assert!(metrics.active <= metrics.max_active && metrics.max_active > 0);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

async fn admission(runtime: &MailRuntime, expected: (usize, usize)) {
    tokio::time::timeout(WAIT, async {
        while runtime.admission_counts_for_test() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "admission {:?}, expected {expected:?}",
            runtime.admission_counts_for_test()
        )
    });
}

#[tokio::test]
async fn foreground_takes_the_released_slot_and_account_revocation_closes_background_work() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(
        root.path(),
        true,
        64 * MIB,
        &[("default", &["INBOX", "Archive"]), ("second", &["INBOX"])],
    );
    let second = Harness::new(
        [(
            "INBOX",
            Folder::new("", 15, [(7, synthetic(7, "second account"))]),
        )],
        256 * 1024,
    )
    .await;
    let messages: [_; 4] = std::array::from_fn(|index| {
        let uid = index as u32 + 1;
        (uid, synthetic(uid, "first account"))
    });
    let first = Arc::new(
        Harness::with_second_account(
            [
                ("INBOX", Folder::new("", 9, messages)),
                ("Archive", Folder::new("\\Archive", 10, [])),
            ],
            256 * 1024,
            &second,
        )
        .await,
    );
    let first_background = first.hold("INBOX", "UID SEARCH UID 1:4");
    let revoked_background = second.hold("INBOX", "UID SEARCH UID 1:7");
    let revoked_facts = second.hold(
        "INBOX",
        "UID FETCH 7 (UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER]<0.131073>)",
    );
    let (app, runtime, cache, witness, catalog) = first.full_cache_router(&path);
    runtime.start_sync();
    let default_first = tokio::select! {
        () = first_background.entered() => true,
        () = revoked_background.entered() => false,
    };
    assert_eq!(runtime.admission_counts_for_test(), (1, 1));

    let mut foreground = Vec::new();
    for uid in 1..=4 {
        let hold = first.hold("INBOX", &format!("UID FETCH {uid} (UID FLAGS)"));
        let h = first.clone();
        let app = app.clone();
        let task = tokio::spawn(async move {
            h.call_tool(
                Some(&app),
                "alice",
                "get_email_status",
                json!({"uid":uid,"uid_validity":9}),
            )
            .await
        });
        if uid < 4 {
            hold.entered().await;
        }
        foreground.push((hold, task));
    }
    admission(&runtime, (5, 4)).await;
    let (held, next) = if default_first {
        (&first_background, &revoked_background)
    } else {
        (&revoked_background, &first_background)
    };
    held.release();
    foreground[3].0.entered().await;
    assert_eq!(runtime.admission_counts_for_test().1, 4);
    assert!(
        !next.is_waiting(),
        "background took the released foreground slot"
    );
    for (hold, task) in foreground {
        hold.release();
        let result = task.await.unwrap();
        assert_eq!(result["data"]["flags"], json!(["Seen"]), "{result}");
    }

    let revoke = if default_first {
        &revoked_background
    } else {
        first_background.entered().await;
        first_background.release();
        &revoked_facts
    };
    revoke.entered().await;
    catalog.remove_account_for_test("second").unwrap();
    revoke.disconnected().await;
    let inaccessible = first
        .call_tool(
            Some(&app),
            "alice",
            "get_cache_status",
            json!({"account_id":"second"}),
        )
        .await;
    assert_eq!(inaccessible["error"]["code"], "ACCOUNT_UNAVAILABLE");
    let revoked_connections = second.connections();
    wait_scope(&first, &app, "default", "Archive", |scope| {
        scope["history"]["complete"] == true
    })
    .await;
    assert_eq!(second.connections(), revoked_connections);
    drop(app);
    runtime.finish_cache(cache).await;
    admission(&runtime, (0, 0)).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn concurrent_write_retains_dirty_until_new_flags_delete_and_move_evidence_converges() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(
        root.path(),
        true,
        64 * MIB,
        &[("default", &["INBOX", "Archive"])],
    );
    let moved = synthetic(8, "moved body");
    let h = Harness::new(
        [
            (
                "INBOX",
                Folder::new(
                    "",
                    9,
                    [(7, synthetic(7, "old generation")), (8, moved.clone())],
                ),
            ),
            ("Archive", Folder::new("\\Archive", 10, [])),
        ],
        256 * 1024,
    )
    .await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    wait_scope(&h, &app, "default", "INBOX", |scope| {
        scope["reconciliation"]["complete"] == true
            && scope["current_coverage"]["full_body"]["materialized"] == 2
    })
    .await;
    assert_eq!(
        cached_body(&h, &app, "INBOX", 9, 8).await["data"]["body"],
        "moved body"
    );

    const FLAGS: &str = "UID FETCH 7,8 (UID FLAGS)";
    let stale = h.hold("INBOX", FLAGS);
    stale.entered().await;
    let mark = h
        .call_tool(
            Some(&app),
            "alice",
            "mark_email",
            json!({"uid":7,"uid_validity":9,"flag":"seen","value":false}),
        )
        .await;
    assert_eq!(mark["data"]["status"], "updated", "{mark}");
    h.update("INBOX", |folder| {
        folder.next = Some(9);
        folder.messages.remove(&8).unwrap();
        folder.message_flags.insert(7, "");
    });
    h.update("Archive", |folder| {
        folder.messages.insert(5, moved);
        folder.next = Some(6);
    });
    let fresh = h.hold("INBOX", FLAGS);
    stale.release();
    fresh.entered().await;
    let blocked = cached_body(&h, &app, "INBOX", 9, 7).await;
    assert_eq!(
        blocked["error"]["code"], "CACHE_NEEDS_VERIFICATION",
        "{blocked}"
    );
    let snapshot = status(&h, &app, "default").await;
    let stored_inbox = snapshot["storage"]["folders"]
        .as_array()
        .and_then(|folders| folders.iter().find(|folder| folder["mailbox"] == "INBOX"))
        .unwrap_or_else(|| panic!("missing INBOX storage status: {snapshot}"));
    assert_eq!(stored_inbox["dirty"], true);
    let blocked_scope = scope_row(&snapshot, "INBOX");
    assert!(
        blocked_scope["progress"]["dirty"] == true
            || blocked_scope["error"] == "verification_pending",
        "{blocked_scope}"
    );
    fresh.release();

    wait_scope(&h, &app, "default", "INBOX", |scope| {
        scope["dirty"] == false
            && scope["current_coverage"]["existence_flags_verified"] == true
            && scope["current_coverage"]["known_messages"] == 1
    })
    .await;
    let deleted = cached_body(&h, &app, "INBOX", 9, 8).await;
    assert_eq!(deleted["error"]["code"], "CACHE_MISS", "{deleted}");
    let rows = h
        .call_tool(
            Some(&app),
            "alice",
            "list_emails",
            json!({"read_source":"cache","uid_validity":9}),
        )
        .await;
    assert_eq!(
        rows["data"]["emails"].as_array().unwrap().len(),
        1,
        "{rows}"
    );
    assert_eq!(rows["data"]["emails"][0]["uid"], 7);
    assert_eq!(rows["data"]["emails"][0]["flags"], json!([]));
    wait_scope(&h, &app, "default", "Archive", |scope| {
        scope["tail"]["high_uid"] == 5
            && scope["current_coverage"]["full_body"]["materialized"] == 1
    })
    .await;
    assert_eq!(
        cached_body(&h, &app, "Archive", 10, 5).await["data"]["body"],
        "moved body"
    );

    h.update("INBOX", |folder| {
        folder.validity = 11;
        folder.next = Some(8);
        folder.messages = [(7, synthetic(7, "new generation"))].into();
        folder.message_flags.clear();
    });
    wait_scope(&h, &app, "default", "INBOX", |scope| {
        scope["uid_validity"] == 11
            && scope["dirty"] == false
            && scope["current_coverage"]["full_body"]["materialized"] == 1
    })
    .await;
    assert_eq!(
        cached_body(&h, &app, "INBOX", 9, 7).await["error"]["code"],
        "STALE_UID"
    );
    assert_eq!(
        cached_body(&h, &app, "INBOX", 11, 7).await["data"]["body"],
        "new generation"
    );
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn quota_pause_survives_restart_and_resumes_after_admin_enlarges_the_cache() {
    let root = tempfile::tempdir().unwrap();
    let scopes = &[("default", &["INBOX"][..])];
    let path = configuration(root.path(), true, 3 * MIB, scopes);
    let raw = synthetic(7, &"x".repeat((4 * MIB) as usize));
    let size = raw.len();
    let h = Harness::new([("INBOX", Folder::new("", 9, [(7, raw)]))], 256 * 1024).await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    let blocked = wait_scope(&h, &app, "default", "INBOX", |scope| {
        scope["history"]["complete"] == true
            && scope["current_coverage"]["work"]["quota_blocked"] == 1
    })
    .await;
    let history = blocked["history"]["scan_id"].clone();
    assert_eq!(blocked["current_coverage"]["raw"]["materialized"], 0);
    assert!(
        h.commands()
            .iter()
            .all(|command| !command.text.contains("BODY.PEEK[]")),
        "the quota was known before downloading the raw payload",
    );
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);

    configuration(root.path(), false, 64 * MIB, scopes);
    let connections = h.connections();
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    let paused = status(&h, &app, "default").await;
    assert_eq!(paused["sync"]["enabled"], false, "{paused}");
    assert_eq!(scope(&paused, "INBOX")["history"]["scan_id"], history);
    assert_eq!(
        scope(&paused, "INBOX")["current_coverage"]["work"]["quota_blocked"],
        1
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.connections(), connections);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);

    configuration(root.path(), true, 64 * MIB, scopes);
    let raw_hold = h.hold("INBOX", "UID FETCH 7 (UID BODY.PEEK[]<65536.65536>)");
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    raw_hold.entered().await;
    assert_eq!(staged_bytes(root.path()), 65_536);
    drop(app);
    runtime.finish_cache(cache).await;
    raw_hold.disconnected().await;
    assert_eq!(staged_bytes(root.path()), 0);
    admission(&runtime, (0, 0)).await;
    drop(runtime);
    drop(witness);

    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    let restarted = status(&h, &app, "default").await;
    assert_eq!(restarted["storage"]["startup_unclean"], false);
    assert_eq!(scope(&restarted, "INBOX")["history"]["scan_id"], history);
    assert_eq!(
        scope(&restarted, "INBOX")["current_coverage"]["raw"]["materialized"],
        0
    );
    runtime.start_sync();
    let resumed = wait_scope(&h, &app, "default", "INBOX", |scope| {
        scope["current_coverage"]["raw"]["materialized_bytes"] == size as u64
            && scope["current_coverage"]["full_body"]["materialized"] == 1
    })
    .await;
    assert_eq!(resumed["history"]["scan_id"], history);
    assert_eq!(resumed["current_coverage"]["work"]["quota_blocked"], 0);
    assert_eq!(
        cached_body(&h, &app, "INBOX", 9, 7).await["data"]["body"],
        "x".repeat(32)
    );
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}
