use super::*;
use std::path::{Path, PathBuf};

const CACHE_PARENT: &str = "父消息@例子.test";

fn configuration(root: &Path) -> PathBuf {
    let path = root.join("cache.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "version":1,"mode":"full","directory":"cache","max_bytes":64 * 1024 * 1024,
            "accounts":[{"account_id":"default","mailboxes":["INBOX","Sent Items"],"max_bytes":32 * 1024 * 1024}]
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

async fn cached(fixture: &ThreadHarness, app: &axum::Router, arguments: Value) -> Value {
    fixture
        .call_tool(Some(app), "alice", "read_conversation", arguments)
        .await
}

async fn warm(
    fixture: &ThreadHarness,
    app: &axum::Router,
    mailbox: &str,
    uid_validity: u32,
    bodies: &[(u32, &str)],
) {
    for (uid, body) in bodies {
        let live = fixture
            .call_tool(
                Some(app),
                "alice",
                "get_email",
                json!({"mailbox":mailbox,"uid_validity":uid_validity,"uid":uid}),
            )
            .await;
        assert_eq!(data(&live)["body"], *body);
    }
    let listed = fixture
        .call_tool(
            Some(app),
            "alice",
            "list_emails",
            json!({"mailbox":mailbox}),
        )
        .await;
    assert_eq!(
        data(&listed)["emails"].as_array().unwrap().len(),
        bodies.len()
    );
    let barrier = fixture
        .call_tool(Some(app), "alice", "get_cache_status", json!({}))
        .await;
    assert_eq!(data(&barrier)["status"], "ready", "{barrier}");
}

fn cross_start(source: &str) -> Value {
    json!({
        "uid":7,"uid_validity":9,"mailboxes":["INBOX","Sent Items"],
        "candidate_limit":1,"limit":1,"read_source":source
    })
}

#[tokio::test]
async fn cache_conversation_restarts_and_replays_without_connecting_to_imap() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path());
    let fixture = ThreadHarness::new(
        [
            (
                "INBOX",
                Folder::new(
                    "",
                    9,
                    [
                        (
                            7,
                            message(
                                Some(SEED),
                                &[CACHE_PARENT],
                                None,
                                "shared subject",
                                "seed body",
                            ),
                        ),
                        (
                            8,
                            message(Some(REPLY), &[SEED], None, "reply", "reply body"),
                        ),
                        (
                            9,
                            message(
                                Some("unrelated@example.test"),
                                &[],
                                None,
                                "shared subject",
                                "unrelated body",
                            ),
                        ),
                    ],
                ),
            ),
            (
                "Sent Items",
                Folder::new(
                    "\\Sent",
                    10,
                    [(
                        7,
                        message(Some(CACHE_PARENT), &[], None, "parent", "parent body"),
                    )],
                ),
            ),
        ],
        64 * 1024,
    )
    .await;
    let (app, runtime, cache, witness, _) = fixture.full_cache_router(&path);
    warm(
        &fixture,
        &app,
        "INBOX",
        9,
        &[(7, "seed body"), (8, "reply body"), (9, "unrelated body")],
    )
    .await;
    warm(&fixture, &app, "Sent Items", 10, &[(7, "parent body")]).await;
    let legacy = cached(
        &fixture,
        &app,
        json!({"uid":7,"uid_validity":9,"before_uid":9,"read_source":"cache"}),
    )
    .await;
    assert_eq!(data(&legacy)["read_source"], "cache");
    assert_eq!(data(&legacy)["cache"]["remote_generation_unverified"], true);
    assert_eq!(data(&legacy)["scope"]["end_uid"], 8);
    assert_eq!(
        data(&legacy)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["uid"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [7, 8],
        "{legacy}"
    );
    assert_eq!(data(&legacy)["messages"][0]["body"], "seed body");
    let live = cached(&fixture, &app, cross_start("live")).await;
    let live_token = data(&live)["cursor"].as_str().unwrap();
    let before = (fixture.connections(), fixture.commands().len());
    let start = cached(&fixture, &app, cross_start("cache")).await;
    let start = data(&start);
    assert_eq!(start["read_source"], "cache");
    assert_eq!(start["cursor_storage"], "persistent");
    assert_eq!(start["complete"], false);
    let input = start["cursor"].as_str().unwrap();
    assert_ne!(live_token, input);
    for (cursor, source) in [(input, "live"), (live_token, "cache")] {
        let error = cached(
            &fixture,
            &app,
            json!({"cursor":cursor,"read_source":source}),
        )
        .await;
        assert_eq!(error["error"]["code"], "INVALID_INPUT", "{error}");
    }
    let page = cached(&fixture, &app, json!({"cursor":input})).await;
    let page = data(&page);
    assert_eq!(page["read_source"], "cache");
    assert_eq!(page["cache"]["scopes"].as_array().unwrap().len(), 2);
    assert_eq!((fixture.connections(), fixture.commands().len()), before);

    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
    fixture.stop_imap();
    let (app, runtime, cache, witness, _) = fixture.full_cache_router(&path);
    let replay = cached(
        &fixture,
        &app,
        json!({"cursor":input,"read_source":"cache"}),
    )
    .await;
    assert_eq!(data(&replay), page);
    let mut locations = BTreeSet::from_iter(
        location_pairs(start)
            .into_iter()
            .chain(location_pairs(page)),
    );
    let mut next = page["cursor"].as_str().map(str::to_owned);
    for _ in 0..20 {
        let Some(cursor) = next else { break };
        let page = cached(&fixture, &app, json!({"cursor":cursor})).await;
        let page = data(&page);
        locations.extend(location_pairs(page));
        next = page["cursor"].as_str().map(str::to_owned);
        if next.is_none() {
            assert_eq!(page["coverage"]["graph_complete"], true);
            assert_eq!(page["complete"], false);
        }
    }
    assert!(next.is_none(), "local graph did not finish");
    assert_eq!(
        locations,
        BTreeSet::from([
            ("INBOX".into(), 7),
            ("INBOX".into(), 8),
            ("Sent Items".into(), 7)
        ])
    );
    assert_eq!((fixture.connections(), fixture.commands().len()), before);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn cache_conversation_rejects_advance_and_replay_after_local_index_changes() {
    let root = tempfile::tempdir().unwrap();
    let path = configuration(root.path());
    let fixture = ThreadHarness::new(
        [
            ("INBOX", Folder::new("", 9, [(7, seed())])),
            (
                "Sent Items",
                Folder::new(
                    "\\Sent",
                    10,
                    [
                        (7, message(Some(PARENT), &[], None, "parent", "parent body")),
                        (
                            8,
                            message(Some(REPLY), &[SEED], None, "reply", "reply body"),
                        ),
                    ],
                ),
            ),
        ],
        64 * 1024,
    )
    .await;
    let (app, runtime, cache, witness, _) = fixture.full_cache_router(&path);
    warm(&fixture, &app, "INBOX", 9, &[(7, "seed body")]).await;
    warm(
        &fixture,
        &app,
        "Sent Items",
        10,
        &[(7, "parent body"), (8, "reply body")],
    )
    .await;
    let first = cached(&fixture, &app, cross_start("cache")).await;
    let replay = data(&first)["cursor"].as_str().unwrap();
    let second = cached(&fixture, &app, json!({"cursor":replay})).await;
    let advance = data(&second)["cursor"].as_str().unwrap();

    fixture.update("Sent Items", |folder| {
        folder.messages.insert(
            9,
            message(
                Some("late@example.test"),
                &[SEED],
                None,
                "late",
                "late body",
            ),
        );
    });
    warm(
        &fixture,
        &app,
        "Sent Items",
        10,
        &[(7, "parent body"), (8, "reply body"), (9, "late body")],
    )
    .await;
    fixture.stop_imap();
    let before = (fixture.connections(), fixture.commands().len());
    for cursor in [replay, advance] {
        let stale = cached(&fixture, &app, json!({"cursor":cursor})).await;
        assert!(
            matches!(
                stale["error"]["code"].as_str(),
                Some("CACHE_NEEDS_VERIFICATION" | "CURSOR_UNAVAILABLE")
            ),
            "{stale}"
        );
    }
    assert_eq!((fixture.connections(), fixture.commands().len()), before);
    drop(app);
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
}

#[tokio::test]
async fn live_conversation_fences_confirmed_candidate_disappearance_without_reading_its_body() {
    let parent = message(Some(PARENT), &[], None, "parent", "parent body");
    let raw_fetch = format!("UID FETCH 8 (UID BODY.PEEK[]<0.{}>)", parent.len());
    for (command, fault, missing) in [
        (
            "UID FETCH 8 (UID BODY.PEEK[HEADER]<0.131073>)",
            Fault::NoDataOk,
            true,
        ),
        ("UID FETCH 8 (UID RFC822.SIZE)", Fault::NoDataOk, true),
        (raw_fetch.as_str(), Fault::NoDataOk, true),
        (raw_fetch.as_str(), Fault::NoAfterData, false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = configuration(root.path());
        let fixture = ThreadHarness::new(
            [(
                "INBOX",
                Folder::new("", 9, [(7, seed()), (8, parent.clone())]),
            )],
            64 * 1024,
        )
        .await;
        let (app, runtime, cache, witness, _) = fixture.full_cache_router(&path);
        warm(
            &fixture,
            &app,
            "INBOX",
            9,
            &[(7, "seed body"), (8, "parent body")],
        )
        .await;
        let args = json!({"uid":8,"uid_validity":9,"read_source":"cache"});
        let before = fixture
            .call_tool(Some(&app), "alice", "get_email", args.clone())
            .await;
        assert_eq!(data(&before)["body"], "parent body");
        let first_command = fixture.commands().len();
        fixture.fault("INBOX", command, fault);
        let live = cached(
            &fixture,
            &app,
            json!({"uid":7,"uid_validity":9,"mailboxes":["INBOX"],"limit":1}),
        )
        .await;
        // The candidate body is deferred to another page. Its missing
        // evidence was observed only by the targeted HEADER/SIZE/raw command.
        assert_eq!(location_pairs(data(&live)), [("INBOX".into(), 7)], "{live}");
        let commands = fixture.commands();
        let commands = &commands[first_command..];
        assert!(
            commands.iter().any(|entry| entry.text == command),
            "{commands:?}"
        );
        assert!(
            commands
                .iter()
                .all(|entry| entry.text != "UID FETCH 8 (UID BODYSTRUCTURE)"),
            "{commands:?}",
        );
        let connections = fixture.connections();
        let after = fixture
            .call_tool(Some(&app), "alice", "get_email", args)
            .await;
        if missing {
            assert!(
                matches!(
                    after["error"]["code"].as_str(),
                    Some("CACHE_MISS" | "CACHE_NEEDS_VERIFICATION")
                ),
                "{command} {after}",
            );
        } else {
            assert_eq!(data(&after)["body"], "parent body");
        }
        assert_eq!(fixture.connections(), connections);
        drop(app);
        runtime.finish_cache(cache).await;
        drop(runtime);
        drop(witness);
    }
}
