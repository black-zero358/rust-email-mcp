use super::*;
use crate::mail::sync::tests::scripted_session;

const ENABLE: (&str, &str) = (
    "ENABLE QRESYNC",
    "* ENABLED QRESYNC\r\n{tag} OK enabled\r\n",
);

#[tokio::test]
async fn a_new_epoch_or_nomodseq_never_returns_old_delta_evidence() {
    for (reply, epoch, state) in [
        (
            "* OK [UIDVALIDITY 10]\r\n* OK [UIDNEXT 2]\r\n* OK [HIGHESTMODSEQ 8]\r\n* VANISHED (EARLIER) 4\r\n{tag} OK [READ-ONLY] epoch changed\r\n",
            10,
            ModSeq::Highest(8),
        ),
        (
            "* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 5]\r\n* OK [NOMODSEQ]\r\n* VANISHED (EARLIER) 4\r\n{tag} OK [READ-ONLY] no modseq\r\n",
            9,
            ModSeq::Unavailable,
        ),
    ] {
        let (client, peer) = scripted_session(
            vec![ENABLE, ("EXAMINE \"INBOX\" (QRESYNC (9 4 4))", reply)],
            false,
        )
        .await;
        let opened = enable(client)
            .await
            .unwrap()
            .examine_known(&AccountKey::synthetic(), "INBOX", 9, 4, &[4])
            .await
            .unwrap();
        assert_eq!(
            (opened.selected.uid_validity, opened.modseq),
            (epoch, state)
        );
        assert!(opened.evidence.earlier.is_empty());
        assert!(opened.evidence.live.is_empty());
        assert!(opened.evidence.flags.is_empty());
        drop(opened);
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn realtime_deletion_fences_offline_access_before_fetch_completion() {
    use crate::{
        accounts::{AccountCatalog, CacheRunIdentity},
        cache::{Ref, sync_state::*},
        config::{self, ValidatedAccountSet},
        journal::Journal,
        mail::MailRuntime,
    };
    use std::{path::Path, sync::Arc, time::Duration};
    use tokio::sync::oneshot;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("cache.json");
    std::fs::write(&path, br#"{"version":1,"mode":"full","directory":"cache","max_bytes":33554432,"accounts":[{"account_id":"default","mailboxes":["INBOX"],"max_bytes":8388608}]}"#).unwrap();
    let settings = ValidatedAccountSet::legacy(config::fixture());
    let cache_settings = config::load_cache_file(&path, &settings).unwrap();
    let catalog = AccountCatalog::from_test_accounts(settings).unwrap();
    let account = catalog.registered_accounts().pop().unwrap().key;
    let persistent = MailRuntime::prepare_cache_for_run(
        &catalog,
        cache_settings,
        Some(CacheRunIdentity {
            instance: uuid::Uuid::new_v4(),
            previous: None,
            current: uuid::Uuid::new_v4(),
        }),
    )
    .unwrap();
    let mail = MailRuntime::with_cache(
        catalog,
        Arc::new(Journal::open(Path::new(":memory:")).unwrap()),
        persistent.clone(),
    )
    .unwrap();
    let cache = persistent.full_storage(&account).unwrap();
    let observed = MailboxObservation {
        uid_validity: 9,
        uid_next: 10,
        checked_at_ms: 1,
    };
    let history = cache
        .start_sync(&account, "INBOX", ScanKind::History, observed)
        .unwrap();
    cache
        .record_sync_search(&history.stamp, 1, 9, &[7, 8, 9], &|| false)
        .unwrap();
    let fence = cache.sync_reconcile_fence(&account, "INBOX").unwrap();
    let audit = cache
        .start_sync_reconcile(&account, "INBOX", observed, &fence)
        .unwrap();
    let base = DeltaObservation {
        uid_validity: 9,
        uid_next: 10,
        highest_modseq: 4,
        checked_at_ms: 1,
    };
    cache
        .prepare_delta_seed(&audit.stamp, base, &|| false)
        .unwrap();
    let page = cache.sync_known_page(&audit.stamp, 200).unwrap();
    let seen = ["Seen".to_owned()];
    let verified = page
        .uids
        .iter()
        .map(|&uid| ReconciledFlags { uid, flags: &seen })
        .collect::<Vec<_>>();
    cache
        .commit_sync_reconcile(&audit.stamp, &page, &verified, 1, &|| false)
        .unwrap();
    for uid in page.uids {
        let reference = Ref {
            account: &account,
            mailbox: "INBOX",
            uid_validity: 9,
            uid,
        };
        cache
            .remember_headers(reference, true, b"Subject: synthetic\r\n\r\n", None)
            .unwrap();
    }
    let target = DeltaObservation {
        highest_modseq: 8,
        checked_at_ms: 2,
        ..base
    };
    let DeltaStart::Round(round) = cache
        .start_delta(
            &account,
            "INBOX",
            DeltaLane::FlagsAndExpunges,
            target,
            &fence,
            &|| false,
        )
        .unwrap()
    else {
        panic!("expected a deletion round")
    };
    let page = cache.delta_page(&round, 200).unwrap();
    let committed = || {
        cache
            .delta_status(&account, "INBOX", DeltaLane::FlagsAndExpunges)
            .unwrap()
            .unwrap()
            .committed_modseq
    };
    let args = serde_json::json!({"read_source":"cache","uid_validity":9});
    let list = || mail.execute("list_emails", &args, "owner");
    assert_eq!(list().await.unwrap()["emails"].as_array().unwrap().len(), 3);

    for case in [
        "NO",
        "BAD",
        "EOF",
        "timeout",
        "cancel",
        "earlier only",
        "range budget",
        "OK",
    ] {
        let earlier = if case == "range budget" {
            // Repeated set members normalize to one UID but consume the
            // cumulative range budget before the real-time removal arrives.
            std::iter::repeat_n("8", MAX_VANISHED_RANGES)
                .collect::<Vec<_>>()
                .join(",")
        } else {
            "8".to_owned()
        };
        let mut response =
            format!("* VANISHED (EARLIER) {earlier}\r\n* 3 FETCH (UID 9 FLAGS () MODSEQ (8))\r\n");
        if case != "earlier only" {
            response.push_str("* VANISHED 7\r\n");
        }
        match case {
            "NO" | "earlier only" => response.push_str("{tag} NO failed\r\n"),
            "BAD" => response.push_str("{tag} BAD failed\r\n"),
            "OK" | "range budget" => response.push_str("{tag} OK done\r\n"),
            _ => {}
        }
        let (client, peer) = scripted_session(vec![
            (ENABLE.0, ENABLE.1.into()),
            ("EXAMINE \"INBOX\" (CONDSTORE)", "* 3 EXISTS\r\n* OK [UIDVALIDITY 9]\r\n* OK [UIDNEXT 10]\r\n* OK [HIGHESTMODSEQ 8]\r\n{tag} OK [READ-ONLY] selected\r\n".into()),
            ("UID FETCH 7:9 (UID FLAGS MODSEQ) (CHANGEDSINCE 4 VANISHED)", response),
        ], case == "EOF").await;
        let mut opened = enable(client)
            .await
            .unwrap()
            .examine_plain(&account, "INBOX")
            .await
            .unwrap();
        let (entered, wait_entered) = oneshot::channel();
        let mut entered = Some(entered);
        let mut fetch = Box::pin(opened.collect(&page.uids, round.base_modseq, || {
            let fenced = persistent.fence_qresync_deletion(&account, "INBOX", 9);
            if let Some(entered) = entered.take() {
                let _ = entered.send(());
            }
            fenced
        }));
        let result = if matches!(case, "timeout" | "cancel") {
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::select! {
                    entered = wait_entered => entered.unwrap(),
                    _ = &mut fetch => panic!("FETCH completed before the live event"),
                }
            })
            .await
            .unwrap();
            assert_eq!(list().await.unwrap_err().code, "CACHE_NEEDS_VERIFICATION");
            if case == "timeout" {
                assert!(
                    tokio::time::timeout(Duration::ZERO, &mut fetch)
                        .await
                        .is_err()
                );
            }
            drop(fetch);
            None
        } else {
            Some(fetch.await)
        };
        drop(opened);
        peer.await.unwrap();

        if case == "OK" {
            let evidence = result.unwrap().unwrap();
            let [change] = evidence.flags.as_slice() else {
                panic!("expected one FLAGS update")
            };
            let flags = [DeltaFlags {
                uid: change.uid,
                modseq: change.modseq,
                flags: &change.flags,
            }];
            cache
                .commit_qresync_page(
                    &round,
                    &page,
                    target,
                    QresyncChanges {
                        flags: &flags,
                        live: &evidence.live,
                        earlier: &evidence.earlier,
                    },
                    &|| false,
                )
                .unwrap();
            persistent.finish_qresync_recovery(&account, "INBOX", 9, false);
            let current = list().await.unwrap();
            assert_eq!(current["emails"].as_array().unwrap().len(), 1);
            assert_eq!(current["emails"][0]["uid"], 9);
            assert_eq!(current["emails"][0]["flags"], serde_json::json!([]));
            assert_eq!(committed(), "8");
        } else {
            assert!(result.is_none_or(|result| result.is_err()), "{case}");
            if case != "earlier only" {
                assert_eq!(
                    list().await.unwrap_err().code,
                    "CACHE_NEEDS_VERIFICATION",
                    "{case}"
                );
                persistent.finish_qresync_recovery(&account, "INBOX", 9, false);
            }
            let unchanged = list().await.unwrap();
            assert_eq!(unchanged["emails"].as_array().unwrap().len(), 3, "{case}");
            assert_eq!(
                unchanged["emails"][0]["flags"],
                serde_json::json!(["Seen"]),
                "{case}"
            );
            assert_eq!(committed(), "4", "{case}");
        }
    }
    mail.finish_cache(persistent).await;
}
