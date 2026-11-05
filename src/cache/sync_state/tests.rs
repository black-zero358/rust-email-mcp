use super::*;
use crate::cache::{
    Access, AccountScope, ArtifactKey, CacheError, Omission, Ref, ThreadId, ThreadRole, disk,
};
use std::{cell::Cell, io::Write, path::Path, sync::Arc};

const INBOX: &str = "INBOX";
const MIB: u64 = 1024 * 1024;

fn open(path: &Path, account: &AccountKey, full: bool) -> Result<Arc<Cache>, CacheError> {
    let scopes = vec![AccountScope {
        account: account.clone(),
        mailboxes: vec![INBOX.into()],
        max_bytes: 32 * MIB,
    }];
    if full {
        Cache::open_full(path, 40 * MIB, scopes, &[])
    } else {
        Cache::open(path, 40 * MIB, scopes, &[])
    }
}

fn observation(uid_next: u32) -> MailboxObservation {
    MailboxObservation {
        uid_validity: 9,
        uid_next,
        checked_at_ms: 100,
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

#[test]
fn discovery_resumes_sparse_history_and_advances_the_tail_separately() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let frozen = observation(u32::MAX - 1);
    let scan_id;
    {
        let cache = open(&path, &account, true).unwrap();
        let first = cache
            .start_sync(&account, INBOX, ScanKind::History, frozen)
            .unwrap();
        assert_eq!(first.search_window(), Some((1, 200)));
        let calls = Cell::new(0);
        assert!(matches!(
            cache.record_sync_search(&first.stamp, 1, 200, &[7], &|| {
                calls.set(calls.get() + 1);
                calls.get() == 2
            }),
            Err(SyncError::Cancelled)
        ));
        assert!(
            cache
                .sync_content_page(&first.stamp, 100, 1)
                .unwrap()
                .is_empty()
        );
        let saved = cache
            .record_sync_search(&first.stamp, 1, 200, &[7, 200], &|| false)
            .unwrap();
        scan_id = saved.stamp.scan_id;
        assert_eq!(saved.next_low, Some(201));
        assert!(matches!(
            cache.record_sync_search(&first.stamp, 1, 200, &[], &|| false),
            Err(SyncError::Stale)
        ));
        assert!(matches!(
            cache.record_sync_search(&saved.stamp, 201, 1000, &[201; 201], &|| false),
            Err(SyncError::Invalid)
        ));
        assert_eq!(saved.search_window(), Some((201, 600)));
        assert!(matches!(
            cache.record_sync_search(&saved.stamp, 201, 450, &[], &|| false),
            Err(SyncError::Invalid)
        ));
        let narrowed = cache
            .record_sync_search(&saved.stamp, 201, 400, &[], &|| false)
            .unwrap();
        assert_eq!(narrowed.width_hint, 200);
        assert_eq!(narrowed.next_low, Some(401));
        cache.mark_clean().unwrap();
    }
    let cache = open(&path, &account, true).unwrap();
    let later = observation(u32::MAX);
    let mut state = cache
        .start_sync(&account, INBOX, ScanKind::History, later)
        .unwrap();
    assert_eq!(state.stamp.scan_id, scan_id);
    assert_eq!(state.stamp.high_uid, u32::MAX - 2);
    assert_eq!(state.next_low, Some(401));
    let mut commands = 0;
    while let Some((low, high)) = state.search_window() {
        let hits = if high == state.stamp.high_uid {
            vec![high]
        } else {
            vec![]
        };
        state = cache
            .record_sync_search(&state.stamp, low, high, &hits, &|| false)
            .unwrap();
        commands += 1;
    }
    assert!(state.complete && commands < 30);
    let tail = cache
        .start_sync(&account, INBOX, ScanKind::Tail, later)
        .unwrap();
    let next = u32::MAX - 1;
    assert_eq!(tail.search_window(), Some((next, next)));
    let tail = cache
        .record_sync_search(&tail.stamp, next, next, &[next], &|| false)
        .unwrap();
    assert!(tail.complete);
    assert_eq!(
        cache
            .sync_content_page(&tail.stamp, 100, 200)
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn dirty_reconcile_needs_a_pre_examine_fence_and_atomic_current_flags() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    let observed = observation(11);
    let history = cache
        .start_sync(&account, INBOX, ScanKind::History, observed)
        .unwrap();
    cache
        .record_sync_search(&history.stamp, 1, 10, &[7, 8, 9], &|| false)
        .unwrap();
    let obsolete = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    let ticket = cache.begin_dirty(&account, &[INBOX]).unwrap();
    assert!(matches!(
        cache.sync_reconcile_fence(&account, INBOX),
        Err(SyncError::ClaimActive)
    ));
    cache.finish_dirty(ticket).unwrap();
    assert!(matches!(
        cache.start_sync_reconcile(&account, INBOX, observed, &obsolete),
        Err(SyncError::Stale)
    ));
    let fence = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    let scan = cache
        .start_sync_reconcile(&account, INBOX, observed, &fence)
        .unwrap();
    let page = cache.sync_known_page(&scan.stamp, 1).unwrap();
    assert_eq!(page.uids, [7]);
    assert!(!page.complete);
    cache
        .tombstone_observed(reference(&account, 8), 11)
        .unwrap();
    assert!(matches!(
        cache.commit_sync_reconcile(&scan.stamp, &page, &[], 120, &|| false),
        Err(SyncError::Stale)
    ));
    let scan = cache
        .start_sync_reconcile(&account, INBOX, observed, &fence)
        .unwrap();
    let page = cache.sync_known_page(&scan.stamp, 200).unwrap();
    assert_eq!(page.uids, [7, 9]);
    assert!(page.complete);
    cache
        .observe_folder(&account, INBOX, 9, Some(11), 110)
        .unwrap();
    cache
        .remember_headers(reference(&account, 11), true, b"later arrival", None)
        .unwrap();
    let calls = Cell::new(0);
    let flags = [ReconciledFlags { uid: 7, flags: &[] }];
    assert!(matches!(
        cache.commit_sync_reconcile(&scan.stamp, &page, &flags, 120, &|| {
            calls.set(calls.get() + 1);
            calls.get() == 2
        }),
        Err(SyncError::Cancelled)
    ));
    assert!(cache.offline_scope_allowed(&account, INBOX, 9).is_err());
    let result = cache
        .commit_sync_reconcile(&scan.stamp, &page, &flags, 120, &|| false)
        .unwrap();
    assert!(result.complete);
    assert_eq!(result.completed_at_ms, Some(120));
    let status = cache.sync_scope_status(&account, INBOX).unwrap();
    let reconcile = status.reconciliation.unwrap();
    assert!(reconcile.complete);
    assert_eq!(reconcile.completed_at_ms, Some(120));
    assert!(status.history_coverage.unwrap().existence_flags_verified);
    assert!(!status.current_coverage.unwrap().existence_flags_verified);
    let row = cache
        .cached_message(reference(&account, 7))
        .unwrap()
        .unwrap();
    assert_eq!(row.flags, Some(Vec::new()));
    assert_eq!(row.flags_checked_at_ms, Some(120));
    assert!(
        cache
            .cached_message(reference(&account, 9))
            .unwrap()
            .is_none()
    );
    assert!(
        cache
            .cached_message(reference(&account, 11))
            .unwrap()
            .is_some()
    );
    let ticket = cache.begin_dirty(&account, &[INBOX]).unwrap();
    let status = cache.sync_scope_status(&account, INBOX).unwrap();
    assert!(status.dirty);
    assert!(!status.reconciliation.unwrap().complete);
    assert!(!status.history_coverage.unwrap().existence_flags_verified);
    cache.finish_dirty(ticket).unwrap();
    cache.mark_restored().unwrap();
    assert!(cache.sync_status(&account, INBOX).unwrap().is_empty());
}

#[test]
fn content_receipts_retries_and_quota_do_not_rewind_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    let scan = cache
        .start_sync(&account, INBOX, ScanKind::History, observation(8))
        .unwrap();
    let scan = cache
        .record_sync_search(&scan.stamp, 1, 7, &[7], &|| false)
        .unwrap();
    let next = |now| {
        cache
            .sync_content_page(&scan.stamp, now, 1)
            .unwrap()
            .pop()
            .unwrap()
    };
    let facts = next(100);
    assert_eq!(facts.layer, ContentLayer::Facts);
    cache
        .commit_sync_facts(
            &facts,
            SyncFacts {
                header: "Subject: test\r\nMessage-ID: <主题@例子.test>\r\n\r\n".as_bytes(),
                complete: true,
                index_text: Some("test"),
                thread_ids: Some(&[ThreadId {
                    role: ThreadRole::MessageId,
                    id: "<主题@例子.test>",
                }]),
                flags: &[],
                internal_date: "1-Jan-2025",
                size: 4,
            },
            100,
            &|| false,
        )
        .unwrap();
    let scope = cache.freeze_scope(&account, INBOX).unwrap();
    assert_eq!(
        cache
            .thread_uids(
                &account,
                &scope,
                ThreadRole::MessageId,
                "<主题@例子.test>",
                1..=7
            )
            .unwrap(),
        [7]
    );
    assert!(cache.thread_header(&account, &scope, 7).unwrap().is_some());
    let coverage = cache
        .sync_scope_status(&account, INBOX)
        .unwrap()
        .current_coverage
        .unwrap();
    assert_eq!(coverage.exact_thread_index, 1);
    assert!(!coverage.exact_thread_index_complete);
    let fence = cache.sync_reconcile_fence(&account, INBOX).unwrap();
    let reconciliation = cache
        .start_sync_reconcile(&account, INBOX, observation(8), &fence)
        .unwrap();
    let page = cache.sync_known_page(&reconciliation.stamp, 1).unwrap();
    cache
        .commit_sync_reconcile(
            &reconciliation.stamp,
            &page,
            &[ReconciledFlags { uid: 7, flags: &[] }],
            100,
            &|| false,
        )
        .unwrap();
    let manifest = next(100);
    assert_eq!(manifest.layer, ContentLayer::Manifest);
    cache
        .commit_sync_content(&manifest, ContentOutcome::NeedsRaw, &|| false)
        .unwrap();
    let raw = next(100);
    assert_eq!(raw.layer, ContentLayer::Raw);
    cache
        .commit_sync_content(&raw, ContentOutcome::QuotaBlocked, &|| false)
        .unwrap();
    let coverage = cache
        .sync_scope_status(&account, INBOX)
        .unwrap()
        .current_coverage
        .unwrap();
    assert!(coverage.exact_thread_index_complete);
    assert_eq!(coverage.raw.counts.missing, 1);
    assert_eq!(coverage.work.quota_blocked, 1);
    assert!(
        coverage
            .gaps
            .iter()
            .any(|gap| (gap.layer, gap.status, gap.reason, gap.count)
                == ("raw", "quota_blocked", "quota", 1))
    );
    assert!(
        cache
            .sync_content_page(&scan.stamp, 100, 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(cache.sync_progress(&scan.stamp).unwrap().quota_blocked, 1);
    assert_eq!(cache.resume_sync_quota(&scan.stamp, 1).unwrap(), 1);
    let raw = next(100);
    cache
        .commit_sync_content(
            &raw,
            ContentOutcome::RetryAt {
                reason: FailureReason::Network,
                not_before_ms: 120,
            },
            &|| false,
        )
        .unwrap();
    assert!(
        cache
            .sync_content_page(&scan.stamp, 119, 1)
            .unwrap()
            .is_empty()
    );
    let coverage = cache
        .sync_scope_status(&account, INBOX)
        .unwrap()
        .history_coverage
        .unwrap();
    assert_eq!(coverage.work.retrying, 1);
    assert_eq!(coverage.work.next_due_ms, Some(120));
    let raw = next(120);
    let mut stage = cache
        .begin_blob(reference(&account, 7), ArtifactKey::Raw, Some(4), 4)
        .unwrap();
    stage.write_all(b"mail").unwrap();
    let blob = stage.publish(&|| false).unwrap();
    cache
        .commit_sync_content(&raw, ContentOutcome::PresentBlob(blob.id), &|| false)
        .unwrap();
    let manifest = next(120);
    assert_eq!(manifest.layer, ContentLayer::Manifest);
    cache
        .commit_sync_content(
            &manifest,
            ContentOutcome::Terminal(Omission::UnsupportedContent),
            &|| false,
        )
        .unwrap();
    assert_eq!(next(120).layer, ContentLayer::FullBody);
    assert!(cache.sync_status(&account, INBOX).unwrap()[0].complete);
    let path = cache
        .disk
        .blob_path(
            &disk::namespace_name(&account.reference_namespace()),
            Uuid::from_bytes(blob.id.bytes()),
            false,
        )
        .unwrap();
    std::fs::write(path, b"bad!").unwrap();
    assert!(matches!(
        cache.open_blob(
            reference(&account, 7),
            ArtifactKey::Raw,
            Access::Offline,
            Some(blob.id)
        ),
        Err(CacheError::Corrupt)
    ));
    assert_eq!(next(120).layer, ContentLayer::Raw);
    let mut stage = cache
        .begin_blob(reference(&account, 7), ArtifactKey::Raw, Some(4), 4)
        .unwrap();
    stage.write_all(b"mail").unwrap();
    let raw = stage.publish(&|| false).unwrap();
    cache
        .commit_sync_content(&next(120), ContentOutcome::PresentBlob(raw.id), &|| false)
        .unwrap();

    let body_key = ArtifactKey::FullBody {
        mime_version: 1,
        html_version: 1,
    };
    let publish_body = || {
        let mut stage = cache
            .begin_blob(reference(&account, 7), body_key, Some(4), 4)
            .unwrap();
        stage.write_all(b"body").unwrap();
        stage.publish_body(b"leaf metadata", &|| false).unwrap()
    };
    let body = publish_body();
    cache
        .commit_sync_content(&next(120), ContentOutcome::PresentBlob(body.id), &|| false)
        .unwrap();
    let old_index = next(120);
    assert_eq!(old_index.layer, ContentLayer::BodyIndex);
    cache
        .remember_full_body_index(reference(&account, 7), body_key, body.id, "body", &|| false)
        .unwrap();
    cache
        .commit_sync_content(&old_index, ContentOutcome::PresentBlob(body.id), &|| false)
        .unwrap();
    let coverage = cache
        .sync_scope_status(&account, INBOX)
        .unwrap()
        .current_coverage
        .unwrap();
    assert!(coverage.body_index_complete && coverage.full_body.materialization_complete);
    assert!(!coverage.attachments.available_complete);
    assert!(
        cache
            .sync_content_page(&scan.stamp, 120, 2)
            .unwrap()
            .is_empty()
    );

    let path = cache
        .disk
        .blob_path(
            &disk::namespace_name(&account.reference_namespace()),
            Uuid::from_bytes(body.id.bytes()),
            false,
        )
        .unwrap();
    std::fs::write(path, b"bad!").unwrap();
    assert!(matches!(
        cache.open_blob(
            reference(&account, 7),
            body_key,
            Access::Offline,
            Some(body.id)
        ),
        Err(CacheError::Corrupt)
    ));
    let jobs = cache.sync_content_page(&scan.stamp, 120, 2).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].layer, ContentLayer::FullBody);
    let repaired = publish_body();
    cache
        .commit_sync_content(&jobs[0], ContentOutcome::PresentBlob(repaired.id), &|| {
            false
        })
        .unwrap();
    assert_eq!(next(120).layer, ContentLayer::BodyIndex);
    assert!(matches!(
        cache.commit_sync_content(&old_index, ContentOutcome::PresentBlob(body.id), &|| false),
        Err(SyncError::Stale)
    ));
    let candidate = cache
        .sync_reclaim_candidates(&account, INBOX, None, 1)
        .unwrap()
        .candidates
        .pop()
        .unwrap();
    let held = cache
        .open_blob(
            reference(&account, 7),
            body_key,
            Access::LiveVerified,
            Some(repaired.id),
        )
        .unwrap();
    assert!(
        !cache
            .evict_sync_candidate(reference(&account, 7), &candidate)
            .unwrap()
    );
    drop(held);
    assert!(
        cache
            .evict_sync_candidate(reference(&account, 7), &candidate)
            .unwrap()
    );
    let coverage = cache
        .sync_scope_status(&account, INBOX)
        .unwrap()
        .current_coverage
        .unwrap();
    assert!(coverage.raw.materialization_complete);
    assert!(coverage.full_body.available_complete);
    assert!(!coverage.full_body.materialization_complete);
    assert!(!coverage.body_index_complete);
    assert_eq!(coverage.work.quota_blocked, 1);
    assert!(
        cache
            .sync_content_page(&scan.stamp, 120, 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        cache
            .resume_sync_quota_after_reclaim(&scan.stamp, 1)
            .unwrap(),
        0
    );
    assert_eq!(cache.resume_sync_quota(&scan.stamp, 1).unwrap(), 1);
    assert_eq!(next(120).layer, ContentLayer::FullBody);
    let dirty = cache.begin_dirty(&account, &[INBOX]).unwrap();
    let coverage = cache
        .sync_scope_status(&account, INBOX)
        .unwrap()
        .current_coverage
        .unwrap();
    assert_eq!(coverage.raw.counts.materialized, 1);
    assert!(!coverage.raw.materialization_complete);
    assert!(!coverage.exact_thread_index_complete);
    cache.finish_dirty(dirty).unwrap();
}

#[test]
fn retired_work_collection_is_bounded_and_preserves_current_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    let first = cache
        .start_sync(&account, INBOX, ScanKind::History, observation(10))
        .unwrap();
    let first = cache
        .record_sync_search(&first.stamp, 1, 9, &[7, 8, 9], &|| false)
        .unwrap();
    assert_eq!(cache.collect_retired_sync_work(&account, 2).unwrap(), 0);
    cache
        .remember_headers(
            reference(&account, 7),
            true,
            b"Subject: old\r\n\r\n",
            Some("old"),
        )
        .unwrap();
    let current = cache
        .start_sync(
            &account,
            INBOX,
            ScanKind::History,
            MailboxObservation {
                uid_validity: 10,
                ..observation(8)
            },
        )
        .unwrap();
    let current = cache
        .record_sync_search(&current.stamp, 1, 7, &[7], &|| false)
        .unwrap();
    assert!(matches!(
        cache.sync_content_page(&first.stamp, 100, 1),
        Err(SyncError::Stale)
    ));
    assert_eq!(cache.collect_retired_sync_work(&account, 2).unwrap(), 2);
    assert_eq!(cache.collect_retired_sync_work(&account, 2).unwrap(), 1);
    assert_eq!(cache.collect_retired_sync_work(&account, 2).unwrap(), 0);
    let mut page = cache.collect_sync_garbage(&account, None).unwrap();
    let mut removed = page.removed;
    while let Some(next) = page.next {
        page = cache.collect_sync_garbage(&account, Some(next)).unwrap();
        removed += page.removed;
    }
    assert_eq!(removed, 3);
    let databases = cache.lock().unwrap();
    let db = &databases[&account.reference_namespace()].connection;
    assert_eq!(
        db.query_row("SELECT count(*) FROM message_index", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM retired_epochs", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    drop(databases);
    let jobs = cache.sync_content_page(&current.stamp, 100, 2).unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].matches_scope(&account.reference_namespace(), INBOX, 10));
    assert_eq!(jobs[0].uid, 7);
}

#[test]
fn coverage_keeps_partial_headers_and_exact_identity_separate_across_watermarks() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    let history = cache
        .start_sync(&account, INBOX, ScanKind::History, observation(8))
        .unwrap();
    let history = cache
        .record_sync_search(&history.stamp, 1, 7, &[1, 3, 7], &|| false)
        .unwrap();
    cache
        .remember_headers(
            reference(&account, 1),
            true,
            b"Subject: complete\r\n\r\n",
            Some("complete"),
        )
        .unwrap();
    cache
        .remember_headers(reference(&account, 3), false, b"Subject: truncated", None)
        .unwrap();
    let tail = cache
        .start_sync(&account, INBOX, ScanKind::Tail, observation(10))
        .unwrap();
    let status = cache.sync_scope_status(&account, INBOX).unwrap();
    let historical = status.history_coverage.unwrap();
    assert_eq!(historical.high_uid, 7);
    assert!(historical.discovery_complete);
    assert_eq!(
        (
            historical.complete_headers,
            historical.partial_headers,
            historical.missing_headers
        ),
        (1, 1, 1)
    );
    assert_eq!(historical.indexed_headers, 1);
    assert_eq!(historical.exact_thread_index, 0);
    assert!(!historical.exact_thread_index_complete);
    assert!(!historical.attachments.available_complete);
    assert!(!status.current_coverage.unwrap().discovery_complete);
    assert_eq!(
        status.history.unwrap().scan_id,
        history.stamp.scan_id.to_string()
    );
    cache
        .record_sync_search(&tail.stamp, 8, 9, &[9], &|| false)
        .unwrap();
    let status = cache.sync_scope_status(&account, INBOX).unwrap();
    assert!(status.tail.unwrap().complete);
    assert_eq!(status.history_coverage.unwrap().known_messages, 3);
    let current = status.current_coverage.unwrap();
    assert!(current.discovery_complete);
    assert!(!current.existence_flags_verified);
    assert_eq!(current.known_messages, 4);
}

#[test]
fn reclaim_pages_derived_candidates_and_keeps_raw_files() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    let history = cache
        .start_sync(&account, INBOX, ScanKind::History, observation(8))
        .unwrap();
    cache
        .record_sync_search(&history.stamp, 1, 7, &[7], &|| false)
        .unwrap();
    let message = reference(&account, 7);
    let publish = |reference, kind, bytes: &[u8]| {
        let len = bytes.len() as u64;
        let mut stage = cache.begin_blob(reference, kind, Some(len), len).unwrap();
        stage.write_all(bytes).unwrap();
        stage.publish(&|| false).unwrap()
    };
    let raw = publish(message, ArtifactKey::Raw, b"raw");
    let first = ArtifactKey::Attachment {
        original_index: 1,
        mime_version: 1,
    };
    let second = ArtifactKey::Attachment {
        original_index: 2,
        mime_version: 1,
    };
    let first_blob = publish(message, first, b"first");
    let second_blob = publish(message, second, b"second");
    let page = cache
        .sync_reclaim_candidates(&account, INBOX, None, 1)
        .unwrap();
    assert_eq!(page.candidates[0].materialized, first_blob);
    assert_eq!(page.candidates[0].raw, raw);
    let position = page.next.unwrap();
    let first_candidate = &page.candidates[0];
    let page = cache
        .sync_reclaim_candidates(&account, INBOX, Some(position), 1)
        .unwrap();
    assert_eq!(page.candidates[0].materialized, second_blob);
    let second_position = page.candidates[0].position;
    assert!(page.next.is_none());
    assert!(
        cache
            .evict_sync_candidate(message, first_candidate)
            .unwrap()
    );
    assert_eq!(cache.collect_blobs(&account).unwrap(), 0);
    assert_eq!(
        cache
            .open_blob(
                message,
                ArtifactKey::Raw,
                Access::LiveVerified,
                Some(raw.id)
            )
            .unwrap()
            .info(),
        raw
    );
    assert_eq!(
        cache
            .open_blob(message, second, Access::LiveVerified, Some(second_blob.id))
            .unwrap()
            .info(),
        second_blob
    );
    cache
        .observe_folder(&account, INBOX, 9, Some(33), 110)
        .unwrap();
    for uid in 8..24 {
        publish(reference(&account, uid), first, b"no local raw");
    }
    let late = reference(&account, 33);
    publish(late, ArtifactKey::Raw, b"raw");
    let late_blob = publish(late, first, b"late");
    let page = cache
        .sync_reclaim_candidates(&account, INBOX, Some(second_position), 1)
        .unwrap();
    assert!(page.candidates.is_empty());
    let page = cache
        .sync_reclaim_candidates(&account, INBOX, Some(page.next.unwrap()), 1)
        .unwrap();
    assert_eq!(page.candidates[0].materialized, late_blob);
    cache
        .observe_folder(&account, INBOX, 10, Some(7), 110)
        .unwrap();
    assert!(matches!(
        cache.sync_reclaim_candidates(&account, INBOX, Some(position), 1),
        Err(SyncError::Stale)
    ));
}

#[test]
fn v4_migration_rolls_back_independently_and_metadata_reopens_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let namespace = account.reference_namespace();
    {
        let cache = open(&path, &account, true).unwrap();
        let databases = cache.lock().unwrap();
        let db = &databases[&namespace].connection;
        db.execute_batch(
            "DROP TRIGGER delta_epoch_change; DROP TRIGGER delta_member_insert;
            DROP TRIGGER delta_member_update; DROP TRIGGER delta_flags_insert; DROP TRIGGER delta_flags_update;
            DROP TABLE sync_delta_seed; DROP TABLE sync_delta;
            DROP TRIGGER recency_artifact_insert; DROP TRIGGER recency_artifact_update;
            DROP TRIGGER recency_artifact_delete; DROP TABLE blob_recency;
            DROP TRIGGER sync_presence_insert; DROP TRIGGER sync_presence_update;
            DROP TRIGGER sync_tombstone_work; DROP TRIGGER sync_artifact_lost; DROP TABLE sync_checkpoints; DROP TABLE sync_work;
            PRAGMA user_version=3;",
        )
        .unwrap();
        // Keep the final index as a deterministic failure after earlier DDL.
    }
    assert!(open(&path, &account, true).is_err());
    {
        let cache = open(&path, &account, false).unwrap();
        let databases = cache.lock().unwrap();
        let db = &databases[&namespace].connection;
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            3
        );
        let partial: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='sync_checkpoints')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!partial);
        db.execute("DROP INDEX sync_message_uid", []).unwrap();
    }
    drop(open(&path, &account, true).unwrap());
    let cache = open(&path, &account, false).unwrap();
    assert_eq!(
        cache.lock().unwrap()[&namespace]
            .connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
}

#[test]
fn v5_recency_migration_is_atomic_and_foreground_touch_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let namespace = account.reference_namespace();
    let message = reference(&account, 7);
    let first = ArtifactKey::Attachment {
        original_index: 1,
        mime_version: 1,
    };
    let second = ArtifactKey::Attachment {
        original_index: 2,
        mime_version: 1,
    };
    {
        let cache = open(&path, &account, true).unwrap();
        cache
            .observe_folder(&account, INBOX, 9, Some(7), 100)
            .unwrap();
        for kind in [ArtifactKey::Raw, first, second] {
            let mut stage = cache.begin_blob(message, kind, Some(3), 3).unwrap();
            stage.write_all(b"raw").unwrap();
            stage.publish(&|| false).unwrap();
        }
        let databases = cache.lock().unwrap();
        databases[&namespace]
            .connection
            .execute_batch(
                "DROP TRIGGER delta_epoch_change; DROP TRIGGER delta_member_insert;
             DROP TRIGGER delta_member_update; DROP TRIGGER delta_flags_insert; DROP TRIGGER delta_flags_update;
             DROP TABLE sync_delta_seed; DROP TABLE sync_delta;
             DROP TRIGGER recency_artifact_insert; DROP TRIGGER recency_artifact_update;
             DROP TRIGGER recency_artifact_delete; DROP TABLE blob_recency;
             PRAGMA user_version=4; CREATE INDEX blob_recency_scope ON messages(uid);",
            )
            .unwrap();
    }
    assert!(open(&path, &account, true).is_err());
    {
        let cache = open(&path, &account, false).unwrap();
        let databases = cache.lock().unwrap();
        let db = &databases[&namespace].connection;
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            4
        );
        assert!(
            !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='blob_recency')",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
        db.execute("DROP INDEX blob_recency_scope", []).unwrap();
    }
    let cache = open(&path, &account, true).unwrap();
    let oldest = cache
        .sync_reclaim_candidates(&account, INBOX, None, 1)
        .unwrap()
        .candidates
        .pop()
        .unwrap();
    assert_eq!(oldest.kind, first);
    cache
        .open_blob(
            message,
            first,
            Access::LiveVerified,
            Some(oldest.materialized.id),
        )
        .unwrap()
        .touch();
    assert!(!cache.evict_sync_candidate(message, &oldest).unwrap());
    drop(cache);
    let cache = open(&path, &account, true).unwrap();
    let oldest = cache
        .sync_reclaim_candidates(&account, INBOX, None, 1)
        .unwrap()
        .candidates
        .pop()
        .unwrap();
    assert_eq!(oldest.kind, second);
    assert!(cache.evict_sync_candidate(message, &oldest).unwrap());
    assert_eq!(cache.collect_blobs(&account).unwrap(), 0);
    assert_eq!(
        cache
            .open_blob(message, ArtifactKey::Raw, Access::LiveVerified, None)
            .unwrap()
            .info(),
        oldest.raw
    );
}

#[test]
fn garbage_keyset_advances_after_an_ineligible_batch_and_keeps_current_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account, true).unwrap();
    let history = cache
        .start_sync(&account, INBOX, ScanKind::History, observation(20))
        .unwrap();
    let uids: Vec<_> = (1..20).collect();
    cache
        .record_sync_search(&history.stamp, 1, 19, &uids, &|| false)
        .unwrap();
    let page = cache.collect_sync_garbage(&account, None).unwrap();
    assert_eq!((page.inspected, page.removed), (16, 0));
    let next = cache.collect_sync_garbage(&account, page.next).unwrap();
    assert_eq!((next.inspected, next.removed), (3, 0));
    let raw_ref = reference(&account, 7);
    let mut stage = cache
        .begin_blob(raw_ref, ArtifactKey::Raw, Some(3), 3)
        .unwrap();
    stage.write_all(b"raw").unwrap();
    let raw = stage.publish(&|| false).unwrap();
    let lease = cache
        .open_blob(
            raw_ref,
            ArtifactKey::Raw,
            Access::LiveVerified,
            Some(raw.id),
        )
        .unwrap();
    let current = cache
        .start_sync(
            &account,
            INBOX,
            ScanKind::History,
            MailboxObservation {
                uid_validity: 10,
                ..observation(8)
            },
        )
        .unwrap();
    let current = cache
        .record_sync_search(&current.stamp, 1, 7, &[7], &|| false)
        .unwrap();
    let mut position = None;
    let mut removed = 0;
    loop {
        let page = cache.collect_sync_garbage(&account, position).unwrap();
        assert!(page.inspected <= 16);
        assert_eq!(page.released_bytes, 0);
        removed += page.removed;
        position = page.next;
        if position.is_none() {
            break;
        }
    }
    assert_eq!(removed, 39);
    drop(lease);
    let mut released = 0;
    loop {
        let page = cache.collect_sync_garbage(&account, position).unwrap();
        released += page.released_bytes;
        position = page.next;
        if position.is_none() {
            break;
        }
    }
    assert!(released > 0);
    let work = cache.sync_content_page(&current.stamp, 100, 1).unwrap();
    assert_eq!(work[0].uid, 7);
    assert!(work[0].matches_scope(&account.reference_namespace(), INBOX, 10));
}

#[test]
fn garbage_releases_real_disk_space_even_when_retired_work_lacks_journal_room() {
    const CHUNK: usize = 64 * 1024;
    const LIMIT: u64 = 8 * MIB;
    let dir = tempfile::tempdir().unwrap();
    let retired_account = AccountKey::synthetic();
    let other_account = AccountKey::synthetic();
    let cache = Cache::open_full(
        &dir.path().join("cache"),
        LIMIT,
        [&retired_account, &other_account]
            .into_iter()
            .map(|account| AccountScope {
                account: account.clone(),
                mailboxes: vec![INBOX.into()],
                max_bytes: LIMIT,
            })
            .collect(),
        &[],
    )
    .unwrap();
    let history = cache
        .start_sync(&retired_account, INBOX, ScanKind::History, observation(21))
        .unwrap();
    let uids: Vec<_> = (1..=12).collect();
    cache
        .record_sync_search(&history.stamp, 1, 20, &uids, &|| false)
        .unwrap();
    let header = vec![b'x'; 96 * 1024];
    for uid in uids {
        cache
            .remember_headers(reference(&retired_account, uid), true, &header, None)
            .unwrap();
    }
    let orphan = reference(&retired_account, 20);
    let block = vec![b'x'; CHUNK];
    let mut stage = cache
        .begin_blob(orphan, ArtifactKey::Raw, Some(2 * MIB), 2 * MIB)
        .unwrap();
    for _ in 0..32 {
        stage.write_all(&block).unwrap();
    }
    stage.publish(&|| false).unwrap();
    cache.tombstone_observed(orphan, 21).unwrap();
    cache
        .observe_folder(&retired_account, INBOX, 10, Some(1), 110)
        .unwrap();
    cache
        .observe_folder(&other_account, INBOX, 9, Some(1), 100)
        .unwrap();
    let name = disk::namespace_name(&other_account.reference_namespace());
    let (used, _) = cache.disk.bytes(&name).unwrap();
    let mut requested = (LIMIT - used) / CHUNK as u64 * CHUNK as u64;
    let mut stage = loop {
        assert!(requested >= CHUNK as u64);
        match cache.begin_blob(
            reference(&other_account, 1),
            ArtifactKey::Raw,
            Some(requested),
            requested,
        ) {
            Ok(stage) => break stage,
            Err(CacheError::Quota) => requested -= CHUNK as u64,
            Err(error) => panic!("{error}"),
        }
    };
    for _ in 0..requested / CHUNK as u64 {
        stage.write_all(&block).unwrap();
    }
    let retained = stage.publish(&|| false).unwrap();
    assert!(matches!(
        cache.collect_retired_sync_work(&retired_account, 1),
        Err(SyncError::Cache(CacheError::Quota))
    ));
    let first = cache.collect_sync_garbage(&retired_account, None).unwrap();
    assert_eq!((first.inspected, first.removed), (12, 0));
    let mut position = first.next;
    let mut released = 0;
    while let Some(next) = position {
        let page = cache
            .collect_sync_garbage(&retired_account, Some(next))
            .unwrap();
        released += page.released_bytes;
        position = page.next;
    }
    assert!(released >= 2 * MIB);
    let retry = cache.collect_sync_garbage(&retired_account, None).unwrap();
    assert_eq!((retry.inspected, retry.removed), (12, 12));
    assert_eq!(
        cache
            .open_blob(
                reference(&other_account, 1),
                ArtifactKey::Raw,
                Access::LiveVerified,
                Some(retained.id)
            )
            .unwrap()
            .info(),
        retained
    );
}
