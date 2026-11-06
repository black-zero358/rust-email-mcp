//! Signed HTTP acceptance for cross-mailbox conversation paging.
use super::*;

mod cache;
pub(super) mod fixture;
use fixture::{Fault, Folder, Harness as ThreadHarness, Search, message};
use std::collections::{BTreeSet, HashSet};

const SEED: &str = "seed@threads.example.test";
const PARENT: &str = "parent@threads.example.test";
const REPLY: &str = "reply@threads.example.test";

fn seed() -> Vec<u8> {
    message(Some(SEED), &[PARENT], None, "shared subject", "seed body")
}

fn start() -> Value {
    json!({"uid":7,"uid_validity":9,"mailboxes":["INBOX","Sent Items","Admin Vault"],"limit":20})
}

fn data(result: &Value) -> &Value {
    result
        .get("data")
        .unwrap_or_else(|| panic!("expected conversation data: {result}"))
}

fn location_pairs(page: &Value) -> Vec<(String, u32)> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|entity| entity["variants"].as_array().unwrap())
        .flat_map(|variant| variant["locations"].as_array().unwrap())
        .map(|location| {
            (
                location["mailbox"].as_str().unwrap().to_owned(),
                location["uid"].as_u64().unwrap() as u32,
            )
        })
        .collect()
}

async fn pages(h: &ThreadHarness, mut result: Value) -> Vec<Value> {
    let mut pages = Vec::new();
    for _ in 0..64 {
        let page = data(&result);
        assert_eq!(page["mode"], "cross_mailbox");
        pages.push(page.clone());
        let Some(cursor) = page["cursor"].as_str() else {
            assert_eq!(page["complete"], true, "{page}");
            return pages;
        };
        result = h.call("alice", json!({"cursor":cursor})).await;
    }
    panic!(
        "conversation did not finish after 64 pages: {:?}",
        h.commands()
    );
}

#[tokio::test]
async fn legacy_contract_and_cross_input_conflicts_are_distinct_before_network() {
    let h = ThreadHarness::new(
        [
            ("INBOX", Folder::new("", 9, [(7, seed())])),
            ("Sent Items", Folder::new("\\Sent", 10, [])),
            ("Admin Vault", Folder::new("", 11, [])),
        ],
        64 * 1024,
    )
    .await;
    let default = h.call("alice", json!({"uid":7,"uid_validity":9})).await;
    let explicit = h
        .call(
            "alice",
            json!({
                "mailbox":"INBOX","uid":7,"uid_validity":9,"candidate_limit":100,
                "limit":10,"body_mode":"full","html_display":"legacy"
            }),
        )
        .await;
    assert_eq!(data(&default), data(&explicit));
    assert_eq!(data(&default)["messages"][0]["body"], "seed body");
    assert_eq!(data(&default)["scope"]["seed_uid"], 7);
    assert!(data(&default).get("cursor").is_none());
    assert!(h.commands().iter().all(|cmd| cmd.mailbox == "INBOX"));

    let before = (h.connections(), h.commands().len());
    for args in [
        json!({"uid":7,"uid_validity":9,"scope":"standard","mailboxes":["INBOX"]}),
        json!({"uid":7,"uid_validity":9,"mailboxes":["INBOX"],"before_uid":8}),
        json!({"uid":7,"mailboxes":["INBOX"]}),
        json!({"uid":7,"uid_validity":9,"mailboxes":[]}),
        json!({"cursor":"00000000-0000-4000-8000-000000000001","uid":7}),
        json!({"cursor":"00000000-0000-4000-8000-000000000001","limit":1}),
    ] {
        let result = h.call("alice", args).await;
        assert_eq!(result["error"]["code"], "INVALID_INPUT", "{result}");
    }
    let mut missing = start();
    missing["account_id"] = json!("missing");
    let result = h.call("alice", missing).await;
    assert_eq!(result["error"]["code"], "ACCOUNT_UNAVAILABLE");
    h.catalog.revoke_read_for_test("default", "bob").unwrap();
    for args in [
        start(),
        json!({"cursor":"00000000-0000-4000-8000-000000000001"}),
    ] {
        let result = h.call("bob", args).await;
        assert_eq!(result["error"]["code"], "ACCOUNT_UNAVAILABLE");
    }
    assert_eq!((h.connections(), h.commands().len()), before);
}

#[tokio::test]
async fn three_folders_keep_equal_uids_distinct_and_confirm_graph_edges_exactly() {
    let h = ThreadHarness::new(
        [
            ("INBOX", Folder::new("", 9, [(7, seed())])),
            (
                "Sent Items",
                Folder::new(
                    "\\Sent",
                    10,
                    [(
                        7,
                        message(Some(PARENT), &[REPLY], None, "parent", "parent body"),
                    )],
                ),
            ),
            (
                "Admin Vault",
                Folder::new(
                    "",
                    11,
                    [
                        (
                            7,
                            message(Some(REPLY), &[], Some(SEED), "reply", "reply body"),
                        ),
                        (
                            8,
                            message(
                                Some("substring@threads.example.test"),
                                &["seed@threads.example.test.evil"],
                                None,
                                "shared subject",
                                "false positive",
                            ),
                        ),
                        (
                            9,
                            message(None, &[SEED], None, "anonymous", "message without ID"),
                        ),
                        (
                            10,
                            message(
                                Some("same-subject@threads.example.test"),
                                &[],
                                None,
                                "shared subject",
                                "unrelated",
                            ),
                        ),
                    ],
                ),
            ),
        ],
        512 * 1024,
    )
    .await;
    let first = h.call("alice", start()).await;
    let pages = pages(&h, first).await;
    let expected = [
        ("INBOX".to_owned(), 7),
        ("Sent Items".to_owned(), 7),
        ("Admin Vault".to_owned(), 7),
        ("Admin Vault".to_owned(), 9),
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let mut delivered = BTreeSet::new();
    let mut anonymous = 0;
    for page in &pages {
        for location in location_pairs(page) {
            assert!(
                delivered.insert(location),
                "same location was emitted again: {page}"
            );
        }
        anonymous += page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entity| entity["message_id"].is_null())
            .count();
    }
    assert_eq!(delivered, expected);
    assert_eq!(anonymous, 1);
    assert_eq!(pages.last().unwrap()["coverage"]["known_locations"], 4);
    let commands = h.commands();
    let mut seen = HashSet::new();
    for command in &commands {
        if let Some(search) = Search::parse(&command.text) {
            assert!(
                search.start > 0 && search.start <= search.end && search.end - search.start < 200
            );
            assert!(search.anchor.is_ascii());
            seen.insert((command.mailbox.as_str(), search.field.unwrap()));
        }
        assert!(!command.text.starts_with("SELECT "));
    }
    for mailbox in ["INBOX", "Sent Items", "Admin Vault"] {
        for field in ["Message-ID", "References", "In-Reply-To"] {
            assert!(seen.iter().any(
                |(name, seen_field)| *name == mailbox && seen_field.eq_ignore_ascii_case(field)
            ));
        }
    }
    assert!(commands.iter().any(|cmd| cmd.mailbox == "Admin Vault"
        && cmd.text.starts_with("UID FETCH 8 (UID BODY.PEEK[HEADER]")));
}

#[tokio::test]
async fn standard_scope_uses_confirmed_roles_and_administrator_archive_only() {
    let related = || message(Some(REPLY), &[SEED], None, "reply", "included");
    let h = ThreadHarness::configured(
        [
            ("INBOX", Folder::new("", 9, [(7, seed())])),
            ("Outbox", Folder::new("\\Sent", 10, [(7, related())])),
            ("Admin Vault", Folder::new("", 11, [(7, related())])),
            (
                "Unconfigured Archive",
                Folder::new("\\Archive", 12, [(7, related())]),
            ),
            ("All", Folder::new("\\All", 13, [(7, related())])),
            ("Drafts", Folder::new("\\Drafts", 14, [(7, related())])),
            ("Junk", Folder::new("\\Junk", 15, [(7, related())])),
            ("Trash", Folder::new("\\Trash", 16, [(7, related())])),
            (
                "Unavailable",
                Folder::new("\\Noselect \\Sent", 17, [(7, related())]),
            ),
        ],
        512 * 1024,
        |cfg| cfg.archive = Some("Admin Vault".into()),
    )
    .await;
    let result = h
        .call(
            "alice",
            json!({"uid":7,"uid_validity":9,"scope":"standard","limit":20}),
        )
        .await;
    let pages = pages(&h, result).await;
    let delivered = pages
        .iter()
        .flat_map(location_pairs)
        .map(|(name, _)| name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        delivered,
        ["Admin Vault", "INBOX", "Outbox"].map(str::to_owned).into()
    );
    let examined = h
        .commands()
        .into_iter()
        .filter(|cmd| cmd.text.starts_with("EXAMINE "))
        .map(|cmd| cmd.mailbox)
        .collect::<BTreeSet<_>>();
    assert_eq!(examined, delivered);
}

#[tokio::test]
async fn verified_duplicate_locations_and_conflicting_content_are_separate_variants() {
    let same = message(Some(SEED), &[], None, "same key headers", "identical body");
    let conflict = message(Some(SEED), &[], None, "same key headers", "different body");
    let h = ThreadHarness::new(
        [
            ("INBOX", Folder::new("", 9, [(7, same.clone())])),
            ("Sent Items", Folder::new("\\Sent", 10, [(7, same)])),
            ("Admin Vault", Folder::new("", 11, [(7, conflict)])),
        ],
        512 * 1024,
    )
    .await;
    let pages = pages(&h, h.call("alice", start()).await).await;
    let mut locations = BTreeMap::<String, BTreeSet<String>>::new();
    let mut entities = BTreeSet::new();
    let mut collision = false;
    for page in pages {
        for entity in page["messages"].as_array().unwrap() {
            assert_eq!(entity["message_id"], format!("<{SEED}>"));
            collision |= entity["identity_status"] == "collision";
            entities.insert(entity["entity_id"].as_str().unwrap().to_owned());
            for variant in entity["variants"].as_array().unwrap() {
                let known = locations
                    .entry(variant["variant_id"].as_str().unwrap().to_owned())
                    .or_default();
                known.extend(
                    variant["locations"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|location| location["mailbox"].as_str().unwrap().to_owned()),
                );
            }
        }
    }
    assert!(collision);
    assert_eq!(entities.len(), 1);
    assert_eq!(locations.len(), 2);
    assert!(
        locations
            .values()
            .any(|names| names == &["INBOX".into(), "Sent Items".into()].into())
    );
    assert!(
        locations
            .values()
            .any(|names| names == &["Admin Vault".into()].into())
    );
}

#[tokio::test]
async fn failed_search_preserves_its_exact_window_and_excludes_mail_above_frozen_watermark() {
    let same = message(Some(SEED), &[], None, "same ID", "copy");
    let mut inbox = Folder::new("", 9, [(7, same.clone()), (205, same.clone())]);
    inbox.next = Some(408);
    let h = ThreadHarness::new(
        [
            ("INBOX", inbox),
            (
                "Sent Items",
                Folder::new(
                    "\\Sent",
                    10,
                    [(
                        7,
                        message(
                            Some("healthy-reply@other.invalid"),
                            &[SEED],
                            None,
                            "reply",
                            "healthy mailbox",
                        ),
                    )],
                ),
            ),
            ("Admin Vault", Folder::new("", 11, [])),
        ],
        512 * 1024,
    )
    .await;
    let first_window = "UID SEARCH UID 208:407 HEADER Message-ID \"example\"";
    let failed_window = "UID SEARCH UID 8:207 HEADER Message-ID \"example\"";
    h.fault("INBOX", failed_window, Fault::NoAfterData);
    let first = h.call("alice", start()).await;
    let initial = data(&first);
    assert!(!location_pairs(initial).contains(&("INBOX".into(), 205)));
    assert!(initial["coverage"]["unresolved_errors"].as_u64().unwrap() > 0);
    let old = initial["cursor"].as_str().unwrap();
    let before = h.commands();
    let failed = before
        .iter()
        .find(|cmd| cmd.mailbox == "INBOX" && cmd.text == failed_window)
        .unwrap();
    assert!(
        before
            .iter()
            .any(|cmd| cmd.mailbox == "INBOX" && cmd.text == first_window)
    );
    assert!(
        !before
            .iter()
            .any(|cmd| cmd.mailbox == "INBOX" && cmd.text.starts_with("UID FETCH 205 "))
    );
    h.update("INBOX", |folder| {
        folder.next = Some(409);
        folder.messages.insert(408, same);
    });

    let pages = pages(&h, h.call("alice", json!({"cursor":old})).await).await;
    let updated = pages
        .iter()
        .flat_map(location_pairs)
        .collect::<BTreeSet<_>>();
    assert!(updated.contains(&("INBOX".into(), 205)));
    assert!(!updated.contains(&("INBOX".into(), 408)));
    let after = h.commands();
    let reopened = after[before.len()..]
        .iter()
        .find(|cmd| cmd.mailbox == "INBOX" && cmd.text == failed_window)
        .unwrap();
    assert_ne!(failed.connection, reopened.connection);
    assert!(
        after[before.len()..]
            .iter()
            .all(|cmd| cmd.mailbox != "INBOX" || cmd.text != first_window)
    );
    assert!(
        after
            .iter()
            .filter(|cmd| cmd.mailbox == "INBOX")
            .filter_map(|cmd| Search::parse(&cmd.text))
            .all(|search| search.end <= 407)
    );
}

#[tokio::test]
async fn healthy_copy_represents_a_new_entity_when_another_folder_temporarily_fails() {
    let child = message(Some(REPLY), &[SEED], None, "shared child", "duplicate body");
    let h = ThreadHarness::new(
        [
            (
                "INBOX",
                Folder::new("", 9, [(7, seed()), (8, child.clone())]),
            ),
            ("Sent Items", Folder::new("\\Sent", 10, [(7, child)])),
            ("Admin Vault", Folder::new("", 11, [])),
        ],
        512 * 1024,
    )
    .await;
    let mut args = start();
    args["limit"] = json!(1);
    let first = h.call("alice", args).await;
    assert_eq!(location_pairs(data(&first)), [("INBOX".into(), 7)]);
    let input = data(&first)["cursor"].as_str().unwrap();
    h.fault("INBOX", "EXAMINE \"INBOX\"", Fault::NoAfterData);
    let second = h.call("alice", json!({"cursor":input})).await;
    let second = data(&second);
    assert_eq!(location_pairs(second), [("Sent Items".into(), 7)]);
    assert!(second["coverage"]["unresolved_errors"].as_u64().unwrap() > 0);
    let entity = &second["messages"][0];
    let variant = &entity["variants"][0];
    assert_eq!(entity["message_id"], format!("<{REPLY}>"));
    assert_eq!(entity["new_entity"], true);
    assert_eq!(variant["new_variant"], true);
    assert_eq!(variant["representative"]["mailbox"], "Sent Items");
    let next = second["cursor"].as_str().unwrap();

    h.fault("Sent Items", "EXAMINE \"Sent Items\"", Fault::NoAfterData);
    let unavailable = h.call("alice", json!({"cursor":input})).await;
    assert_eq!(unavailable["error"]["code"], "MAIL_BACKEND");
    let replay = h.call("alice", json!({"cursor":input})).await;
    let replay = data(&replay);
    assert_eq!(replay["cursor"], next);
    assert_eq!(
        replay["messages"][0]["variants"][0]["representative"],
        variant["representative"]
    );
    assert_eq!(replay["messages"][0]["new_entity"], true);

    let after = pages(&h, h.call("alice", json!({"cursor":next})).await).await;
    let recovered = after
        .iter()
        .find(|page| location_pairs(page).contains(&("INBOX".into(), 8)))
        .unwrap();
    assert_eq!(recovered["messages"][0]["entity_id"], entity["entity_id"]);
    assert_eq!(recovered["messages"][0]["new_entity"], false);
    assert_eq!(
        recovered["messages"][0]["variants"][0]["new_variant"],
        false
    );
}

#[tokio::test]
async fn candidate_header_budget_is_shared_and_an_unseen_folder_message_waits_for_resume() {
    let mut folders = Vec::new();
    for (index, (mailbox, validity)) in [("INBOX", 9), ("Sent Items", 10), ("Admin Vault", 11)]
        .into_iter()
        .enumerate()
    {
        let mut folder = Folder::new("", validity, []);
        for uid in 20..95 {
            let id = format!("noise-{index}-{uid}@unrelated.invalid");
            let wrong = format!("noise-{uid}@threads.example.test");
            folder.messages.insert(
                uid,
                message(Some(&id), &[&wrong], None, "false positive", "noise"),
            );
        }
        if mailbox == "INBOX" {
            folder
                .messages
                .insert(7, message(Some(SEED), &[], None, "seed", "body"));
        }
        if mailbox == "Admin Vault" {
            folder.messages.insert(
                7,
                message(Some(REPLY), &[SEED], None, "related", "last verified"),
            );
        }
        folders.push((mailbox, folder));
    }
    let h = ThreadHarness::new(folders, 512 * 1024).await;
    let first = h.call("alice", start()).await;
    let page = data(&first);
    assert!(page["cursor"].is_string());
    assert!(!location_pairs(page).contains(&("Admin Vault".into(), 7)));
    let headers = h
        .commands()
        .into_iter()
        .filter(|cmd| cmd.text.contains("BODY.PEEK[HEADER]"))
        .filter_map(|cmd| {
            let uid = cmd
                .text
                .strip_prefix("UID FETCH ")?
                .split_once(' ')?
                .0
                .parse::<u32>()
                .ok()?;
            Some((cmd.mailbox, uid))
        })
        .collect::<BTreeSet<_>>();
    assert!(
        headers.len() <= 201,
        "seed plus at most 200 verified candidate headers"
    );
    let pages = pages(&h, first).await;
    assert!(
        pages
            .iter()
            .flat_map(location_pairs)
            .any(|pair| pair == ("Admin Vault".into(), 7))
    );
}

#[tokio::test]
async fn delivery_caps_results_and_body_bytes_across_folders() {
    let body = "多字节 synthetic body\n".repeat(1400);
    let mut folders = Vec::new();
    for (index, (mailbox, validity)) in [("INBOX", 9), ("Sent Items", 10), ("Admin Vault", 11)]
        .into_iter()
        .enumerate()
    {
        let mut folder = Folder::new("", validity, []);
        for uid in 20..=27 {
            let id = format!("reply-{index}-{uid}@other.invalid");
            folder
                .messages
                .insert(uid, message(Some(&id), &[SEED], None, "related", &body));
        }
        if mailbox == "INBOX" {
            folder
                .messages
                .insert(7, message(Some(SEED), &[], None, "seed", &body));
        }
        folders.push((mailbox, folder));
    }
    let h = ThreadHarness::new(folders, crate::config::DEFAULT_MAX_OUTPUT).await;
    let pages = pages(&h, h.call("alice", start()).await).await;
    let mut locations = BTreeSet::new();
    let mut body_budget_reached = false;
    for page in &pages {
        let batch = location_pairs(page);
        assert!(batch.len() <= 20);
        let bytes = page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|entity| entity["variants"].as_array().unwrap())
            .filter_map(|variant| variant["message"]["body"].as_str())
            .map(str::len)
            .sum::<usize>();
        assert!(bytes <= 256 * 1024);
        body_budget_reached |= !page["omitted_refs"].as_array().unwrap().is_empty();
        for location in batch {
            assert!(locations.insert(location));
        }
    }
    assert_eq!(locations.len(), 25);
    assert!(pages.len() >= 2);
    assert!(body_budget_reached);
}

fn paged_folders() -> [(&'static str, Folder); 3] {
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
        (
            "Admin Vault",
            Folder::new(
                "",
                11,
                [(
                    7,
                    message(
                        Some("archive@threads.example.test"),
                        &[REPLY],
                        None,
                        "archive",
                        "archive body",
                    ),
                )],
            ),
        ),
    ]
}

fn thin_start() -> Value {
    let mut args = start();
    args["candidate_limit"] = json!(1);
    args["limit"] = json!(1);
    args
}

async fn open_cursor(h: &ThreadHarness) -> String {
    let first = h.call("alice", thin_start()).await;
    data(&first)["cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("expected cursor: {first}"))
        .into()
}

#[tokio::test]
async fn resume_revalidates_uidvalidity_and_disables_stale_cursor() {
    let h = ThreadHarness::new(paged_folders(), 512 * 1024).await;
    let cursor = open_cursor(&h).await;
    h.update("Sent Items", |folder| folder.validity = 101);
    let stale = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(stale["error"]["code"], "STALE_UID");
    let before = (h.connections(), h.commands().len());
    let invalid = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(invalid["error"]["code"], "CURSOR_UNAVAILABLE");
    assert_eq!((h.connections(), h.commands().len()), before);
}

#[tokio::test]
async fn cursor_is_bound_to_the_subject_and_current_acl_and_expires_on_restart() {
    let h = ThreadHarness::new(paged_folders(), 512 * 1024).await;
    let cursor = open_cursor(&h).await;
    let before = (h.connections(), h.commands().len());
    let other = h.call("bob", json!({"cursor":cursor})).await;
    assert_eq!(other["error"]["code"], "CURSOR_UNAVAILABLE");
    let restarted = h.call_restarted("alice", json!({"cursor":cursor})).await;
    assert_eq!(restarted["error"]["code"], "CURSOR_UNAVAILABLE");
    h.catalog.revoke_read_for_test("default", "alice").unwrap();
    let revoked = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(revoked["error"]["code"], "ACCOUNT_UNAVAILABLE");
    assert_eq!((h.connections(), h.commands().len()), before);
}

#[tokio::test]
async fn nondefault_cursor_resumes_only_with_its_authorized_account() {
    const MAX_OUTPUT: usize = 512 * 1024;
    let mut secondary = paged_folders();
    secondary[0].1.messages.insert(
        7,
        message(
            Some("other-seed@threads.example.test"),
            &[PARENT],
            None,
            "second account seed",
            "second body",
        ),
    );
    let second = ThreadHarness::new(secondary, MAX_OUTPUT).await;
    let h = ThreadHarness::with_second_account(paged_folders(), MAX_OUTPUT, &second).await;

    let first = h.call("alice", thin_start()).await;
    assert_eq!(data(&first)["account_id"], "default");
    assert_eq!(
        data(&first)["messages"][0]["message_id"],
        format!("<{SEED}>")
    );
    let default_cursor = data(&first)["cursor"].as_str().unwrap();

    let mut args = thin_start();
    args["account_id"] = json!("second");
    let first = h.call("alice", args).await;
    let page = data(&first);
    assert_eq!(page["account_id"], "second");
    assert_eq!(
        page["seed"],
        json!({"account_id":"second","mailbox":"INBOX","uid_validity":9,"uid":7})
    );
    assert_eq!(
        page["messages"][0]["message_id"],
        "<other-seed@threads.example.test>"
    );
    let secondary_cursor = page["cursor"].as_str().unwrap();
    assert_ne!(secondary_cursor, default_cursor);

    let activity = |h: &ThreadHarness| (h.connections(), h.commands().len());
    let before = (activity(&h), activity(&second));
    for args in [
        json!({"cursor":secondary_cursor}),
        json!({"cursor":secondary_cursor,"account_id":"default"}),
        json!({"cursor":default_cursor,"account_id":"second"}),
    ] {
        let rejected = h.call("alice", args).await;
        assert_eq!(rejected["error"]["code"], "CURSOR_UNAVAILABLE");
    }
    assert_eq!((activity(&h), activity(&second)), before);

    let next = h
        .call(
            "alice",
            json!({"cursor":secondary_cursor,"account_id":"second"}),
        )
        .await;
    let next = data(&next);
    assert_eq!(next["account_id"], "second");
    assert_eq!(next["seed"], page["seed"]);
    assert_ne!(next["cursor"], page["cursor"]);
    assert_eq!(activity(&h), before.0);
    assert!(second.commands().len() > before.1.1);
}

#[tokio::test]
async fn expired_cursor_fails_before_backend_access() {
    let h = ThreadHarness::new(paged_folders(), 512 * 1024).await;
    let cursor = open_cursor(&h).await;
    let before = (h.connections(), h.commands().len());
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(601)).await;
    let expired = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(expired["error"]["code"], "CURSOR_UNAVAILABLE");
    assert_eq!((h.connections(), h.commands().len()), before);
}

#[tokio::test]
async fn concurrent_resume_is_rejected_and_cancellation_releases_the_lease() {
    let h = Arc::new(ThreadHarness::new(paged_folders(), 512 * 1024).await);
    let cursor = open_cursor(&h).await;
    let before = h.connections();
    h.fault("INBOX", "EXAMINE \"INBOX\"", Fault::Hang);
    let first = tokio::spawn({
        let h = h.clone();
        let cursor = cursor.clone();
        async move { h.call("alice", json!({"cursor":cursor})).await }
    });
    h.wait_for(|command| command.connection > before && command.text == "EXAMINE \"INBOX\"")
        .await;
    let busy_before = (h.connections(), h.commands().len());
    let busy = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(busy["error"]["code"], "CURSOR_UNAVAILABLE");
    assert_eq!((h.connections(), h.commands().len()), busy_before);
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let retried = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(data(&retried)["mode"], "cross_mailbox");
}

#[tokio::test]
async fn sixty_second_page_deadline_stops_a_stalled_search_and_preserves_cursor() {
    let h = Arc::new(
        ThreadHarness::configured(paged_folders(), 512 * 1024, |cfg| {
            cfg.timeout = Duration::from_secs(90);
        })
        .await,
    );
    let query = "UID SEARCH UID 1:7 HEADER Message-ID \"example\"";
    h.fault("INBOX", query, Fault::Hang);
    let started = tokio::time::Instant::now();
    let first = tokio::spawn({
        let h = h.clone();
        async move { h.call("alice", start()).await }
    });
    h.wait_for(|command| command.mailbox == "INBOX" && command.text == query)
        .await;
    tokio::time::pause();
    let just_before = started + Duration::from_secs(59);
    tokio::time::advance(just_before.saturating_duration_since(tokio::time::Instant::now())).await;
    assert!(!first.is_finished());
    tokio::time::advance(Duration::from_secs(2)).await;
    let result = first.await.unwrap();
    let page = data(&result);
    assert_eq!(page["complete"], false);
    assert!(page["cursor"].is_string());
    assert_eq!(page["coverage"]["completed_windows"], 0);
    assert_eq!(
        h.commands()
            .iter()
            .filter(|command| Search::parse(&command.text).is_some())
            .count(),
        1
    );
}

fn lean_wire_bytes(content: &Value, id: &RequestId) -> usize {
    let mut content = content.clone();
    let page = &mut content["data"];
    let mut omitted = Vec::new();
    for entity in page["messages"].as_array_mut().unwrap() {
        for variant in entity["variants"].as_array_mut().unwrap() {
            if variant.as_object_mut().unwrap().remove("message").is_some() {
                omitted.push(variant["representative"].clone());
            }
        }
    }
    page["omitted_refs"].as_array_mut().unwrap().extend(omitted);
    page["coverage"]["body_returned"] = json!(0);
    page["response_budget_omitted"] = json!(true);
    serde_json::to_vec(&JsonRpcResponse {
        jsonrpc: JsonRpcVersion2_0,
        id: id.clone(),
        result: ServerResult::CallToolResult(CallToolResult::structured(content)),
    })
    .unwrap()
    .len()
}

#[tokio::test]
async fn complete_wire_failure_keeps_the_old_cursor_replayable() {
    let sample = ThreadHarness::new(paged_folders(), 512 * 1024).await;
    let small_id = RequestId::Number(1);
    let large_id = RequestId::String("\n\"\\中".repeat(110).into());
    let mut args = start();
    args["limit"] = json!(1);
    let (first, first_size) = sample.measure("alice", args.clone(), json!(1)).await;
    let cursor = data(&first)["cursor"].as_str().unwrap();
    let (next, next_size) = sample
        .measure("alice", json!({"cursor":cursor}), json!(1))
        .await;
    assert_eq!(location_pairs(data(&next)).len(), 1);
    let max = lean_wire_bytes(&first, &small_id)
        .min(first_size)
        .max(lean_wire_bytes(&next, &small_id).min(next_size));
    assert!(lean_wire_bytes(&next, &large_id) > max);
    let narrow = ThreadHarness::new(paged_folders(), max).await;
    let first = narrow.call_id("alice", args, json!(1)).await;
    let old = data(&first)["cursor"].as_str().unwrap();
    let failed = narrow
        .call_id(
            "alice",
            json!({"cursor":old}),
            serde_json::to_value(&large_id).unwrap(),
        )
        .await;
    assert_eq!(failed["error"]["code"], "LIMIT_EXCEEDED");
    assert_eq!(
        failed["error"]["details"]["limit_kind"],
        "wire_response_bytes"
    );
    let retried = narrow
        .call_id("alice", json!({"cursor":old}), json!(1))
        .await;
    assert_eq!(location_pairs(data(&retried)), location_pairs(data(&next)));
    let expected_cursor = data(&retried)["cursor"].clone();
    let before = narrow
        .commands()
        .iter()
        .filter(|cmd| Search::parse(&cmd.text).is_some())
        .count();
    let replay = narrow
        .call_id("alice", json!({"cursor":old}), json!(1))
        .await;
    assert_eq!(data(&replay)["cursor"], expected_cursor);
    assert_eq!(
        location_pairs(data(&replay)),
        location_pairs(data(&retried))
    );
    assert_eq!(
        narrow
            .commands()
            .iter()
            .filter(|cmd| Search::parse(&cmd.text).is_some())
            .count(),
        before
    );
}

#[tokio::test]
async fn wire_omitted_resume_replays_the_committed_page_with_a_shorter_id() {
    fn folders() -> [(&'static str, Folder); 3] {
        let mut folders = paged_folders();
        let body_suffix = [b'x'; 8 * 1024];
        for (_, folder) in &mut folders[1..] {
            for raw in folder.messages.values_mut() {
                raw.extend_from_slice(&body_suffix);
            }
        }
        folders
    }

    let sample = ThreadHarness::new(folders(), 512 * 1024).await;
    let mut args = start();
    args["limit"] = json!(1);
    let (first, first_bytes) = sample.measure("alice", args.clone(), json!(1)).await;
    let cursor = data(&first)["cursor"].as_str().unwrap();
    let (next, max_output) = sample
        .measure("alice", json!({"cursor":cursor}), json!(1))
        .await;
    assert!(first_bytes < max_output);
    assert_eq!(data(&next)["coverage"]["body_returned"], 1);
    assert_eq!(data(&next)["response_budget_omitted"], false);
    let long_id = RequestId::String("\n\"\\中".repeat(110).into());
    assert!(lean_wire_bytes(&next, &long_id) < max_output);

    let narrow = ThreadHarness::new(folders(), max_output).await;
    let first = narrow.call_id("alice", args, json!(1)).await;
    let old = data(&first)["cursor"].as_str().unwrap();
    let omitted = narrow
        .call_id(
            "alice",
            json!({"cursor":old}),
            serde_json::to_value(&long_id).unwrap(),
        )
        .await;
    let committed = data(&omitted);
    assert_eq!(committed["response_budget_omitted"], true);
    assert_eq!(committed["coverage"]["body_returned"], 0);
    assert_eq!(committed["omitted_refs"].as_array().unwrap().len(), 1);
    assert_eq!(location_pairs(committed), location_pairs(data(&next)));

    let before = narrow.commands().len();
    let replay = narrow
        .call_id("alice", json!({"cursor":old}), json!(1))
        .await;
    assert_eq!(data(&replay), committed);
    assert!(
        narrow.commands()[before..].iter().all(|cmd| {
            Search::parse(&cmd.text).is_none() && !cmd.text.starts_with("UID FETCH ")
        })
    );
}

#[tokio::test]
async fn completed_replay_rechecks_a_frozen_folder_with_no_delivered_locations() {
    let mut folders = paged_folders();
    folders[2].1.messages.clear();
    let h = ThreadHarness::new(folders, 512 * 1024).await;
    let mut cursor = open_cursor(&h).await;
    let mut finished = None;
    for _ in 0..16 {
        let result = h.call("alice", json!({"cursor":cursor})).await;
        let page = data(&result);
        assert!(
            location_pairs(page)
                .iter()
                .all(|(mailbox, _)| mailbox != "Admin Vault")
        );
        if page["complete"] == true {
            assert!(page["cursor"].is_null());
            finished = Some(page.clone());
            break;
        }
        cursor = page["cursor"].as_str().unwrap().into();
    }
    let finished = finished.expect("conversation terminal page");
    assert!(
        finished["folders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|folder| folder["mailbox"] == "Admin Vault" && folder["uid_validity"] == 11)
    );
    h.fault("Admin Vault", "EXAMINE \"Admin Vault\"", Fault::NoAfterData);
    let failed = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(failed["error"]["code"], "MAIL_BACKEND");
    assert_eq!(failed["error"]["retryable"], true);
    assert!(failed.get("data").is_none());
    let replayed = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(data(&replayed), &finished);

    h.fault("INBOX", "EXAMINE \"INBOX\"", Fault::NoAfterData);
    h.update("Admin Vault", |folder| folder.validity = 12);
    let stale = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(stale["error"]["code"], "STALE_UID");
    let before = (h.connections(), h.commands().len());
    let discarded = h.call("alice", json!({"cursor":cursor})).await;
    assert_eq!(discarded["error"]["code"], "CURSOR_UNAVAILABLE");
    assert_eq!((h.connections(), h.commands().len()), before);
}

#[tokio::test]
async fn fingerprints_obey_the_account_raw_limit_while_body_parts_remain_readable() {
    let body = "x".repeat(40 * 1024);
    let copy = message(Some(SEED), &[], None, "same message", &body);
    let h = ThreadHarness::configured(
        [
            ("INBOX", Folder::new("", 9, [(7, copy.clone())])),
            ("Sent Items", Folder::new("\\Sent", 10, [(7, copy)])),
            ("Admin Vault", Folder::new("", 11, [])),
        ],
        512 * 1024,
        |cfg| cfg.max_message = 32 * 1024,
    )
    .await;
    let result = pages(&h, h.call("alice", start()).await).await;
    let mut variants = BTreeSet::new();
    let mut received = 0;
    for page in result {
        for entity in page["messages"].as_array().unwrap() {
            assert_eq!(entity["identity_status"], "identity_unverified");
            for variant in entity["variants"].as_array().unwrap() {
                variants.insert(variant["variant_id"].as_str().unwrap().to_owned());
                assert_eq!(variant["message"]["body"], body);
                received += 1;
            }
        }
    }
    assert_eq!(variants.len(), 2);
    assert_eq!(received, 2);
    let commands = h.commands();
    assert!(commands.iter().all(|cmd| !cmd.text.contains("BODY.PEEK[]")));
    assert!(
        commands
            .iter()
            .any(|cmd| cmd.text.contains("BODY.PEEK[1]<"))
    );
}

#[tokio::test]
async fn a_failed_candidate_is_attempted_once_per_page_across_header_fields() {
    let h = ThreadHarness::new(
        [
            (
                "INBOX",
                Folder::new(
                    "",
                    9,
                    [(7, message(Some(SEED), &[], None, "seed", "seed body"))],
                ),
            ),
            (
                "Sent Items",
                Folder::new(
                    "\\Sent",
                    10,
                    [(
                        8,
                        message(
                            Some("child@other.invalid"),
                            &[SEED],
                            Some(SEED),
                            "child",
                            "child body",
                        ),
                    )],
                ),
            ),
            ("Admin Vault", Folder::new("", 11, [])),
        ],
        512 * 1024,
    )
    .await;
    let header_fetch = "UID FETCH 8 (UID BODY.PEEK[HEADER]<0.131073>)";
    h.fault("Sent Items", header_fetch, Fault::NoAfterData);
    let first = h.call("alice", start()).await;
    let page = data(&first);
    assert!(!location_pairs(page).contains(&("Sent Items".into(), 8)));
    let cursor = page["cursor"].as_str().unwrap();
    let before = h.commands();
    assert_eq!(
        before
            .iter()
            .filter(|cmd| cmd.mailbox == "Sent Items" && cmd.text == header_fetch)
            .count(),
        1
    );
    for field in ["References", "In-Reply-To"] {
        assert!(before.iter().any(|cmd| cmd.mailbox == "Sent Items"
            && Search::parse(&cmd.text).is_some_and(|search| search.field == Some(field))));
    }
    let after = pages(&h, h.call("alice", json!({"cursor":cursor})).await).await;
    assert!(
        after
            .iter()
            .flat_map(location_pairs)
            .any(|pair| pair == ("Sent Items".into(), 8))
    );
    assert!(
        h.commands()[before.len()..]
            .iter()
            .any(|cmd| cmd.mailbox == "Sent Items" && cmd.text == header_fetch)
    );
}

#[tokio::test]
async fn an_initially_unavailable_folder_reports_how_to_restart_after_healthy_work() {
    let h = ThreadHarness::new(paged_folders(), 512 * 1024).await;
    h.fault("Admin Vault", "EXAMINE \"Admin Vault\"", Fault::NoAfterData);
    let result = h.call("alice", start()).await;
    let page = data(&result);
    assert_eq!(page["complete"], false);
    assert_eq!(page["coverage"]["remaining_windows"], 0);
    assert_eq!(page["coverage"]["pending_candidates"], 0);
    assert_eq!(page["coverage"]["delivery_pending"], false);
    assert_eq!(
        page["cursor_action"],
        "restart_later_or_with_narrower_scope"
    );
    let folder = page["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["mailbox"] == "Admin Vault")
        .unwrap();
    assert!(folder["uid_validity"].is_null());
    assert!(folder["high_uid"].is_null());
    assert!(folder["error"].is_object());
    assert!(
        location_pairs(page)
            .iter()
            .all(|(mailbox, _)| mailbox != "Admin Vault")
    );
    assert!(
        h.commands()
            .iter()
            .filter(|cmd| cmd.mailbox == "Admin Vault")
            .all(|cmd| cmd.text.starts_with("EXAMINE "))
    );
}
