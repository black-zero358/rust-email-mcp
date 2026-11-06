//! Signed background QRESYNC acceptance against a local TLS IMAP server.
use super::{
    conversation::fixture::{Folder, Harness, message},
    *,
};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

const BASE: u64 = 9_007_199_254_740_999;
const WAIT: Duration = Duration::from_secs(25);

struct Remote {
    highest: u64,
    exists: usize,
    uid_next: u32,
    enable: (Vec<u8>, &'static str),
    reject_condstore_after_enable: bool,
    reject_next_condstore: bool,
    evidence: BTreeMap<String, Vec<u8>>,
}

impl Remote {
    fn reply(&mut self, mailbox: &str, command: &str) -> Option<(Vec<u8>, &'static str)> {
        if command == "CAPABILITY" {
            return Some((
                b"* CAPABILITY IMAP4rev1 UIDPLUS ENABLE QRESYNC\r\n".to_vec(),
                "OK",
            ));
        }
        if command == "ENABLE QRESYNC" {
            self.reject_next_condstore = self.reject_condstore_after_enable;
            return Some(self.enable.clone());
        }
        assert_eq!(mailbox, "INBOX");
        if command.ends_with(" (CONDSTORE)") && std::mem::take(&mut self.reject_next_condstore) {
            return Some((Vec::new(), "BAD"));
        }
        let mut evidence = self.evidence.remove(command).unwrap_or_default();
        if command.ends_with(" (CONDSTORE)") || command.contains(" (QRESYNC (") {
            let mut response = format!("* {} EXISTS\r\n* OK [UIDVALIDITY 9] selected\r\n* OK [UIDNEXT {}] next\r\n* OK [HIGHESTMODSEQ {}] observed\r\n", self.exists, self.uid_next, self.highest).into_bytes();
            response.append(&mut evidence);
            Some((response, "OK [READ-ONLY]"))
        } else if !evidence.is_empty() || command.contains(" (CHANGEDSINCE ") {
            Some((evidence, "OK"))
        } else {
            None
        }
    }
}

fn configuration(root: &Path, account: &str) -> PathBuf {
    let path = root.join("cache.json");
    std::fs::write(&path, serde_json::to_vec(&json!({
        "version":1,"mode":"full","directory":"cache","max_bytes":67_108_864,
        "accounts":[{"account_id":account,"mailboxes":["INBOX"],"max_bytes":67_108_864}],
        "sync":{"enabled":true,"interval_seconds":1,"reconcile_interval_seconds":60,"protocol":"auto"}
    })).unwrap()).unwrap();
    path
}

async fn harness(uids: impl IntoIterator<Item = u32>) -> (Harness, Arc<Mutex<Remote>>) {
    let mut folder = Folder::new("", 9, []);
    for uid in uids {
        folder.messages.insert(
            uid,
            message(
                Some(&format!("qresync-{uid}@example.test")),
                &[],
                None,
                "synthetic qresync",
                "local proof",
            ),
        );
        folder.message_flags.insert(uid, "\\Seen");
    }
    let uid_next = folder
        .messages
        .last_key_value()
        .map_or(1, |(&uid, _)| uid + 1);
    folder.next = Some(uid_next);
    let remote = Arc::new(Mutex::new(Remote {
        highest: BASE,
        exists: folder.messages.len(),
        uid_next,
        enable: (b"* ENABLED QRESYNC\r\n".to_vec(), "OK"),
        reject_condstore_after_enable: false,
        reject_next_condstore: false,
        evidence: BTreeMap::new(),
    }));
    let h = Harness::new([("INBOX", folder)], 256 * 1024).await;
    let reply = remote.clone();
    h.override_replies(move |mailbox, command| reply.lock().unwrap().reply(mailbox, command));
    (h, remote)
}

fn exact(set: &str) -> String {
    format!("EXAMINE \"INBOX\" (QRESYNC (9 {BASE} {set}))")
}

fn vanished(set: &str) -> String {
    format!("UID FETCH {set} (UID FLAGS MODSEQ) (CHANGEDSINCE {BASE} VANISHED)")
}

fn at(row: &Value, lane: &str, value: u64) -> bool {
    row[lane]["committed_modseq"]
        .as_str()
        .and_then(|number| number.parse().ok())
        == Some(value)
}

async fn status(h: &Harness, app: &Router, account: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let status = h
            .call_tool(
                Some(app),
                "alice",
                "get_cache_status",
                json!({"account_id":account}),
            )
            .await;
        assert_eq!(status["data"]["sync"]["protocol"], "auto", "{status}");
        let row = &status["data"]["sync"]["folders"][0];
        if ready(row) {
            return row.clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "{status}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn seeded(h: &Harness, app: &Router, account: &str, count: usize) {
    status(h, app, account, |row| {
        at(row, "qresync", BASE)
            && row["progress"]["current_coverage"]["known_messages"] == count
            && (count > 4 || row["progress"]["current_coverage"]["complete_headers"] == count)
    })
    .await;
}

#[tokio::test]
async fn small_sparse_examine_applies_earlier_live_and_flags_to_the_qresync_lane() {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness([1, 3, 7, 9]).await;
    let (app, runtime, cache, _witness, _) =
        h.full_cache_router(&configuration(root.path(), "default"));
    runtime.start_sync();
    seeded(&h, &app, "default", 4).await;
    h.update("INBOX", |folder| {
        folder.messages.remove(&1);
        folder.messages.remove(&3);
        folder.message_flags.insert(7, "");
    });
    let command = exact("1,3,7,9");
    {
        let mut remote = remote.lock().unwrap();
        remote.highest = BASE + 1;
        remote.exists = 2;
        remote.evidence.insert(command.clone(), format!("* VANISHED (EARLIER) 1\r\n* 1 FETCH (UID 7 FLAGS () MODSEQ ({}))\r\n* VANISHED 3\r\n", BASE + 1).into_bytes());
    }
    let row = status(&h, &app, "default", |row| at(row, "qresync", BASE + 1)).await;
    assert!(at(&row, "condstore", BASE), "{row}");
    assert_eq!(row["runtime"]["protocol"], "qresync");
    let result = h
        .call_tool(
            Some(&app),
            "alice",
            "list_emails",
            json!({"read_source":"cache","uid_validity":9}),
        )
        .await;
    let emails = result["data"]["emails"].as_array().unwrap();
    assert_eq!(
        emails
            .iter()
            .map(|row| row["uid"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [9, 7]
    );
    assert_eq!(emails[1]["flags"], json!([]));
    let trace = h.commands();
    let selected = trace
        .iter()
        .position(|entry| entry.text == command)
        .unwrap();
    assert!(
        trace[..selected]
            .iter()
            .any(|entry| entry.connection == trace[selected].connection
                && entry.text == "ENABLE QRESYNC")
    );
    assert!(trace.iter().all(|entry| !entry.text.contains(" VANISHED)")));
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn enable_rejection_or_missing_enabled_uses_fresh_condstore_only() {
    for enable in [
        (Vec::new(), "BAD"),
        (b"* ENABLED CONDSTORE\r\n".to_vec(), "OK"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (h, remote) = harness([1, 2]).await;
        remote.lock().unwrap().enable = enable;
        let (app, runtime, cache, _witness, _) =
            h.full_cache_router(&configuration(root.path(), "default"));
        runtime.start_sync();
        seeded(&h, &app, "default", 2).await;
        remote.lock().unwrap().highest = BASE + 1;
        let row = status(&h, &app, "default", |row| at(row, "condstore", BASE + 1)).await;
        assert!(at(&row, "qresync", BASE), "{row}");
        assert_eq!(row["runtime"]["protocol"], "condstore");
        assert_eq!(row["runtime"]["protocol_fallback"], "qresync_enable_failed");
        let trace = h.commands();
        let enabled = trace
            .iter()
            .find(|entry| entry.text == "ENABLE QRESYNC")
            .unwrap();
        assert!(!trace.iter().any(
            |entry| entry.connection == enabled.connection && entry.text.starts_with("EXAMINE")
        ));
        assert!(
            trace
                .iter()
                .any(|entry| entry.connection > enabled.connection
                    && entry.text.contains(&format!("CHANGEDSINCE {BASE})")))
        );
        assert!(
            trace
                .iter()
                .all(|entry| !entry.text.contains(" (QRESYNC (")
                    && !entry.text.contains(" VANISHED)"))
        );
        drop(app);
        runtime.finish_cache(cache).await;
    }
}

#[tokio::test]
async fn rejected_qresync_and_rejected_fresh_condstore_use_only_two_connections() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path(), "default");
    let (h, remote) = harness([1, 2]).await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    status(&h, &app, "default", |row| {
        at(row, "qresync", BASE) && row["progress"]["current_coverage"]["indexed_bodies"] == 2
    })
    .await;
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);

    // Run the first round after restart with no recurring discovery or audit
    // due while checking the number of connections used by the fallback.
    let mut configured: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    configured["sync"]["interval_seconds"] = json!(300);
    configured["sync"]["reconcile_interval_seconds"] = json!(300);
    std::fs::write(&path, serde_json::to_vec(&configured).unwrap()).unwrap();
    {
        let mut remote = remote.lock().unwrap();
        remote.highest = BASE + 1;
        remote.enable = (Vec::new(), "BAD");
        remote.reject_condstore_after_enable = true;
    }
    let previous = h.commands().len();
    let (app, runtime, cache, _witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    let row = status(&h, &app, "default", |row| {
        row["runtime"]["protocol_fallback"] == "extension_failed"
    })
    .await;
    assert!(
        at(&row, "qresync", BASE) && at(&row, "condstore", BASE),
        "{row}"
    );
    drop(app);
    runtime.finish_cache(cache).await;
    let trace = h.commands();
    let selected = previous
        + trace[previous..]
            .iter()
            .position(|entry| entry.text == "ENABLE QRESYNC")
            .unwrap();
    let start = trace[selected].connection;
    let fallback = &trace[selected + 1..];
    assert_eq!(fallback.len(), 2, "{fallback:?}");
    assert_eq!(fallback[0].text, "CAPABILITY");
    assert_eq!(fallback[1].text, "EXAMINE \"INBOX\" (CONDSTORE)");
    assert_eq!(fallback[0].connection, start + 1);
    assert_eq!(fallback[1].connection, start + 1);
    assert_eq!(h.connections(), start + 1);
}

#[tokio::test]
async fn oversized_qresync_receipt_discards_deletion_and_uses_one_fresh_connection() {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness([1, 3]).await;
    let (app, runtime, cache, _witness, _) =
        h.full_cache_router(&configuration(root.path(), "default"));
    runtime.start_sync();
    seeded(&h, &app, "default", 2).await;
    let command = exact("1,3");
    let hold = h.hold("INBOX", &command);
    remote.lock().unwrap().highest = BASE + 1;
    hold.entered().await;
    let mut evidence = b"* VANISHED (EARLIER) 1\r\n".to_vec();
    evidence.extend(b"* OK [ALERT] bounded synthetic notice\r\n".repeat(4_000));
    remote
        .lock()
        .unwrap()
        .evidence
        .insert(command.clone(), evidence);
    hold.release();
    let row = status(&h, &app, "default", |row| {
        at(row, "condstore", BASE + 1) && row["runtime"]["protocol_fallback"] == "qresync_failed"
    })
    .await;
    assert!(at(&row, "qresync", BASE), "{row}");
    let listed = h
        .call_tool(
            Some(&app),
            "alice",
            "list_emails",
            json!({"read_source":"cache","uid_validity":9}),
        )
        .await;
    assert_eq!(
        listed["data"]["emails"].as_array().unwrap().len(),
        2,
        "{listed}"
    );
    let trace = h.commands();
    let failed = trace
        .iter()
        .rev()
        .find(|entry| entry.text == command)
        .unwrap();
    assert!(
        trace
            .iter()
            .any(|entry| entry.connection == failed.connection + 1
                && entry.text.ends_with(&format!("(CHANGEDSINCE {BASE})")))
    );
    assert!(
        trace
            .iter()
            .all(|entry| entry.connection != failed.connection
                || !entry.text.starts_with("UID FETCH"))
    );
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn empty_scope_advances_without_an_empty_uid_set_or_fetch() {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness([]).await;
    let (app, runtime, cache, _witness, _) =
        h.full_cache_router(&configuration(root.path(), "default"));
    runtime.start_sync();
    seeded(&h, &app, "default", 0).await;
    remote.lock().unwrap().highest = BASE + 1;
    let row = status(&h, &app, "default", |row| at(row, "qresync", BASE + 1)).await;
    assert_eq!(row["qresync"]["high_uid"], 0);
    let trace = h.commands();
    let enabled = trace
        .iter()
        .find(|entry| entry.text == "ENABLE QRESYNC")
        .unwrap();
    assert!(
        trace
            .iter()
            .any(|entry| entry.connection == enabled.connection
                && entry.text == "EXAMINE \"INBOX\" (CONDSTORE)")
    );
    assert!(
        trace.iter().all(
            |entry| !entry.text.contains(" (QRESYNC (") && !entry.text.starts_with("UID FETCH")
        )
    );
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn large_delta_persists_first_page_and_page_outside_removal_across_restart() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path(), "default");
    let (h, remote) = harness(1..=202).await;
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    seeded(&h, &app, "default", 202).await;
    let first = vanished("1:200");
    let second = vanished("201");
    let held = h.hold("INBOX", &second);
    h.update("INBOX", |folder| {
        folder.messages.remove(&202);
    });
    {
        let mut remote = remote.lock().unwrap();
        remote.highest = BASE + 1;
        remote.exists = 201;
        remote
            .evidence
            .insert(first.clone(), b"* VANISHED 202\r\n".to_vec());
    }
    held.entered().await;
    let row = status(&h, &app, "default", |row| {
        row["qresync"]["known_after"] == 200
    })
    .await;
    assert!(at(&row, "qresync", BASE), "{row}");
    assert_eq!(row["qresync"]["target_modseq"], (BASE + 1).to_string());
    assert_eq!(row["progress"]["current_coverage"]["known_messages"], 201);
    drop(app);
    runtime.finish_cache(cache).await;
    held.disconnected().await;
    drop(runtime);
    drop(witness);

    let (app, runtime, cache, _witness, _) = h.full_cache_router(&path);
    let restored = status(&h, &app, "default", |row| {
        row["qresync"]["known_after"] == 200
    })
    .await;
    assert!(at(&restored, "qresync", BASE));
    runtime.start_sync();
    let complete = status(&h, &app, "default", |row| at(row, "qresync", BASE + 1)).await;
    assert!(at(&complete, "condstore", BASE));
    assert!(complete["qresync"]["known_after"].is_null());
    let trace = h.commands();
    assert_eq!(trace.iter().filter(|entry| entry.text == first).count(), 1);
    assert_eq!(trace.iter().filter(|entry| entry.text == second).count(), 2);
    assert!(
        trace
            .iter()
            .all(|entry| !entry.text.contains(" (QRESYNC ("))
    );
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn oversized_live_range_blocks_offline_until_a_new_full_rev1_audit() {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness(1..=201).await;
    h.update("INBOX", |folder| folder.next = Some(203));
    remote.lock().unwrap().uid_next = 203;
    let (app, runtime, cache, _witness, _) =
        h.full_cache_router(&configuration(root.path(), "default"));
    runtime.start_sync();
    seeded(&h, &app, "default", 201).await;
    let first = vanished("1:200");
    let delta = h.hold("INBOX", &first);
    remote.lock().unwrap().highest = BASE + 1;
    delta.entered().await;
    // UID 202 has never been cached. A foreground network command sees its
    // old positive evidence, but its tagged reply is held until after audit.
    h.update("INBOX", |folder| {
        folder
            .messages
            .insert(202, message(None, &[], None, "late positive", "old"));
    });
    const SUMMARY: &str =
        "UID FETCH 202 (UID FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER]<0.8192>)";
    let late = h.hold("INBOX", SUMMARY);
    let mut foreground =
        Box::pin(h.call_tool(Some(&app), "alice", "list_emails", json!({"limit":1})));
    tokio::select! {
        () = late.entered() => {},
        result = &mut foreground => panic!("foreground did not wait for its captured reply: {result}"),
    }
    let last = h.hold("INBOX", "UID FETCH 201 (UID FLAGS)");
    h.update("INBOX", |folder| {
        folder.messages.clear();
    });
    {
        let mut remote = remote.lock().unwrap();
        remote.exists = 0;
        remote
            .evidence
            .insert(first, b"* VANISHED 1:202\r\n".to_vec());
    }
    delta.release();
    last.entered().await;
    let blocked = h
        .call_tool(
            Some(&app),
            "alice",
            "list_emails",
            json!({"read_source":"cache","uid_validity":9}),
        )
        .await;
    assert_eq!(
        blocked["error"]["code"], "CACHE_NEEDS_VERIFICATION",
        "{blocked}"
    );
    let row = status(&h, &app, "default", |row| {
        row["runtime"]["protocol_fallback"] == "qresync_reconciliation_required"
    })
    .await;
    assert!(row["qresync"].is_null(), "{row}");
    assert_eq!(row["error"], "verification_pending");
    last.release();
    let recovered = status(&h, &app, "default", |row| {
        row["error"].is_null()
            && row["progress"]["current_coverage"]["existence_flags_verified"] == true
            && row["progress"]["current_coverage"]["known_messages"] == 0
    })
    .await;
    assert!(!at(&recovered, "qresync", BASE + 1), "{recovered}");
    let missing = h
        .call_tool(
            Some(&app),
            "alice",
            "get_email",
            json!({"read_source":"cache","uid_validity":9,"uid":1}),
        )
        .await;
    assert_eq!(missing["error"]["code"], "CACHE_MISS", "{missing}");
    let header = "Subject: late positive\r\n\r\n";
    remote.lock().unwrap().evidence.insert(SUMMARY.into(),
        format!("* 202 FETCH (UID 202 FLAGS (\\Seen) RFC822.SIZE 30 BODY[HEADER]<0> {{{}}}\r\n{header})\r\n", header.len()).into_bytes());
    late.release();
    let late_result = foreground.await;
    assert_eq!(
        late_result["data"]["emails"][0]["uid"], 202,
        "{late_result}"
    );
    status(&h, &app, "default", |row| {
        row["progress"]["current_coverage"]["known_messages"] == 0
    })
    .await;
    let absent = h
        .call_tool(
            Some(&app),
            "alice",
            "get_email",
            json!({"read_source":"cache","uid_validity":9,"uid":202}),
        )
        .await;
    assert_eq!(absent["error"]["code"], "CACHE_MISS", "{absent}");

    h.update("INBOX", |folder| {
        folder.next = Some(204);
        folder
            .messages
            .insert(203, message(None, &[], None, "fresh positive", "new"));
    });
    let fresh = h
        .call_tool(Some(&app), "alice", "list_emails", json!({"limit":1}))
        .await;
    assert_eq!(fresh["data"]["emails"][0]["uid"], 203, "{fresh}");
    status(&h, &app, "default", |row| {
        row["progress"]["current_coverage"]["known_messages"] == 1
            && row["progress"]["current_coverage"]["complete_headers"] == 1
    })
    .await;
    let local = h
        .wait_for_cache_listing(&app, json!({"read_source":"cache","uid_validity":9}))
        .await;
    assert_eq!(local["data"]["emails"][0]["uid"], 203, "{local}");
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn fresh_rev1_audit_waits_for_an_old_fifo_write_already_executing() {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness(1..=201).await;
    h.update("INBOX", |folder| folder.next = Some(203));
    remote.lock().unwrap().uid_next = 203;
    let (app, runtime, cache, _witness, catalog) =
        h.full_cache_router(&configuration(root.path(), "default"));
    runtime.start_sync();
    seeded(&h, &app, "default", 201).await;
    let first = vanished("1:200");
    let delta = h.hold("INBOX", &first);
    remote.lock().unwrap().highest = BASE + 1;
    delta.entered().await;
    let lease = catalog.read_account("alice", None).unwrap();
    let (entered, release) = cache.hold_header_write_for_test(&lease.key, 202);
    tokio::time::timeout(WAIT, entered).await.unwrap().unwrap();
    let before = h.connections();
    h.update("INBOX", |folder| folder.messages.clear());
    {
        let mut remote = remote.lock().unwrap();
        remote.exists = 0;
        remote
            .evidence
            .insert(first, b"* VANISHED 1:202\r\n".to_vec());
    }
    delta.release();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let read = h
            .call_tool(
                Some(&app),
                "alice",
                "list_emails",
                json!({"read_source":"cache","uid_validity":9}),
            )
            .await;
        if read["error"]["code"] == "CACHE_NEEDS_VERIFICATION" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{read}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(
        h.connections(),
        before,
        "fresh selection overtook the FIFO barrier"
    );
    release.send(()).unwrap();
    status(&h, &app, "default", |row| {
        row["error"].is_null() && row["progress"]["current_coverage"]["known_messages"] == 0
    })
    .await;
    assert!(
        h.commands()
            .iter()
            .any(|command| command.connection > before
                && command.text == "UID FETCH 201,202 (UID FLAGS)")
    );
    let missing = h
        .call_tool(
            Some(&app),
            "alice",
            "get_email",
            json!({"read_source":"cache","uid_validity":9,"uid":202}),
        )
        .await;
    assert_eq!(missing["error"]["code"], "CACHE_MISS", "{missing}");
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn final_explicit_watermark_below_frozen_target_keeps_qresync_anchor() {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness(1..=201).await;
    let (app, runtime, cache, _witness, _) =
        h.full_cache_router(&configuration(root.path(), "default"));
    runtime.start_sync();
    seeded(&h, &app, "default", 201).await;
    {
        let mut remote = remote.lock().unwrap();
        remote.highest = BASE + 3;
        remote.evidence.insert(
            vanished("1:200"),
            format!("* OK [HIGHESTMODSEQ {}] completed\r\n", BASE + 2).into_bytes(),
        );
    }
    let row = status(&h, &app, "default", |row| {
        row["runtime"]["protocol_fallback"] == "qresync_failed"
    })
    .await;
    assert!(at(&row, "qresync", BASE), "{row}");
    assert_eq!(row["qresync"]["known_after"], 0);
    assert_eq!(row["qresync"]["target_modseq"], (BASE + 3).to_string());
    let trace = h.commands();
    let selected = trace
        .iter()
        .find(|entry| entry.text == vanished("1:200"))
        .unwrap();
    assert!(
        trace
            .iter()
            .any(|entry| entry.connection > selected.connection
                && entry.text.ends_with(&format!("(CHANGEDSINCE {BASE})")))
    );
    assert!(trace.iter().all(|entry| entry.text != vanished("201")));
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn revocation_cancels_enabled_exact_selection_without_changing_the_anchor() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path(), "second");
    let (secondary, remote) = harness([1, 3]).await;
    let front =
        Harness::with_second_account([("INBOX", Folder::new("", 9, []))], 256 * 1024, &secondary)
            .await;
    let (app, runtime, cache, witness, catalog) = front.full_cache_router(&path);
    runtime.start_sync();
    seeded(&front, &app, "second", 2).await;
    let hold = secondary.hold("INBOX", &exact("1,3"));
    remote.lock().unwrap().highest = BASE + 1;
    hold.entered().await;
    catalog.remove_account_for_test("second").unwrap();
    hold.disconnected().await;
    let denied = front
        .call_tool(
            Some(&app),
            "alice",
            "get_cache_status",
            json!({"account_id":"second"}),
        )
        .await;
    assert_eq!(denied["error"]["code"], "ACCOUNT_UNAVAILABLE");
    let connections = secondary.connections();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(secondary.connections(), connections);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(catalog);
    drop(witness);

    let (app, runtime, cache, _witness, _) = front.full_cache_router(&path);
    let row = status(&front, &app, "second", |row| at(row, "qresync", BASE)).await;
    assert!(row["qresync"]["known_after"].is_null());
    drop(app);
    runtime.finish_cache(cache).await;
}
