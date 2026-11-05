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
    let jobs = cache.sync_content_page(&current.stamp, 100, 2).unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].matches_scope(&account.reference_namespace(), INBOX, 10));
    assert_eq!(jobs[0].uid, 7);
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
            "DROP TRIGGER sync_presence_insert; DROP TRIGGER sync_presence_update;
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
        4
    );
}
