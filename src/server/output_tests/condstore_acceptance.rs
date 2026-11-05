//! Signed production background CONDSTORE acceptance with a synthetic TLS IMAP server.
use super::{
    conversation::fixture::{Folder, Harness, message},
    *,
};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

const FLOOR: u64 = 9_007_199_254_740_999;
const WAIT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy)]
enum Mode {
    Unsupported,
    Rejected,
    Nomodseq,
    Highest(u64),
}

#[derive(Clone, Copy)]
enum WireModseq {
    Highest(u64),
    Unavailable,
}

impl WireModseq {
    fn reply(self, output: &mut String) {
        use std::fmt::Write;
        match self {
            Self::Highest(value) => {
                write!(output, "* OK [HIGHESTMODSEQ {value}] observed floor\r\n").unwrap()
            }
            Self::Unavailable => output.push_str("* OK [NOMODSEQ] unavailable\r\n"),
        }
    }
}

struct Remote {
    mode: Mode,
    count: u32,
    flags: BTreeMap<u32, (u64, &'static str)>,
    command_highest: Option<u64>,
    command_early: Option<WireModseq>,
    audit_notice: Option<(String, WireModseq)>,
}

impl Remote {
    fn reply(&mut self, mailbox: &str, command: &str) -> Option<(Vec<u8>, &'static str)> {
        if command == "CAPABILITY" {
            // QRESYNC implies CONDSTORE. This release must still avoid ENABLE.
            return Some((
                if matches!(self.mode, Mode::Unsupported) {
                    b"* CAPABILITY IMAP4rev1 UIDPLUS\r\n".to_vec()
                } else {
                    b"* CAPABILITY IMAP4rev1 UIDPLUS ENABLE QRESYNC\r\n".to_vec()
                },
                "OK",
            ));
        }
        assert_eq!(mailbox, "INBOX");
        if command.ends_with(" (CONDSTORE)") {
            let code = match self.mode {
                Mode::Unsupported => panic!("extension attempted without capability"),
                Mode::Rejected => return Some((Vec::new(), "BAD")),
                Mode::Nomodseq => "NOMODSEQ".into(),
                Mode::Highest(value) => format!("HIGHESTMODSEQ {value}"),
            };
            return Some((format!("* {} EXISTS\r\n* OK [UIDVALIDITY 9] current\r\n* OK [UIDNEXT {}] next\r\n* OK [{code}] checkpoint\r\n", self.count, self.count + 1).into_bytes(), "OK [READ-ONLY]"));
        }
        let (set, modifier) = command.strip_prefix("UID FETCH ")?.split_once(' ')?;
        if modifier == "(UID FLAGS)"
            && self
                .audit_notice
                .as_ref()
                .is_some_and(|(expected, _)| expected == set)
        {
            use std::fmt::Write;
            let (_, notice) = self.audit_notice.take().unwrap();
            let mut reply = String::new();
            for uid in set.split(',') {
                write!(reply, "* {uid} FETCH (UID {uid} FLAGS (\\Seen))\r\n").unwrap();
            }
            notice.reply(&mut reply);
            return Some((reply.into_bytes(), "OK"));
        }
        let base: u64 = modifier
            .strip_prefix("(UID FLAGS MODSEQ) (CHANGEDSINCE ")?
            .strip_suffix(')')?
            .parse()
            .unwrap();
        assert!(
            matches!(self.mode, Mode::Highest(_)),
            "delta sent without HIGHESTMODSEQ"
        );
        let mut reply = String::new();
        if let Some(early) = self.command_early.take() {
            early.reply(&mut reply);
        }
        for uid in set.split(',').map(|uid| uid.parse::<u32>().unwrap()) {
            if let Some(&(modseq, flags)) = self.flags.get(&uid)
                && modseq > base
            {
                use std::fmt::Write;
                write!(
                    reply,
                    "* {uid} FETCH (UID {uid} FLAGS ({flags}) MODSEQ ({modseq}))\r\n"
                )
                .unwrap();
            }
        }
        if let Some(highest) = self.command_highest {
            use std::fmt::Write;
            write!(reply, "* OK [HIGHESTMODSEQ {highest}] completed floor\r\n").unwrap();
        }
        Some((reply.into_bytes(), "OK"))
    }
}

fn configuration(root: &Path) -> PathBuf {
    configuration_for(root, "default")
}

fn configuration_for(root: &Path, account: &str) -> PathBuf {
    let path = root.join("cache.json");
    std::fs::write(&path, serde_json::to_vec(&json!({
        "version":1,"mode":"full","directory":"cache","max_bytes":67_108_864,
        "accounts":[{"account_id":account,"mailboxes":["INBOX"],"max_bytes":67_108_864}],
        "sync":{"enabled":true,"interval_seconds":1,"reconcile_interval_seconds":60,"protocol":"auto"}
    })).unwrap()).unwrap();
    path
}

fn raw(uid: u32) -> Vec<u8> {
    message(
        Some(&format!("condstore-{uid}@example.test")),
        &[],
        None,
        "synthetic modseq",
        "local proof",
    )
}

async fn harness(count: u32, mode: Mode) -> (Harness, Arc<Mutex<Remote>>) {
    let mut folder = Folder::new("", 9, []);
    folder.generate(1..=count, raw);
    folder.message_flags.insert(1, "\\Seen");
    folder.message_flags.insert(2, "\\Seen");
    let h = Harness::new([("INBOX", folder)], 256 * 1024).await;
    let remote = Arc::new(Mutex::new(Remote {
        mode,
        count,
        flags: BTreeMap::new(),
        command_highest: None,
        command_early: None,
        audit_notice: None,
    }));
    let reply = remote.clone();
    h.override_replies(move |mailbox, command| reply.lock().unwrap().reply(mailbox, command));
    (h, remote)
}

async fn wait_status(h: &Harness, app: &Router, ready: impl Fn(&Value) -> bool) -> Value {
    wait_account_status(h, app, "default", ready).await
}

async fn wait_account_status(
    h: &Harness,
    app: &Router,
    account: &str,
    ready: impl Fn(&Value) -> bool,
) -> Value {
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
        let folder = &status["data"]["sync"]["folders"][0];
        if ready(folder) {
            return folder.clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "{status}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn account_revocation_closes_a_delta_and_keeps_its_committed_watermark() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration_for(root.path(), "second");
    let (secondary, remote) = harness(2, Mode::Highest(FLOOR)).await;
    let front =
        Harness::with_second_account([("INBOX", Folder::new("", 9, []))], 256 * 1024, &secondary)
            .await;
    let (app, runtime, cache, witness, catalog) = front.full_cache_router(&path);
    runtime.start_sync();
    wait_account_status(&front, &app, "second", |row| watermarked(row, FLOOR)).await;
    let fetch = format!("UID FETCH 1,2 (UID FLAGS MODSEQ) (CHANGEDSINCE {FLOOR})");
    let hold = secondary.hold("INBOX", &fetch);
    remote.lock().unwrap().mode = Mode::Highest(FLOOR + 1);
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
    assert_eq!(runtime.admission_counts_for_test(), (0, 0));
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(catalog);
    drop(witness);

    let (app, runtime, cache, _witness, _) = front.full_cache_router(&path);
    let row = wait_account_status(&front, &app, "second", |row| {
        row["condstore"]["committed_modseq"].is_string()
    })
    .await;
    assert_eq!(row["condstore"]["committed_modseq"], FLOOR.to_string());
    assert_eq!(row["condstore"]["known_after"], 0);
    drop(app);
    runtime.finish_cache(cache).await;
}

fn fallback(row: &Value, reason: &str) -> bool {
    row["runtime"]["protocol"] == "rev1" && row["runtime"]["protocol_fallback"] == reason
}

fn watermarked(row: &Value, expected: u64) -> bool {
    row["runtime"]["protocol"] == "condstore"
        && row["condstore"]["committed_modseq"]
            .as_str()
            .and_then(|modseq| modseq.parse().ok())
            == Some(expected)
}

#[tokio::test]
async fn capability_absence_and_tagged_rejection_keep_real_rev1_reconciliation() {
    for (mode, reason) in [
        (Mode::Unsupported, "capability_unavailable"),
        (Mode::Rejected, "extension_failed"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (h, _) = harness(2, mode).await;
        let (app, runtime, cache, _witness, _) = h.full_cache_router(&configuration(root.path()));
        runtime.start_sync();
        let row = wait_status(&h, &app, |row| {
            fallback(row, reason)
                && row["progress"]["current_coverage"]["existence_flags_verified"] == true
        })
        .await;
        assert!(row["condstore"].is_null());
        let commands = h.commands();
        let extended = commands
            .iter()
            .find(|command| command.text.ends_with(" (CONDSTORE)"));
        if let Some(extended) = extended {
            assert!(matches!(mode, Mode::Rejected));
            assert!(
                commands
                    .iter()
                    .any(|command| command.connection > extended.connection
                        && command.text == "EXAMINE \"INBOX\"")
            );
            assert!(
                commands
                    .iter()
                    .all(|command| command.connection != extended.connection
                        || !command.text.starts_with("UID FETCH"))
            );
        } else {
            assert!(matches!(mode, Mode::Unsupported));
        }
        assert!(
            commands
                .iter()
                .all(|command| !command.text.starts_with("ENABLE ")
                    && !command.text.starts_with("SELECT "))
        );
        drop(app);
        runtime.finish_cache(cache).await;
    }
}

#[tokio::test]
async fn changed_flags_apply_without_removing_omitted_uids_and_nomodseq_revokes_the_anchor() {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness(2, Mode::Highest(FLOOR)).await;
    let (app, runtime, cache, _witness, _) = h.full_cache_router(&configuration(root.path()));
    runtime.start_sync();
    wait_status(&h, &app, |row| {
        watermarked(row, FLOOR) && row["progress"]["current_coverage"]["complete_headers"] == 2
    })
    .await;
    {
        let mut remote = remote.lock().unwrap();
        remote.mode = Mode::Highest(FLOOR + 1);
        remote.flags.insert(1, (FLOOR + 1, ""));
    }
    wait_status(&h, &app, |row| watermarked(row, FLOOR + 1)).await;
    let response = h
        .call_tool(
            Some(&app),
            "alice",
            "list_emails",
            json!({"read_source":"cache","uid_validity":9}),
        )
        .await;
    let rows = response["data"]["emails"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().find(|row| row["uid"] == 1).unwrap()["flags"],
        json!([])
    );
    assert_eq!(
        rows.iter().find(|row| row["uid"] == 2).unwrap()["flags"],
        json!(["Seen"])
    );
    remote.lock().unwrap().mode = Mode::Unsupported;
    let withdrawn = wait_status(&h, &app, |row| fallback(row, "capability_unavailable")).await;
    assert_eq!(
        withdrawn["condstore"]["committed_modseq"],
        (FLOOR + 1).to_string()
    );
    remote.lock().unwrap().mode = Mode::Nomodseq;
    wait_status(&h, &app, |row| {
        fallback(row, "nomodseq") && row["condstore"].is_null()
    })
    .await;
    let commands = h.commands();
    assert!(
        commands
            .iter()
            .any(|command| command.text.contains(&format!("CHANGEDSINCE {FLOOR}")))
    );
    assert!(
        commands
            .iter()
            .all(|command| !command.text.starts_with("ENABLE "))
    );
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn last_page_promotes_the_frozen_watermark_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path());
    let (h, remote) = harness(201, Mode::Highest(FLOOR)).await;
    let second = format!("UID FETCH 201 (UID FLAGS MODSEQ) (CHANGEDSINCE {FLOOR})");
    let hold = h.hold("INBOX", &second);
    let (app, runtime, cache, witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    wait_status(&h, &app, |row| watermarked(row, FLOOR)).await;
    remote.lock().unwrap().mode = Mode::Highest(FLOOR + 1);
    hold.entered().await;
    let row = wait_status(&h, &app, |row| row["condstore"]["known_after"] == 200).await;
    assert_eq!(row["condstore"]["committed_modseq"], FLOOR.to_string());
    assert_eq!(row["condstore"]["target_modseq"], (FLOOR + 1).to_string());
    drop(app);
    runtime.finish_cache(cache).await;
    hold.disconnected().await;
    drop(runtime);
    drop(witness);

    let count = h.commands().len();
    let (app, runtime, cache, _witness, _) = h.full_cache_router(&path);
    runtime.start_sync();
    let row = wait_status(&h, &app, |row| watermarked(row, FLOOR + 1)).await;
    assert!(row["condstore"]["target_modseq"].is_null());
    let commands = h.commands();
    let pages = commands[count..]
        .iter()
        .filter(|command| command.text.contains("CHANGEDSINCE"))
        .map(|command| command.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(pages, [second.as_str()]);
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn a_lower_later_selection_rejects_the_pending_seed_even_after_the_server_recovers() {
    reject_pending_seed(SeedFailure::SelectionRollback, "modseq_rollback").await;
}

#[tokio::test]
async fn baseline_command_replies_can_invalidate_the_pending_seed_before_the_final_page() {
    reject_pending_seed(
        SeedFailure::Audit(WireModseq::Highest(FLOOR - 2)),
        "modseq_rollback",
    )
    .await;
    reject_pending_seed(SeedFailure::Audit(WireModseq::Unavailable), "nomodseq").await;
}

#[tokio::test]
async fn a_plain_rev1_page_discards_a_pending_seed_without_reusing_it() {
    reject_pending_seed(SeedFailure::CapabilityLost, "capability_unavailable").await;
}

enum SeedFailure {
    SelectionRollback,
    Audit(WireModseq),
    CapabilityLost,
}

async fn reject_pending_seed(failure: SeedFailure, reason: &str) {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness(201, Mode::Highest(FLOOR)).await;
    let first = (1..=200)
        .map(|uid| uid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let first_page = h.hold("INBOX", &format!("UID FETCH {first} (UID FLAGS)"));
    let last_page = h.hold("INBOX", "UID FETCH 201 (UID FLAGS)");
    let (app, runtime, cache, _witness, _) = h.full_cache_router(&configuration(root.path()));
    runtime.start_sync();

    first_page.entered().await;
    {
        let mut remote = remote.lock().unwrap();
        match failure {
            SeedFailure::SelectionRollback => remote.mode = Mode::Highest(FLOOR - 2),
            SeedFailure::Audit(notice) => remote.audit_notice = Some(("201".into(), notice)),
            SeedFailure::CapabilityLost => remote.mode = Mode::Unsupported,
        }
    }
    first_page.release();
    last_page.entered().await;
    // A flag change after the first page now has a MODSEQ below the rejected
    // seed. The server can rebound before the next incremental round.
    h.update("INBOX", |folder| {
        folder.message_flags.insert(1, "\\Flagged");
    });
    {
        let mut remote = remote.lock().unwrap();
        remote.mode = Mode::Highest(FLOOR + 1);
        remote.flags.insert(1, (FLOOR - 1, "\\Flagged"));
    }
    last_page.release();

    let completed = wait_status(&h, &app, |row| {
        fallback(row, reason) && row["progress"]["reconciliation"]["complete"] == true
    })
    .await;
    let completed_id = completed["progress"]["reconciliation"]["scan_id"].clone();
    assert!(completed["condstore"].is_null(), "{completed}");
    let recovered = wait_status(&h, &app, |row| watermarked(row, FLOOR + 1)).await;
    assert_ne!(
        recovered["progress"]["reconciliation"]["scan_id"],
        completed_id
    );
    let stale_search = format!("CHANGEDSINCE {FLOOR})");
    assert!(
        h.commands()
            .iter()
            .all(|command| !command.text.contains(&stale_search))
    );
    let deadline = tokio::time::Instant::now() + WAIT;
    let listing = loop {
        let listing = h
            .call_tool(
                Some(&app),
                "alice",
                "list_emails",
                json!({"read_source":"cache","uid_validity":9,"before_uid":2}),
            )
            .await;
        // Background content work can change the cached scope revision
        // between the list snapshot and its final verification.
        if listing["error"]["code"] != "CACHE_NEEDS_VERIFICATION" {
            break listing;
        }
        assert!(tokio::time::Instant::now() < deadline, "{listing}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    assert_eq!(
        listing["data"]["emails"][0]["flags"],
        json!(["Flagged"]),
        "{listing}"
    );
    drop(app);
    runtime.finish_cache(cache).await;
}

#[tokio::test]
async fn a_later_highest_cannot_mask_a_delta_rollback_or_nomodseq() {
    for (notice, reason) in [
        (WireModseq::Highest(FLOOR - 1), "modseq_rollback"),
        (WireModseq::Unavailable, "nomodseq"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (h, remote) = harness(2, Mode::Highest(FLOOR)).await;
        let (app, runtime, cache, _witness, _) = h.full_cache_router(&configuration(root.path()));
        runtime.start_sync();
        wait_status(&h, &app, |row| watermarked(row, FLOOR)).await;
        {
            let mut remote = remote.lock().unwrap();
            remote.mode = Mode::Highest(FLOOR + 2);
            remote.command_early = Some(notice);
            remote.command_highest = Some(FLOOR + 2);
        }
        wait_status(&h, &app, |row| {
            fallback(row, reason) && row["condstore"].is_null()
        })
        .await;
        let commands = h.commands();
        let delta = commands
            .iter()
            .find(|command| command.text.contains("CHANGEDSINCE"))
            .unwrap();
        assert!(
            commands
                .iter()
                .any(|command| command.connection > delta.connection
                    && command.text == "UID FETCH 1,2 (UID FLAGS)")
        );
        drop(app);
        runtime.finish_cache(cache).await;
    }
}

#[tokio::test]
async fn command_completion_floor_and_selected_rollback_never_promote_old_evidence() {
    let root = tempfile::tempdir().unwrap();
    let (h, remote) = harness(2, Mode::Highest(FLOOR)).await;
    let (app, runtime, cache, _witness, _) = h.full_cache_router(&configuration(root.path()));
    runtime.start_sync();
    wait_status(&h, &app, |row| watermarked(row, FLOOR)).await;
    {
        let mut remote = remote.lock().unwrap();
        remote.mode = Mode::Highest(FLOOR + 2);
        remote.command_highest = Some(FLOOR + 1);
        remote.flags.insert(1, (FLOOR + 2, "\\Flagged"));
    }
    let row = wait_status(&h, &app, |row| fallback(row, "extension_failed")).await;
    assert_eq!(row["condstore"]["committed_modseq"], FLOOR.to_string());
    {
        let mut remote = remote.lock().unwrap();
        remote.mode = Mode::Highest(FLOOR - 1);
        remote.command_highest = None;
    }
    wait_status(&h, &app, |row| {
        fallback(row, "modseq_rollback") && row["condstore"].is_null()
    })
    .await;
    drop(app);
    runtime.finish_cache(cache).await;
}
