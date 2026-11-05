use super::{
    reconcile::{self, Mode},
    scan::{self, Selected, Window},
};
use crate::accounts::AccountKey;
use async_imap::Session;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    task::JoinHandle,
};

async fn command(server: &mut BufReader<DuplexStream>, expected: &str) -> String {
    let mut line = String::new();
    assert_ne!(server.read_line(&mut line).await.unwrap(), 0);
    let (tag, actual) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
    assert_eq!(actual, expected);
    tag.into()
}

async fn selected(
    account: &AccountKey,
    validity: u32,
    uid_next: u32,
    reply: Option<(&str, Vec<u8>, &str)>,
) -> (Session<DuplexStream>, Selected, JoinHandle<()>) {
    let (client, server) = tokio::io::duplex(4096);
    let reply =
        reply.map(|(command, bytes, status)| (command.to_owned(), bytes, status.to_owned()));
    let task = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        let tag = command(&mut server, "LOGIN \"synthetic\" \"local\"").await;
        server
            .write_all(format!("{tag} OK login\r\n").as_bytes())
            .await
            .unwrap();
        let tag = command(&mut server, "EXAMINE \"INBOX\"").await;
        server.write_all(format!(
            "* 0 EXISTS\r\n* OK [UIDVALIDITY {validity}] valid\r\n* OK [UIDNEXT {uid_next}] next\r\n{tag} OK [READ-ONLY] selected\r\n"
        ).as_bytes()).await.unwrap();
        if let Some((expected, bytes, status)) = reply {
            let tag = command(&mut server, &expected).await;
            if server.write_all(&bytes).await.is_err() {
                return;
            }
            if status == "EOF" {
                server.shutdown().await.unwrap();
                return;
            }
            if status != "HANG"
                && server
                    .write_all(format!("{tag} {status} done\r\n").as_bytes())
                    .await
                    .is_err()
            {
                return;
            }
        }
        let mut extra = String::new();
        assert_eq!(server.read_line(&mut extra).await.unwrap(), 0, "{extra}");
    });
    let mut session = async_imap::Client::with_buffer_limit(client, 2 * 1024 * 1024)
        .login("synthetic", "local")
        .await
        .unwrap();
    let (observed, _) = Selected::examine(&mut session, account, "INBOX")
        .await
        .unwrap();
    assert!(observed.matches_account(account));
    (session, observed, task)
}

fn search_line(range: std::ops::RangeInclusive<u32>) -> Vec<u8> {
    let mut bytes = b"* SEARCH".to_vec();
    for uid in range {
        use std::io::Write;
        write!(bytes, " {uid}").unwrap();
    }
    bytes.extend_from_slice(b"\r\n");
    bytes
}

#[test]
fn numeric_windows_never_wrap_or_reverse() {
    assert!(Window::from_width(1, 0, 200).is_none());
    assert!(Window::from_width(0, 9, 200).is_none());
    assert!(Window::from_width(1, 9, 0).is_none());
    let window = Window::from_width(u32::MAX - 3, u32::MAX - 1, u32::MAX).unwrap();
    assert_eq!(window.high, u32::MAX - 1);
    assert!(window.narrow().is_none());
}

#[tokio::test]
async fn only_single_result_uid_overflow_reselects_and_retries_same_start() {
    let account = AccountKey::synthetic();
    let window = Window::from_width(100, 1000, 2000).unwrap();
    for validity in [7, 8] {
        let (session, original, first) = selected(
            &account,
            7,
            1001,
            Some(("UID SEARCH UID 100:1000", search_line(100..=300), "OK")),
        )
        .await;
        let (fresh, checked, second) = selected(
            &account,
            validity,
            2001,
            (validity == 7).then_some((
                "UID SEARCH UID 100:299",
                b"* SEARCH 260 100\r\n".to_vec(),
                "OK",
            )),
        )
        .await;
        let result = scan::discover(session, original, 7, 1000, window, || async {
            first.await.unwrap();
            Ok((fresh, checked))
        })
        .await;
        second.await.unwrap();
        if validity == 7 {
            let receipt = result.unwrap();
            assert_eq!(
                receipt.window,
                Window {
                    low: 100,
                    high: 299
                }
            );
            assert_eq!(receipt.uids, [100, 260]);
        } else {
            assert_eq!(result.unwrap_err().code, "STALE_UID");
        }
    }
}

#[tokio::test]
async fn incomplete_wrong_window_and_duplicate_search_do_not_reconnect() {
    let account = AccountKey::synthetic();
    let window = Window::from_width(100, 1000, 2000).unwrap();
    let mut duplicate = search_line(100..=298);
    duplicate.extend_from_slice(b"* SEARCH 100 101\r\n");
    let mut duplicate_overflow = search_line(100..=299);
    duplicate_overflow.truncate(duplicate_overflow.len() - 2);
    duplicate_overflow.extend_from_slice(b" 100\r\n");
    for (data, status, empty) in [
        (b"* SEARCH\r\n".to_vec(), "OK", true),
        (b"* SEARCH 99\r\n".to_vec(), "OK", false),
        (b"* 0 EXISTS\r\n".to_vec(), "OK", false),
        (b"* SEARCH 101\r\n".to_vec(), "NO", false),
        (b"* SEARCH 101\r\n".to_vec(), "EOF", false),
        (duplicate, "OK", false),
        (duplicate_overflow, "OK", false),
    ] {
        let (session, observed, server) = selected(
            &account,
            7,
            1001,
            Some(("UID SEARCH UID 100:1000", data, status)),
        )
        .await;
        let result = scan::discover(session, observed, 7, 1000, window, || async {
            panic!("only a single oversized SEARCH result may shrink")
        })
        .await;
        if empty {
            let receipt = result.unwrap();
            assert!(receipt.uids.is_empty());
            assert_eq!(receipt.window, window);
        } else {
            assert!(result.is_err());
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn flags_require_present_attribute_and_only_tagged_ok_proves_absence() {
    let account = AccountKey::synthetic();
    for (data, status, complete) in [
        (
            "* 1 FETCH (UID 7 FLAGS (\\Seen))\r\n* 1 FETCH (UID 7 FLAGS ())\r\n* 2 FETCH (UID 99 FLAGS ())\r\n",
            "OK",
            true,
        ),
        ("* 1 FETCH (UID 7)\r\n", "OK", false),
        (
            "* 1 FETCH (UID 7 FLAGS ())\r\n* 2 FETCH (FLAGS ())\r\n",
            "OK",
            false,
        ),
        ("* 1 FETCH (UID 7 FLAGS ())\r\n", "BAD", false),
        ("* 1 FETCH (UID 7 FLAGS ())\r\n", "EOF", false),
        ("* 1 FETCH (UID 7 FLAGS ())\r\n* 1 EXPUNGE\r\n", "OK", false),
    ] {
        let (mut session, observed, server) = selected(
            &account,
            7,
            9,
            Some((
                "UID FETCH 7,8 (UID FLAGS)",
                data.as_bytes().to_vec(),
                status,
            )),
        )
        .await;
        let result = reconcile::collect(&mut session, &observed, &[7, 8], Mode::Flags).await;
        drop(session);
        server.await.unwrap();
        if complete {
            let proof = result.unwrap();
            assert_eq!(proof.absent, [8]);
            assert_eq!(proof.present.len(), 1);
            assert_eq!(proof.present[0].uid, 7);
            assert!(proof.present[0].flags.is_empty());
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn facts_merge_target_fragments_with_exact_header_origin_and_all_attributes() {
    let account = AccountKey::synthetic();
    let command = "UID FETCH 7 (UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER]<0.131073>)";
    for (origin, include_date, good) in [(0, true, true), (1, true, false), (0, false, false)] {
        let date = if include_date {
            " INTERNALDATE \" 7-Feb-2025 09:00:00 +0000\""
        } else {
            ""
        };
        let data = format!(
            "* 1 FETCH (UID 7 BODY[HEADER]<{origin}> {{8}}\r\nX: a\r\n\r\n)\r\n* 1 FETCH (UID 7 RFC822.SIZE 8 FLAGS (\\Seen){date})\r\n"
        );
        let (mut session, observed, server) =
            selected(&account, 7, 9, Some((command, data.into_bytes(), "OK"))).await;
        let result = reconcile::collect(&mut session, &observed, &[7], Mode::Facts).await;
        drop(session);
        server.await.unwrap();
        if good {
            let proof = result.unwrap();
            assert!(proof.absent.is_empty());
            let [item] = proof.present.as_slice() else {
                panic!("missing target facts")
            };
            assert_eq!(item.size, Some(8));
            assert_eq!(item.flags, ["Seen"]);
            assert!(
                item.internal_date
                    .as_deref()
                    .unwrap()
                    .starts_with("2025-02-07")
            );
            let header = item.header.as_ref().unwrap();
            assert!(header.complete);
            assert_eq!(header.bytes.as_ref(), b"X: a\r\n\r\n");
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn protocol_receipts_atomically_advance_discovery_and_dirty_reconciliation() {
    use crate::cache::{
        AccountScope, Cache, Ref, ThreadRole,
        sync_state::{ScanKind, SyncError},
    };
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = Cache::open_full(
        &directory.path().join("cache"),
        16 * 1024 * 1024,
        vec![AccountScope {
            account: account.clone(),
            mailboxes: vec!["INBOX".into()],
            max_bytes: 16 * 1024 * 1024,
        }],
        &[],
    )
    .unwrap();
    let (session, observed, server) = selected(
        &account,
        7,
        10,
        Some(("UID SEARCH UID 1:9", b"* SEARCH 7 8\r\n".to_vec(), "NO")),
    )
    .await;
    let history = cache
        .start_sync(
            &account,
            "INBOX",
            ScanKind::History,
            observed.observation(1),
        )
        .unwrap();
    let window = Window::from_width(1, history.stamp.high_uid, history.width_hint).unwrap();
    assert!(
        scan::discover(session, observed, 7, 9, window, || async {
            panic!("tagged rejection must never reconnect")
        })
        .await
        .is_err()
    );
    server.await.unwrap();
    assert_eq!(
        cache.sync_status(&account, "INBOX").unwrap(),
        std::slice::from_ref(&history)
    );

    let (session, observed, server) = selected(
        &account,
        7,
        10,
        Some(("UID SEARCH UID 1:9", b"* SEARCH 7 8\r\n".to_vec(), "HANG")),
    )
    .await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            scan::discover(session, observed, 7, 9, window, || async {
                panic!("a cancelled SEARCH must not reconnect")
            }),
        )
        .await
        .is_err()
    );
    server.await.unwrap();
    assert_eq!(
        cache.sync_status(&account, "INBOX").unwrap(),
        std::slice::from_ref(&history)
    );

    let (session, observed, server) = selected(
        &AccountKey::synthetic(),
        7,
        10,
        Some(("UID SEARCH UID 1:9", b"* SEARCH 7 8\r\n".to_vec(), "OK")),
    )
    .await;
    let wrong_account = scan::discover(session, observed, 7, 9, window, || async {
        panic!("the bounded SEARCH succeeded")
    })
    .await
    .unwrap();
    assert!(matches!(
        wrong_account.commit(&cache, &history.stamp, &|| false),
        Err(SyncError::Invalid)
    ));
    server.await.unwrap();

    let (session, observed, server) = selected(
        &account,
        7,
        10,
        Some(("UID SEARCH UID 1:9", b"* SEARCH 8 7\r\n".to_vec(), "OK")),
    )
    .await;
    let receipt = scan::discover(session, observed, 7, 9, window, || async {
        panic!("the bounded SEARCH succeeded")
    })
    .await
    .unwrap();
    let history = receipt.commit(&cache, &history.stamp, &|| false).unwrap();
    assert!(history.complete);
    server.await.unwrap();

    let header = "Subject: synthetic history\r\nMessage-ID: <history@example.test>\r\n\r\n";
    let facts = format!(
        "* 1 FETCH (UID 7 FLAGS (\\Seen) INTERNALDATE \" 7-Feb-2025 09:00:00 +0000\" RFC822.SIZE {} BODY[HEADER]<0> {{{}}}\r\n{header})\r\n",
        header.len(),
        header.len(),
    );
    let (mut session, observed, server) = selected(
        &account,
        7,
        10,
        Some((
            "UID FETCH 7 (UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER]<0.131073>)",
            facts.into_bytes(),
            "OK",
        )),
    )
    .await;
    let work = cache.sync_content_page(&history.stamp, 1, 1).unwrap();
    assert_eq!(work[0].uid, 7);
    let proof = reconcile::collect(&mut session, &observed, &[7], Mode::Facts)
        .await
        .unwrap();
    proof.commit_facts(&cache, &work[0], 1, &|| false).unwrap();
    let snapshot = cache.freeze_scope(&account, "INBOX").unwrap();
    assert_eq!(
        cache
            .thread_uids(
                &account,
                &snapshot,
                ThreadRole::MessageId,
                "<history@example.test>",
                1..=9
            )
            .unwrap(),
        [7],
    );
    drop(session);
    server.await.unwrap();

    let (session, observed, server) = selected(
        &account,
        7,
        13,
        Some(("UID SEARCH UID 10:12", b"* SEARCH\r\n".to_vec(), "OK")),
    )
    .await;
    let tail = cache
        .start_sync(&account, "INBOX", ScanKind::Tail, observed.observation(2))
        .unwrap();
    let window = Window::from_width(10, tail.stamp.high_uid, tail.width_hint).unwrap();
    let receipt = scan::discover(session, observed, 7, 12, window, || async {
        panic!("the empty bounded SEARCH succeeded")
    })
    .await
    .unwrap();
    assert!(
        receipt
            .commit(&cache, &tail.stamp, &|| false)
            .unwrap()
            .complete
    );
    server.await.unwrap();

    let ticket = cache.begin_dirty(&account, &["INBOX"]).unwrap();
    cache.finish_dirty(ticket).unwrap();
    let fence = cache.sync_reconcile_fence(&account, "INBOX").unwrap();
    let (mut session, observed, server) = selected(
        &account,
        7,
        13,
        Some((
            "UID FETCH 7,8 (UID FLAGS)",
            b"* 1 FETCH (UID 7 FLAGS ())\r\n".to_vec(),
            "OK",
        )),
    )
    .await;
    let reconcile = cache
        .start_sync_reconcile(&account, "INBOX", observed.observation(3), &fence)
        .unwrap();
    let page = cache.sync_known_page(&reconcile.stamp, 200).unwrap();
    assert_eq!(page.uids, [7, 8]);
    let proof = reconcile::collect(&mut session, &observed, &page.uids, Mode::Flags)
        .await
        .unwrap();
    drop(session);
    server.await.unwrap();
    assert!(matches!(
        proof.commit_reconcile(&cache, &reconcile.stamp, &page, 4, &|| true),
        Err(SyncError::Cancelled)
    ));
    assert!(cache.offline_scope_allowed(&account, "INBOX", 7).is_err());
    assert!(
        proof
            .commit_reconcile(&cache, &reconcile.stamp, &page, 4, &|| false)
            .unwrap()
            .complete
    );
    let reference = |uid| Ref {
        account: &account,
        mailbox: "INBOX",
        uid_validity: 7,
        uid,
    };
    assert_eq!(
        cache.cached_message(reference(7)).unwrap().unwrap().flags,
        Some(vec![])
    );
    assert!(cache.cached_message(reference(8)).unwrap().is_none());
}

#[tokio::test]
async fn completed_empty_examine_needs_no_empty_uid_fetch_to_clear_dirty() {
    use crate::cache::{AccountScope, Cache, sync_state::ScanKind};
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = Cache::open_full(
        &directory.path().join("cache"),
        8 * 1024 * 1024,
        vec![AccountScope {
            account: account.clone(),
            mailboxes: vec!["INBOX".into()],
            max_bytes: 8 * 1024 * 1024,
        }],
        &[],
    )
    .unwrap();
    let (session, observed, server) = selected(&account, 7, 1, None).await;
    for kind in [ScanKind::History, ScanKind::Tail] {
        assert!(
            cache
                .start_sync(&account, "INBOX", kind, observed.observation(1))
                .unwrap()
                .complete
        );
    }
    drop(session);
    server.await.unwrap();

    let ticket = cache.begin_dirty(&account, &["INBOX"]).unwrap();
    cache.finish_dirty(ticket).unwrap();
    let fence = cache.sync_reconcile_fence(&account, "INBOX").unwrap();
    let (session, observed, server) = selected(&account, 7, 1, None).await;
    let scan = cache
        .start_sync_reconcile(&account, "INBOX", observed.observation(2), &fence)
        .unwrap();
    let page = cache.sync_known_page(&scan.stamp, 200).unwrap();
    let proof = reconcile::Proof::empty_reconciliation(&observed, &page).unwrap();
    assert!(
        proof
            .commit_reconcile(&cache, &scan.stamp, &page, 2, &|| false)
            .unwrap()
            .complete
    );
    assert!(cache.offline_scope_allowed(&account, "INBOX", 7).is_ok());
    drop(session);
    server.await.unwrap();
}
