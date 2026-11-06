use super::*;
use std::{cell::Cell, io::Write};

const MIB: u64 = 1024 * 1024;
const INBOX: &str = "INBOX";
const BODY: ArtifactKey = ArtifactKey::FullBody {
    mime_version: 1,
    html_version: 1,
};

fn open(path: &Path, account: &AccountKey) -> Arc<Cache> {
    let scope = AccountScope {
        account: account.clone(),
        mailboxes: vec![INBOX.into()],
        max_bytes: 64 * MIB,
    };
    Cache::open_full(path, 96 * MIB, vec![scope], &[]).unwrap()
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
fn body_index_is_complete_only_after_all_segments_and_matches_across_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let blob = publish_body(&cache, reference);
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
