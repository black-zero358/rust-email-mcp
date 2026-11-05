use super::*;
use crate::cache::{AccountScope, Ref};
use std::{cell::Cell, path::Path, sync::Arc};

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
    if !scan.complete {
        cache
            .record_sync_search(&scan.stamp, 1, 7, &[1, 3, 7], &|| false)
            .unwrap();
    }
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

fn start(cache: &Cache, account: &AccountKey, modseq: u64, at: i64) -> DeltaStart {
    let fence = cache.sync_reconcile_fence(account, INBOX).unwrap();
    cache
        .start_delta(
            account,
            INBOX,
            DeltaLane::Flags,
            selected(modseq, at),
            &fence,
            &|| false,
        )
        .unwrap()
}

fn round(cache: &Cache, account: &AccountKey, modseq: u64, at: i64) -> DeltaRound {
    let DeltaStart::Round(round) = start(cache, account, modseq, at) else {
        panic!("delta round expected");
    };
    round
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
fn initial_watermark_waits_for_the_full_rev1_receipt_and_keeps_earliest_floor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let cache = open(&path, &account, true).unwrap();
    discover(&cache, &account);
    let scan = reconcile(&cache, &account, 101);
    cache
        .prepare_delta_seed(&scan.stamp, selected(50, 101), &|| false)
        .unwrap();
    let scan = commit_rev1(&cache, &scan, 1, 102, &|| false).unwrap();
    assert!(!scan.complete);
    assert!(status(&cache, &account, DeltaLane::Flags).is_none());
    assert!(matches!(
        start(&cache, &account, 60, 103),
        DeltaStart::NeedsBaseline
    ));
    cache.mark_clean().unwrap();
    drop(cache);
    let cache = open(&path, &account, true).unwrap();
    let resumed = reconcile(&cache, &account, 104);
    assert_eq!(resumed.stamp.scan_id, scan.stamp.scan_id);
    cache
        .prepare_delta_seed(&resumed.stamp, selected(90, 104), &|| false)
        .unwrap();
    let calls = Cell::new(0);
    assert!(matches!(
        commit_rev1(&cache, &resumed, 200, 105, &|| {
            calls.set(calls.get() + 1);
            calls.get() == 2
        }),
        Err(SyncError::Cancelled)
    ));
    assert!(status(&cache, &account, DeltaLane::Flags).is_none());
    assert!(
        commit_rev1(&cache, &resumed, 200, 105, &|| false)
            .unwrap()
            .complete
    );
    for lane in [DeltaLane::Flags, DeltaLane::FlagsAndExpunges] {
        assert_eq!(
            status(&cache, &account, lane).unwrap().committed_modseq,
            "50"
        );
    }
    let periodic = reconcile(&cache, &account, 110);
    cache
        .prepare_delta_seed(&periodic.stamp, selected(90, 110), &|| false)
        .unwrap();
    assert!(
        commit_rev1(&cache, &periodic, 200, 111, &|| false)
            .unwrap()
            .complete
    );
    assert_eq!(
        status(&cache, &account, DeltaLane::Flags)
            .unwrap()
            .committed_modseq,
        "50"
    );
}

#[test]
fn pages_commit_sparse_positive_flags_but_advance_only_the_frozen_flags_lane() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let cache = open(&path, &account, true).unwrap();
    seeded(&cache, &account);
    let pass = round(&cache, &account, 80, 110);
    assert!(matches!(
        cache.delta_page(&pass, 201),
        Err(SyncError::Invalid)
    ));
    let page = cache.delta_page(&pass, 1).unwrap();
    assert_eq!(page.uids, [1]);
    let changed = [DeltaFlags {
        uid: 1,
        modseq: 120,
        flags: &["\\Seen".into()],
    }];
    let next = cache
        .commit_delta_page(&pass, &page, selected(80, 111), &changed, &|| false)
        .unwrap()
        .unwrap();
    assert!(matches!(cache.delta_page(&pass, 1), Err(SyncError::Stale)));
    assert_eq!(
        status(&cache, &account, DeltaLane::Flags)
            .unwrap()
            .committed_modseq,
        "50"
    );
    cache.mark_clean().unwrap();
    drop(cache);
    let cache = open(&path, &account, true).unwrap();
    let resumed = round(&cache, &account, 130, 120);
    assert_eq!(resumed, next);
    assert_eq!(resumed.target_modseq, 80);
    let page = cache.delta_page(&resumed, 200).unwrap();
    assert_eq!(page.uids, [3, 7]);
    assert!(page.complete);
    assert!(matches!(
        cache.commit_delta_page(&resumed, &page, selected(79, 120), &[], &|| false),
        Err(SyncError::Invalid)
    ));
    assert!(
        cache
            .commit_delta_page(&resumed, &page, selected(130, 120), &[], &|| false)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        status(&cache, &account, DeltaLane::Flags)
            .unwrap()
            .committed_modseq,
        "80"
    );
    assert_eq!(
        status(&cache, &account, DeltaLane::FlagsAndExpunges)
            .unwrap()
            .committed_modseq,
        "50"
    );
    let facts = |uid| {
        cache
            .cached_message(reference(&account, uid))
            .unwrap()
            .unwrap()
            .flags
            .unwrap()
    };
    assert_eq!(facts(1), ["\\Seen"]);
    assert!(facts(3).is_empty() && facts(7).is_empty());
    assert_eq!(round(&cache, &account, 130, 121).base_modseq, 80);
}

#[test]
fn delta_cas_rejects_actual_foreground_change_and_mutation_but_accepts_repeat_observation() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    seeded(&cache, &account);
    let pass = round(&cache, &account, 80, 110);
    let page = cache.delta_page(&pass, 200).unwrap();
    let put = |flags: &[String]| {
        cache
            .remember_listing(
                reference(&account, 1),
                ListingEvidence {
                    flags: Some(flags),
                    internal_date: None,
                    size: None,
                    observed_at_ms: 110,
                },
            )
            .unwrap()
    };
    put(&[]);
    assert_eq!(cache.delta_page(&pass, 200).unwrap(), page);
    cache
        .observe_folder(&account, INBOX, 9, Some(10), 110)
        .unwrap();
    cache
        .remember_listing(
            reference(&account, 8),
            ListingEvidence {
                flags: Some(&["\\Seen".into()]),
                internal_date: None,
                size: None,
                observed_at_ms: 110,
            },
        )
        .unwrap();
    assert_eq!(cache.delta_page(&pass, 200).unwrap(), page);
    put(&["\\Flagged".into()]);
    assert!(matches!(
        cache.commit_delta_page(&pass, &page, selected(80, 111), &[], &|| false),
        Err(SyncError::Stale)
    ));
    assert_eq!(
        status(&cache, &account, DeltaLane::Flags)
            .unwrap()
            .committed_modseq,
        "50"
    );
    let pass = round(&cache, &account, 80, 112);
    let page = cache.delta_page(&pass, 200).unwrap();
    let calls = Cell::new(0);
    let facts = [DeltaFlags {
        uid: 1,
        modseq: 80,
        flags: &[],
    }];
    assert!(matches!(
        cache.commit_delta_page(&pass, &page, selected(80, 113), &facts, &|| {
            calls.set(calls.get() + 1);
            calls.get() == 2
        }),
        Err(SyncError::Cancelled)
    ));
    let retained = cache
        .cached_message(reference(&account, 1))
        .unwrap()
        .unwrap();
    assert_eq!(retained.flags.unwrap(), ["\\Flagged"]);
    let ticket = cache.begin_dirty(&account, &[INBOX]).unwrap();
    assert!(matches!(
        cache.delta_page(&pass, 200),
        Err(SyncError::ClaimActive)
    ));
    cache.finish_dirty(ticket).unwrap();
    assert!(matches!(
        cache.delta_page(&pass, 200),
        Err(SyncError::Stale)
    ));
    assert!(status(&cache, &account, DeltaLane::Flags).is_none());
    assert!(matches!(
        start(&cache, &account, 90, 114),
        DeltaStart::NeedsBaseline
    ));
}

#[test]
fn explicit_no_modseq_rollback_and_generation_reset_remove_both_lanes() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    seeded(&cache, &account);
    let fence = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    assert!(matches!(
        cache.invalidate_delta(&account, INBOX, 9, &fence, &|| true),
        Err(SyncError::Cancelled)
    ));
    assert!(status(&cache, &account, DeltaLane::FlagsAndExpunges).is_some());
    assert!(matches!(
        start(&cache, &account, 40, 110),
        DeltaStart::Rollback
    ));
    assert!(status(&cache, &account, DeltaLane::FlagsAndExpunges).is_none());
    seeded(&cache, &account);
    cache
        .invalidate_delta(&account, INBOX, 9, &fence, &|| false)
        .unwrap();
    assert!(matches!(
        start(&cache, &account, 80, 110),
        DeltaStart::NeedsBaseline
    ));
    seeded(&cache, &account);
    let pass = round(&cache, &account, 80, 111);
    cache.mark_restored().unwrap();
    assert!(status(&cache, &account, DeltaLane::FlagsAndExpunges).is_none());
    assert!(matches!(cache.delta_page(&pass, 1), Err(SyncError::Stale)));
    seeded(&cache, &account);
    let pass = round(&cache, &account, 80, 111);
    cache
        .observe_folder(&account, INBOX, 10, Some(1), 112)
        .unwrap();
    assert!(status(&cache, &account, DeltaLane::Flags).is_none());
    assert!(matches!(cache.delta_page(&pass, 1), Err(SyncError::Stale)));
}

#[test]
fn first_modseq_seen_after_a_rev1_page_cannot_seed_the_completed_range() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    discover(&cache, &account);
    let scan = reconcile(&cache, &account, 101);
    let scan = commit_rev1(&cache, &scan, 1, 102, &|| false).unwrap();
    cache
        .prepare_delta_seed(&scan.stamp, selected(90, 103), &|| false)
        .unwrap();
    assert!(
        commit_rev1(&cache, &scan, 200, 104, &|| false)
            .unwrap()
            .complete
    );
    assert!(status(&cache, &account, DeltaLane::Flags).is_none());
}

#[test]
fn a_lower_selection_between_rev1_pages_durably_disqualifies_the_seed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let cache = open(&path, &account, true).unwrap();
    discover(&cache, &account);
    let scan = reconcile(&cache, &account, 101);
    cache
        .prepare_delta_seed(&scan.stamp, selected(100, 101), &|| false)
        .unwrap();
    assert_eq!(
        cache.pending_delta_floor(&account, INBOX).unwrap(),
        Some((9, 100))
    );
    let scan = commit_rev1(&cache, &scan, 1, 102, &|| false).unwrap();
    let calls = Cell::new(0);
    assert!(matches!(
        cache.prepare_delta_seed(&scan.stamp, selected(90, 103), &|| {
            calls.set(calls.get() + 1);
            calls.get() == 2
        }),
        Err(SyncError::Cancelled)
    ));
    assert_eq!(
        cache.pending_delta_floor(&account, INBOX).unwrap(),
        Some((9, 100))
    );
    cache
        .prepare_delta_seed(&scan.stamp, selected(90, 103), &|| false)
        .unwrap();
    assert!(
        cache
            .pending_delta_floor(&account, INBOX)
            .unwrap()
            .is_none()
    );
    cache.mark_clean().unwrap();
    drop(cache);
    let cache = open(&path, &account, true).unwrap();
    let scan = reconcile(&cache, &account, 104);
    cache
        .prepare_delta_seed(&scan.stamp, selected(110, 104), &|| false)
        .unwrap();
    assert!(
        commit_rev1(&cache, &scan, 200, 105, &|| false)
            .unwrap()
            .complete
    );
    assert!(status(&cache, &account, DeltaLane::Flags).is_none());
    let next = reconcile(&cache, &account, 110);
    cache
        .prepare_delta_seed(&next.stamp, selected(120, 110), &|| false)
        .unwrap();
    assert!(
        commit_rev1(&cache, &next, 200, 111, &|| false)
            .unwrap()
            .complete
    );
    assert_eq!(
        status(&cache, &account, DeltaLane::Flags)
            .unwrap()
            .committed_modseq,
        "120"
    );
}

#[test]
fn a_plain_page_discards_only_its_pending_seed_and_preserves_both_committed_lanes() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    seeded(&cache, &account);
    let newer = MailboxObservation {
        uid_next: 10,
        ..observation(110)
    };
    let tail = cache
        .start_sync(&account, INBOX, ScanKind::Tail, newer)
        .unwrap();
    cache
        .record_sync_search(&tail.stamp, 8, 9, &[9], &|| false)
        .unwrap();
    let fence = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    let scan = cache
        .start_sync_reconcile(&account, INBOX, newer, &fence)
        .unwrap();
    let selected = DeltaObservation {
        uid_next: 10,
        ..selected(100, 110)
    };
    cache
        .prepare_delta_seed(&scan.stamp, selected, &|| false)
        .unwrap();
    let scan = commit_rev1(&cache, &scan, 1, 111, &|| false).unwrap();
    let calls = Cell::new(0);
    assert!(matches!(
        cache.discard_delta_seed(&scan.stamp, &|| {
            calls.set(calls.get() + 1);
            calls.get() == 2
        }),
        Err(SyncError::Cancelled)
    ));
    assert_eq!(
        cache.pending_delta_floor(&account, INBOX).unwrap(),
        Some((9, 100))
    );
    cache.discard_delta_seed(&scan.stamp, &|| false).unwrap();
    cache.discard_delta_seed(&scan.stamp, &|| false).unwrap();
    assert!(
        cache
            .pending_delta_floor(&account, INBOX)
            .unwrap()
            .is_none()
    );
    cache
        .prepare_delta_seed(
            &scan.stamp,
            DeltaObservation {
                highest_modseq: 120,
                checked_at_ms: 112,
                ..selected
            },
            &|| false,
        )
        .unwrap();
    assert!(
        commit_rev1(&cache, &scan, 200, 113, &|| false)
            .unwrap()
            .complete
    );
    for lane in [DeltaLane::Flags, DeltaLane::FlagsAndExpunges] {
        let committed = status(&cache, &account, lane).unwrap();
        assert_eq!(
            (committed.high_uid, committed.committed_modseq.as_str()),
            (7, "50")
        );
    }
}

#[test]
fn reopening_with_a_narrower_scope_rejects_old_round_and_reconciliation_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let cache = open(&path, &account, true).unwrap();
    seeded(&cache, &account);
    let round = round(&cache, &account, 80, 110);
    let page = cache.delta_page(&round, 200).unwrap();
    let scan = reconcile(&cache, &account, 111);
    let known = cache.sync_known_page(&scan.stamp, 200).unwrap();
    let fence = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    cache.mark_clean().unwrap();
    drop(cache);

    let cache = Cache::open_full(
        &path,
        16 * MIB,
        vec![AccountScope {
            account: account.clone(),
            mailboxes: vec!["Archive".into()],
            max_bytes: 12 * MIB,
        }],
        &[],
    )
    .unwrap();
    assert!(matches!(
        cache.delta_page(&round, 200),
        Err(SyncError::Cache(CacheError::OutOfScope))
    ));
    assert!(matches!(
        cache.commit_delta_page(&round, &page, selected(80, 112), &[], &|| false),
        Err(SyncError::Cache(CacheError::OutOfScope))
    ));
    assert!(matches!(
        cache.sync_known_page(&scan.stamp, 200),
        Err(SyncError::Cache(CacheError::OutOfScope))
    ));
    assert!(matches!(
        cache.prepare_delta_seed(&scan.stamp, selected(80, 111), &|| false),
        Err(SyncError::Cache(CacheError::OutOfScope))
    ));
    assert!(matches!(
        cache.discard_delta_seed(&scan.stamp, &|| false),
        Err(SyncError::Cache(CacheError::OutOfScope))
    ));
    assert!(matches!(
        cache.commit_sync_reconcile(&scan.stamp, &known, &[], 112, &|| false),
        Err(SyncError::Cache(CacheError::OutOfScope))
    ));
    assert!(matches!(
        cache.invalidate_delta(&account, INBOX, 9, &fence, &|| false),
        Err(SyncError::Cache(CacheError::OutOfScope))
    ));
}

#[test]
fn lower_uncommitted_target_preserves_the_base_for_a_rev1_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    seeded(&cache, &account);
    let pass = round(&cache, &account, 80, 110);
    let page = cache.delta_page(&pass, 1).unwrap();
    let next = cache
        .commit_delta_page(&pass, &page, selected(80, 111), &[], &|| false)
        .unwrap()
        .unwrap();
    assert!(matches!(
        start(&cache, &account, 75, 112),
        DeltaStart::NeedsBaseline
    ));
    assert!(matches!(cache.delta_page(&next, 1), Err(SyncError::Stale)));
    let retained = status(&cache, &account, DeltaLane::Flags).unwrap();
    assert_eq!(retained.committed_modseq, "50");
    assert!(retained.target_modseq.is_none());
}

#[test]
fn a_completed_rev1_seed_extends_the_frozen_range_to_tail_messages() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    seeded(&cache, &account);
    let newer = MailboxObservation {
        uid_next: 10,
        ..observation(110)
    };
    let tail = cache
        .start_sync(&account, INBOX, ScanKind::Tail, newer)
        .unwrap();
    cache
        .record_sync_search(&tail.stamp, 8, 9, &[9], &|| false)
        .unwrap();
    let fence = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    let scan = cache
        .start_sync_reconcile(&account, INBOX, newer, &fence)
        .unwrap();
    let seen = DeltaObservation {
        uid_next: 10,
        ..selected(90, 110)
    };
    cache
        .prepare_delta_seed(&scan.stamp, seen, &|| false)
        .unwrap();
    assert_eq!(
        status(&cache, &account, DeltaLane::Flags).unwrap().high_uid,
        7
    );
    assert!(
        commit_rev1(&cache, &scan, 200, 111, &|| false)
            .unwrap()
            .complete
    );
    let fence = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    let newer = DeltaObservation {
        highest_modseq: 100,
        checked_at_ms: 112,
        ..seen
    };
    let DeltaStart::Round(round) = cache
        .start_delta(&account, INBOX, DeltaLane::Flags, newer, &fence, &|| false)
        .unwrap()
    else {
        panic!("delta round expected");
    };
    assert_eq!((round.base_modseq, round.high_uid), (90, 9));
    assert_eq!(cache.delta_page(&round, 200).unwrap().uids, [1, 3, 7, 9]);
}

#[test]
fn v6_migration_rolls_back_and_keeps_v5_evidence_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let namespace = account.reference_namespace();
    {
        let cache = open(&path, &account, true).unwrap();
        seeded(&cache, &account);
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
