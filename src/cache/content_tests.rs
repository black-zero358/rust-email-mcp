use super::*;
use crate::accounts::CacheRunIdentity;
use std::{
    cell::Cell,
    io::{Read, Write},
};
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;
const INBOX: &str = "INBOX";
const SENT: &str = "Sent Items";
const BODY: ArtifactKey = ArtifactKey::FullBody {
    mime_version: 1,
    html_version: 1,
};

fn scopes(account: &AccountKey, mailboxes: &[&str]) -> Vec<AccountScope> {
    vec![AccountScope {
        account: account.clone(),
        mailboxes: mailboxes.iter().copied().map(str::to_owned).collect(),
        max_bytes: 64 * MIB,
    }]
}

fn open(path: &Path, account: &AccountKey) -> Arc<Cache> {
    Cache::open_full(path, 96 * MIB, scopes(account, &[INBOX, SENT]), &[]).unwrap()
}

fn open_run(
    path: &Path,
    account: &AccountKey,
    mailboxes: &[&str],
    run: CacheRunIdentity,
) -> Arc<Cache> {
    Cache::open_full_with_run(path, 96 * MIB, scopes(account, mailboxes), &[], run).unwrap()
}

fn message(account: &AccountKey, uid: u32) -> Ref<'_> {
    Ref {
        account,
        mailbox: INBOX,
        uid_validity: 9,
        uid,
    }
}

fn observe(cache: &Cache, account: &AccountKey) {
    cache
        .observe_folder(account, INBOX, 9, Some(600), 100)
        .unwrap();
}

fn publish_body(cache: &Arc<Cache>, reference: Ref<'_>) -> BlobInfo {
    let text = "完整正文".as_bytes();
    let mut stage = cache
        .begin_blob(reference, BODY, Some(text.len() as u64), text.len() as u64)
        .unwrap();
    stage.write_all(text).unwrap();
    stage
        .publish_body(b"versioned leaf metadata", &|| false)
        .unwrap()
}

#[test]
fn v2_to_v3_keeps_headers_and_a_failed_migration_does_not_publish_partial_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let namespace = account.reference_namespace();
    let v1 = || Cache::open(&path, 96 * MIB, scopes(&account, &[INBOX, SENT]), &[]).unwrap();
    {
        let cache = v1();
        observe(&cache, &account);
        cache
            .remember_headers(message(&account, 7), true, b"old", None)
            .unwrap();
        let databases = cache.lock().unwrap();
        let db = &databases[&namespace].connection;
        db.execute_batch(schema::V2).unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
        db.execute("CREATE TABLE body_segments(placeholder INTEGER)", [])
            .unwrap();
    }
    assert!(Cache::open_full(&path, 96 * MIB, scopes(&account, &[INBOX, SENT]), &[]).is_err());
    {
        let cache = v1();
        let databases = cache.lock().unwrap();
        let db = &databases[&namespace].connection;
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let partial: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='listing_evidence')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!partial);
        db.execute("DROP TABLE body_segments", []).unwrap();
    }
    let cache = open(&path, &account);
    assert_eq!(
        cache.header(message(&account, 7)).unwrap().as_deref(),
        Some(&b"old"[..])
    );
    assert_eq!(
        cache.lock().unwrap()[&namespace]
            .connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        6
    );
}

#[test]
fn receipts_require_adjacent_runs_matching_root_and_configured_scope() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let moved = dir.path().join("other-root");
    let account = AccountKey::synthetic();
    let instance = Uuid::new_v4();
    let run = |previous| CacheRunIdentity {
        instance,
        previous,
        current: Uuid::new_v4(),
    };
    let a = run(None);
    let before_gap;
    {
        let cache = open_run(&path, &account, &[SENT, INBOX], a);
        observe(&cache, &account);
        assert!(cache.offline_scope_allowed(&account, INBOX, 9).is_ok());
        cache.mark_clean().unwrap();
    }
    let b = run(Some(a.current));
    {
        let cache = open_run(&path, &account, &[INBOX, SENT], b);
        assert!(cache.offline_scope_allowed(&account, INBOX, 9).is_ok());
        before_gap = cache.freeze_scope(&account, INBOX).unwrap();
        cache.mark_clean().unwrap();
    }
    let skipped = run(Some(b.current));
    let c = run(Some(skipped.current));
    {
        let cache = open_run(&path, &account, &[INBOX, SENT], c);
        assert!(matches!(
            cache.offline_scope_allowed(&account, INBOX, 9),
            Err(CacheError::NeedsVerification)
        ));
        cache
            .clear_dirty_if_verified(&cache.dirty_stamp(&account, INBOX).unwrap(), 9)
            .unwrap();
        assert!(cache.verify_scope(&account, &before_gap).is_err());
        cache.mark_clean().unwrap();
    }
    let d = run(Some(c.current));
    {
        let cache = open_run(&path, &account, &[INBOX], d);
        assert!(cache.offline_scope_allowed(&account, INBOX, 9).is_err());
        cache
            .clear_dirty_if_verified(&cache.dirty_stamp(&account, INBOX).unwrap(), 9)
            .unwrap();
        cache.mark_clean().unwrap();
    }
    std::fs::rename(&path, &moved).unwrap();
    let cache = open_run(&moved, &account, &[INBOX], run(Some(d.current)));
    assert!(cache.offline_scope_allowed(&account, INBOX, 9).is_err());
}

#[test]
fn outstanding_blob_lease_leaves_the_run_unclean() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let first = CacheRunIdentity {
        instance: Uuid::new_v4(),
        previous: None,
        current: Uuid::new_v4(),
    };
    {
        let cache = open_run(&path, &account, &[INBOX], first);
        observe(&cache, &account);
        let reference = message(&account, 7);
        let blob = publish_body(&cache, reference);
        let _lease = cache
            .open_blob(reference, BODY, Access::Offline, Some(blob.id))
            .unwrap();
        cache.mark_clean().unwrap();
    }
    let next = CacheRunIdentity {
        previous: Some(first.current),
        current: Uuid::new_v4(),
        ..first
    };
    let cache = open_run(&path, &account, &[INBOX], next);
    assert!(matches!(
        cache.offline_scope_allowed(&account, INBOX, 9),
        Err(CacheError::NeedsVerification)
    ));
}

#[test]
fn listing_evidence_keeps_observed_flags_dates_and_sizes_independently() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let flags = ["Seen".into()];
    cache
        .remember_headers(reference, true, b"header", None)
        .unwrap();
    cache
        .remember_listing(
            reference,
            ListingEvidence {
                flags: Some(&flags),
                internal_date: Some("2026-11-05T00:00:00+00:00"),
                size: Some(123),
                observed_at_ms: 120,
            },
        )
        .unwrap();
    cache
        .remember_listing(
            reference,
            ListingEvidence {
                flags: Some(&[]),
                internal_date: None,
                size: None,
                observed_at_ms: 130,
            },
        )
        .unwrap();
    cache
        .remember_listing(
            reference,
            ListingEvidence {
                flags: Some(&flags),
                internal_date: None,
                size: Some(99),
                observed_at_ms: 110,
            },
        )
        .unwrap();
    cache
        .remember_headers(message(&account, 8), false, b"prefix", None)
        .unwrap();
    let first = cache.list_cached(&account, INBOX, None, 1).unwrap();
    assert_eq!(first.items[0].uid, 8);
    assert!(!first.items[0].header_complete);
    assert!(first.items[0].flags.is_none());
    let second = cache
        .list_cached(&account, INBOX, first.next_before_uid, 1)
        .unwrap();
    let known = &second.items[0];
    assert_eq!(known.uid, 7);
    assert_eq!(known.flags.as_deref(), Some(&[][..]));
    assert_eq!(known.flags_checked_at_ms, Some(130));
    assert_eq!(
        known.internal_date.as_deref(),
        Some("2026-11-05T00:00:00+00:00")
    );
    assert_eq!(known.size, Some(123));
    assert_eq!(known.size_checked_at_ms, Some(120));
    assert!(second.next_before_uid.is_none());

    let ticket = cache.begin_dirty(&account, &[INBOX]).unwrap();
    assert!(cache.list_cached(&account, INBOX, None, 1).is_err());
    assert!(cache.cached_message(reference).is_err());
    assert!(
        cache
            .cached_message_live_verified(reference)
            .unwrap()
            .is_some()
    );
    cache.finish_dirty(ticket).unwrap();
}

#[test]
fn list_roles_stay_unknown_until_observed_and_sent_evidence_survives_dirty_fence() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    assert!(
        cache
            .cached_mailboxes(&account)
            .unwrap()
            .iter()
            .all(|row| row.evidence.is_none())
    );
    let evidence = MailboxEvidence {
        name: SENT.into(),
        attributes: "[Sent]".into(),
        attribute_flags: vec!["\\Sent".into()],
        delimiter: Some("/".into()),
        selectable: true,
        special_use: vec!["sent".into()],
        role_source: Some("server".into()),
        role_candidates: vec![],
    };
    cache
        .remember_mailboxes(&account, std::slice::from_ref(&evidence), 100)
        .unwrap();
    let rows = cache.cached_mailboxes(&account).unwrap();
    assert_eq!(rows[1].evidence.as_ref(), Some(&evidence));
    assert_eq!(rows[1].checked_at_ms, Some(100));
    let ticket = cache.begin_dirty(&account, &[SENT]).unwrap();
    assert!(
        cache.cached_mailboxes(&account).unwrap()[1]
            .evidence
            .is_none()
    );
    assert_eq!(cache.confirmed_sent(&account).unwrap(), [SENT]);
    cache.finish_dirty(ticket).unwrap();
}

#[test]
fn exact_thread_roles_are_bound_to_headers_and_frozen_index_revision() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let ids = [
        ThreadId {
            role: ThreadRole::MessageId,
            id: "<id@example.test>",
        },
        ThreadId {
            role: ThreadRole::References,
            id: "<prefix-id@example.test>",
        },
        ThreadId {
            role: ThreadRole::InReplyTo,
            id: "<id@example.test>",
        },
        ThreadId {
            role: ThreadRole::References,
            id: "<主题@例子.test>",
        },
    ];
    cache
        .remember_headers(reference, true, b"header", None)
        .unwrap();
    assert!(
        cache
            .remember_thread_ids(reference, THREAD_INDEX_VERSION, b"old", &ids)
            .is_err()
    );
    cache
        .remember_thread_ids(reference, THREAD_INDEX_VERSION, b"header", &ids)
        .unwrap();
    let frozen = cache.freeze_scope(&account, INBOX).unwrap();
    assert_eq!(frozen.indexed_threads, 1);
    for (id, expected) in [("<主题@例子.test>", &[7][..]), ("<主题@例子.TEST>", &[])] {
        assert_eq!(
            cache
                .thread_uids(&account, &frozen, ThreadRole::References, id, 1..=200)
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        cache
            .thread_uids(
                &account,
                &frozen,
                ThreadRole::MessageId,
                "<id@example.test>",
                1..=200
            )
            .unwrap(),
        [7]
    );
    assert!(
        cache
            .thread_uids(
                &account,
                &frozen,
                ThreadRole::References,
                "<id@example.test>",
                1..=200
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        cache
            .thread_uids(
                &account,
                &frozen,
                ThreadRole::MessageId,
                "<id@example.test>",
                1..=201
            )
            .is_err()
    );
    assert_eq!(
        cache
            .thread_header(&account, &frozen, 7)
            .unwrap()
            .as_deref(),
        Some(&b"header"[..])
    );
    cache
        .remember_headers(reference, true, b"header", None)
        .unwrap();
    cache
        .remember_thread_ids(reference, THREAD_INDEX_VERSION, b"header", &ids)
        .unwrap();
    cache.verify_scope(&account, &frozen).unwrap();
    cache
        .remember_headers(message(&account, 8), true, b"new older evidence", None)
        .unwrap();
    assert!(cache.verify_scope(&account, &frozen).is_err());
    let current = cache.freeze_scope(&account, INBOX).unwrap();
    assert_eq!(
        cache.scope_uids(&account, &current, 1..=500, 202).unwrap(),
        [8, 7]
    );
    cache.tombstone(reference).unwrap();
    assert!(cache.thread_header(&account, &current, 7).is_err());
}

#[test]
fn body_index_is_complete_only_after_all_segments_and_matches_across_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let blob = publish_body(&cache, reference);
    let mut lease = cache
        .open_blob(reference, BODY, Access::Offline, Some(blob.id))
        .unwrap();
    assert_eq!(lease.body_metadata(), Some(&b"versioned leaf metadata"[..]));
    let mut raw = String::new();
    lease.read_to_string(&mut raw).unwrap();
    assert_eq!(raw, "完整正文");
    let text = format!("{}中文会议 a\"b", "x".repeat(16380));
    let scope = cache.freeze_scope(&account, INBOX).unwrap();
    let initial = cache.search_revision(&account, &scope).unwrap();
    let calls = Cell::new(0);
    assert!(
        cache
            .remember_full_body_index(reference, BODY, blob.id, &text, &|| {
                calls.set(calls.get() + 1);
                calls.get() == 6
            })
            .is_err()
    );
    let partial = cache
        .search_cached(&account, INBOX, "中文会", None, 10, Some((1, 1)))
        .unwrap();
    assert!(partial.items.is_empty());
    assert_eq!(partial.indexed_body, 0);
    assert_eq!(partial.search_revision, initial);
    cache
        .remember_full_body_index(reference, BODY, blob.id, &text, &|| false)
        .unwrap();
    for literal in ["中文会", "中文", "a\"b"] {
        let result = cache
            .search_cached(&account, INBOX, literal, None, 10, Some((1, 1)))
            .unwrap();
        assert_eq!(result.indexed_body, 1);
        assert!(result.search_revision > initial);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].message.uid, 7);
        assert!(result.items[0].matched_body);
    }
    let complete = cache.search_revision(&account, &scope).unwrap();
    cache.verify_scope(&account, &scope).unwrap();
    cache
        .remember_full_body_index(reference, BODY, blob.id, &text, &|| false)
        .unwrap();
    assert_eq!(cache.search_revision(&account, &scope).unwrap(), complete);
    drop(lease);
    let mut raw = cache
        .begin_blob(reference, ArtifactKey::Raw, Some(3), 3)
        .unwrap();
    raw.write_all(b"raw").unwrap();
    let raw = raw.publish(&|| false).unwrap();
    assert!(
        cache
            .evict_derived(reference, BODY, blob.id, raw.id)
            .unwrap()
    );
    let evicted = cache.search_revision(&account, &scope).unwrap();
    assert!(evicted > complete);
    cache.collect_blobs(&account).unwrap();
    let result = cache
        .search_cached(&account, INBOX, "中文会", None, 10, Some((1, 1)))
        .unwrap();
    assert_eq!(result.indexed_body, 0);
    assert!(result.items.is_empty());
    assert_eq!(result.search_revision, evicted);
}

#[test]
fn search_revision_tracks_filter_evidence_and_ignores_repeat_observations() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    cache
        .remember_headers(reference, true, b"header", None)
        .unwrap();
    let scope = cache.freeze_scope(&account, INBOX).unwrap();
    let revision = || cache.search_revision(&account, &scope).unwrap();
    let initial = revision();
    cache
        .remember_headers(reference, true, b"header", Some("会议议程"))
        .unwrap();
    assert!(revision() > initial);
    let indexed = revision();
    cache
        .remember_headers(reference, true, b"header", Some("会议议程"))
        .unwrap();
    assert_eq!(revision(), indexed);
    let flags = ["Seen".into(), "Flagged".into()];
    let reordered = ["Flagged".into(), "Seen".into()];
    let listing = ListingEvidence {
        flags: Some(&flags),
        internal_date: Some("2026-11-05T00:00:00+00:00"),
        size: Some(123),
        observed_at_ms: 120,
    };
    cache
        .remember_listing(reference, ListingEvidence { ..listing })
        .unwrap();
    assert!(revision() > indexed);
    let mut last = revision();
    cache
        .remember_listing(
            reference,
            ListingEvidence {
                flags: Some(&reordered),
                observed_at_ms: 130,
                ..listing
            },
        )
        .unwrap();
    assert_eq!(revision(), last);
    for update in [
        ListingEvidence {
            flags: Some(&[]),
            internal_date: None,
            size: None,
            observed_at_ms: 140,
        },
        ListingEvidence {
            flags: None,
            internal_date: Some("2026-11-06T00:00:00+00:00"),
            size: None,
            observed_at_ms: 150,
        },
        ListingEvidence {
            flags: None,
            internal_date: None,
            size: Some(456),
            observed_at_ms: 160,
        },
    ] {
        cache.remember_listing(reference, update).unwrap();
        assert!(revision() > last);
        last = revision();
    }
    cache.remember_parts(reference, b"manifest").unwrap();
    assert!(revision() > last);
    last = revision();
    cache.remember_parts(reference, b"manifest").unwrap();
    assert_eq!(revision(), last);
    cache.verify_scope(&account, &scope).unwrap();
}

#[test]
fn body_metadata_is_verified_with_the_blob_before_a_lease_is_returned() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let blob = publish_body(&cache, reference);
    cache
        .remember_full_body_index(reference, BODY, blob.id, "完整正文", &|| false)
        .unwrap();
    let scope = cache.freeze_scope(&account, INBOX).unwrap();
    let complete = cache.search_revision(&account, &scope).unwrap();
    {
        let databases = cache.lock().unwrap();
        let db = &databases[&account.reference_namespace()].connection;
        db.execute(
            "UPDATE body_leaf_metadata SET payload=?1 WHERE blob_id=?2",
            rusqlite::params![&b"changed leaf metadata"[..], blob.id.bytes()],
        )
        .unwrap();
    }
    assert!(matches!(
        cache.open_blob(reference, BODY, Access::Offline, Some(blob.id)),
        Err(CacheError::Corrupt)
    ));
    assert_eq!(
        cache.artifact_state(reference, BODY).unwrap(),
        ArtifactState::Missing
    );
    let page = cache
        .search_cached(&account, INBOX, "完整正文", None, 10, Some((1, 1)))
        .unwrap();
    assert!(page.search_revision > complete);
    assert_eq!(page.indexed_body, 0);
    assert!(page.items.is_empty());
}

#[test]
fn long_and_short_queries_continue_after_a_bounded_empty_page() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    for uid in 1..=202 {
        let text = if uid == 1 {
            "会议议程"
        } else {
            "其他消息"
        };
        cache
            .remember_headers(message(&account, uid), true, b"header", Some(text))
            .unwrap();
    }
    for query in ["会议议", "会议"] {
        let first = cache
            .search_cached(&account, INBOX, query, None, 1, None)
            .unwrap();
        assert!(first.items.is_empty());
        assert!(first.next.is_some());
        let second = cache
            .search_cached(&account, INBOX, query, first.next, 1, None)
            .unwrap();
        assert_eq!(second.items[0].message.uid, 1);
        assert!(second.items[0].matched_metadata);
        assert!(second.next.is_none());
    }
}

#[test]
fn short_body_search_resumes_inside_a_message_without_a_duplicate_hit() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let blob = publish_body(&cache, reference);
    let text = format!("{}最后海", "x".repeat(16 * 1024 * 201));
    cache
        .remember_full_body_index(reference, BODY, blob.id, &text, &|| false)
        .unwrap();
    let first = cache
        .search_cached(&account, INBOX, "海", None, 1, Some((1, 1)))
        .unwrap();
    assert!(first.items.is_empty());
    assert_eq!(
        first.next,
        Some(SearchPosition {
            uid: 7,
            segment: Some(200)
        })
    );
    let next = cache
        .search_cached(&account, INBOX, "海", first.next, 1, Some((1, 1)))
        .unwrap();
    assert_eq!(next.items[0].message.uid, 7);
    assert!(next.items[0].matched_body);
    assert!(next.next.is_none());
    let fts = cache
        .search_cached(&account, INBOX, "最后海", None, 1, Some((1, 1)))
        .unwrap();
    assert_eq!(fts.items[0].message.uid, 7);
}

#[test]
fn offline_checkpoint_cas_preserves_the_old_token_after_a_scope_changes() {
    use super::cursor::{CursorBinding, CursorError, CursorExpected, CursorScope, CursorWrite};
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let expected = cache.freeze_scope(&account, INBOX).unwrap();
    let binding = CursorBinding {
        namespace: account.reference_namespace(),
        principal: [7; 32],
    };
    let old = [1; 16];
    let next = [2; 16];
    let folders = [CursorScope {
        mailbox: INBOX,
        uid_validity: 9,
    }];
    let make = |active, replay, expected| CursorWrite {
        binding,
        expected,
        active,
        replay,
        codec_version: 2,
        owned_bytes: 16,
        encoded_len: 4,
        expires_ms: 600_100,
        last_progress_ms: 100,
        now_ms: 100,
        frozen: &folders,
        cancelled: &|| false,
    };
    let encode =
        |writer: &mut dyn Write| writer.write_all(b"page").map_err(|_| CursorError::Storage);
    let backend = cache.cursor_backend();
    backend
        .save_offline(
            make(Some(old), None, None),
            std::slice::from_ref(&expected),
            encode,
        )
        .unwrap();
    cache
        .remember_headers(message(&account, 7), true, b"new", None)
        .unwrap();
    assert_eq!(
        backend.save_offline(
            make(
                Some(next),
                Some(old),
                Some(CursorExpected {
                    token: old,
                    revision: 1
                })
            ),
            std::slice::from_ref(&expected),
            encode,
        ),
        Err(CursorError::Unavailable)
    );
    assert_eq!(backend.validate(binding, old, 100).unwrap().revision, 1);
    assert_eq!(
        backend.validate(binding, next, 100),
        Err(CursorError::Unavailable)
    );
}
