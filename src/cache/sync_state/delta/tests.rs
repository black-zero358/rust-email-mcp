use super::*;
use crate::cache::{AccountScope, Ref};
use std::{path::Path, sync::Arc};

const INBOX: &str = "INBOX";
const MIB: u64 = 1024 * 1024;

fn open(path: &Path, account: &AccountKey, full: bool) -> Result<Arc<Cache>, CacheError> {
    let scopes = vec![AccountScope {
        account: account.clone(),
        mailboxes: vec![INBOX.into()],
        max_bytes: 12 * MIB,
    }];
    if full {
        Cache::open_full(path, 16 * MIB, scopes, &[])
    } else {
        Cache::open(path, 16 * MIB, scopes, &[])
    }
}

fn selected(modseq: u64, checked_at_ms: i64) -> DeltaObservation {
    DeltaObservation {
        uid_validity: 9,
        uid_next: 8,
        highest_modseq: modseq,
        checked_at_ms,
    }
}

fn observation(checked_at_ms: i64) -> MailboxObservation {
    MailboxObservation {
        uid_validity: 9,
        uid_next: 8,
        checked_at_ms,
    }
}

fn reference(account: &AccountKey, uid: u32) -> Ref<'_> {
    Ref {
        account,
        mailbox: INBOX,
        uid_validity: 9,
        uid,
    }
}

fn discover(cache: &Cache, account: &AccountKey) {
    let scan = cache
        .start_sync(account, INBOX, ScanKind::History, observation(100))
        .unwrap();
    cache
        .record_sync_search(&scan.stamp, 1, 7, &[1, 3, 7], &|| false)
        .unwrap();
}

fn reconcile(cache: &Cache, account: &AccountKey, at: i64) -> ScanState {
    let fence = cache.sync_reconcile_fence(account, INBOX).unwrap();
    cache
        .start_sync_reconcile(account, INBOX, observation(at), &fence)
        .unwrap()
}

fn commit_rev1(
    cache: &Cache,
    scan: &ScanState,
    limit: usize,
    at: i64,
    cancel: &dyn Fn() -> bool,
) -> Result<ScanState, SyncError> {
    let page = cache.sync_known_page(&scan.stamp, limit).unwrap();
    let facts: Vec<_> = page
        .uids
        .iter()
        .map(|&uid| ReconciledFlags { uid, flags: &[] })
        .collect();
    cache.commit_sync_reconcile(&scan.stamp, &page, &facts, at, cancel)
}

fn seeded(cache: &Cache, account: &AccountKey) {
    discover(cache, account);
    let scan = reconcile(cache, account, 101);
    cache
        .prepare_delta_seed(&scan.stamp, selected(50, 101), &|| false)
        .unwrap();
    assert!(
        commit_rev1(cache, &scan, 200, 102, &|| false)
            .unwrap()
            .complete
    );
}

fn status(cache: &Cache, account: &AccountKey, lane: DeltaLane) -> Option<DeltaStatus> {
    cache.delta_status(account, INBOX, lane).unwrap()
}

#[test]
fn qresync_oversized_deletion_defers_to_a_fresh_rev1_audit_without_advancing() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    seeded(&cache, &account);
    let scan = reconcile(&cache, &account, 110);
    let scan = commit_rev1(&cache, &scan, 1, 111, &|| false).unwrap();
    let fence = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    let DeltaStart::Round(round) = cache
        .start_delta(
            &account,
            INBOX,
            DeltaLane::FlagsAndExpunges,
            selected(80, 112),
            &fence,
            &|| false,
        )
        .unwrap()
    else {
        panic!("QRESYNC round expected");
    };
    let page = cache.delta_page(&round, 200).unwrap();
    let observation = DeltaObservation {
        uid_next: 2_000_000,
        ..selected(80, 113)
    };
    let vanished = 1..=1_000_000;
    assert!(matches!(
        cache.commit_qresync_page(
            &round,
            &page,
            observation,
            QresyncChanges {
                live: std::slice::from_ref(&vanished),
                ..QresyncChanges::default()
            },
            &|| false
        ),
        Err(SyncError::ReconcileRequired)
    ));
    assert_eq!(
        status(&cache, &account, DeltaLane::FlagsAndExpunges)
            .unwrap()
            .committed_modseq,
        "50"
    );
    assert!(
        cache
            .cached_message(reference(&account, 1))
            .unwrap()
            .is_some()
    );
    cache
        .require_qresync_reconcile(&account, INBOX, 9, &fence, &|| false)
        .unwrap();
    assert!(matches!(
        cache.delta_page(&round, 200),
        Err(SyncError::Stale)
    ));
    assert!(matches!(
        cache.sync_known_page(&scan.stamp, 200),
        Err(SyncError::Stale)
    ));
    let restarted = reconcile(&cache, &account, 114);
    assert_ne!(restarted.stamp.scan_id, scan.stamp.scan_id);
    assert_eq!(
        cache.sync_known_page(&restarted.stamp, 200).unwrap().uids,
        [1, 3, 7]
    );
}

#[test]
fn v6_migration_rolls_back_and_keeps_v5_evidence_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let namespace = account.reference_namespace();
    {
        let cache = open(&path, &account, true).unwrap();
        cache
            .observe_folder(&account, INBOX, 9, Some(8), 100)
            .unwrap();
        cache
            .remember_headers(
                reference(&account, 1),
                true,
                b"Subject: kept\r\n\r\n",
                Some("kept"),
            )
            .unwrap();
        let databases = cache.lock().unwrap();
        databases[&namespace].connection.execute_batch(
            "DROP TRIGGER delta_epoch_change; DROP TRIGGER delta_member_insert;
             DROP TRIGGER delta_member_update; DROP TRIGGER delta_flags_insert; DROP TRIGGER delta_flags_update;
             DROP TABLE sync_delta_seed; DROP TABLE sync_delta;
             PRAGMA user_version=5;
             CREATE TRIGGER delta_flags_update AFTER UPDATE ON cache_owner BEGIN SELECT 1; END;",
        ).unwrap();
    }
    assert!(open(&path, &account, true).is_err());
    {
        let cache = open(&path, &account, false).unwrap();
        assert_eq!(
            cache.header(reference(&account, 1)).unwrap().as_deref(),
            Some(&b"Subject: kept\r\n\r\n"[..])
        );
        let databases = cache.lock().unwrap();
        let db = &databases[&namespace].connection;
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            5
        );
        assert!(!db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name IN ('sync_delta_seed','sync_delta'))",[],|row|row.get::<_,bool>(0)).unwrap());
        db.execute("DROP TRIGGER delta_flags_update", []).unwrap();
    }
    drop(open(&path, &account, true).unwrap());
    let cache = open(&path, &account, false).unwrap();
    let databases = cache.lock().unwrap();
    assert_eq!(
        databases[&namespace]
            .connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
}
