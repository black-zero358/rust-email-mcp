use super::*;
use crate::cache::{AccountScope, CacheError};

const QUOTA: u64 = 32 * 1024 * 1024;
const WAIT: Duration = Duration::from_secs(2);

fn storage() -> (tempfile::TempDir, AccountKey, Arc<Cache>) {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = Cache::open_full(
        &directory.path().join("cache"),
        QUOTA,
        vec![AccountScope {
            account: account.clone(),
            mailboxes: vec!["INBOX".into(), "Drafts".into()],
            max_bytes: QUOTA,
        }],
        &[],
    )
    .unwrap();
    for mailbox in ["INBOX", "Drafts"] {
        cache
            .observe_folder(&account, mailbox, 7, Some(2), 1)
            .unwrap();
    }
    (directory, account, cache)
}

#[tokio::test]
async fn a_mutation_fences_all_affected_folders_before_the_caller_can_write() {
    let (_directory, account, cache) = storage();
    let pool = WriteGuardPool::default();
    assert!(cache.offline_scope_allowed(&account, "INBOX", 7).is_ok());
    assert!(cache.offline_scope_allowed(&account, "Drafts", 7).is_ok());

    let fence = pool
        .begin(&cache, &account, &["INBOX", "Drafts"], WAIT)
        .await
        .unwrap();
    for mailbox in ["INBOX", "Drafts"] {
        assert!(matches!(
            cache.offline_scope_allowed(&account, mailbox, 7),
            Err(CacheError::NeedsVerification)
        ));
        assert!(matches!(
            cache.dirty_stamp(&account, mailbox),
            Err(CacheError::NeedsVerification)
        ));
    }
    drop(fence);
    assert!(pool.drain(WAIT).await);

    for mailbox in ["INBOX", "Drafts"] {
        // Completion released the claim, but online evidence is still required.
        assert!(matches!(
            cache.offline_scope_allowed(&account, mailbox, 7),
            Err(CacheError::NeedsVerification)
        ));
        let stamp = cache.dirty_stamp(&account, mailbox).unwrap();
        cache.clear_dirty_if_verified(&stamp, 7).unwrap();
        assert!(cache.offline_scope_allowed(&account, mailbox, 7).is_ok());
    }
}

#[tokio::test]
async fn a_stale_reconciliation_stamp_cannot_clear_a_later_mutation() {
    let (_directory, account, cache) = storage();
    let pool = WriteGuardPool::default();
    let first = pool
        .begin(&cache, &account, &["INBOX"], WAIT)
        .await
        .unwrap();
    drop(first);
    assert!(pool.drain(WAIT).await);
    let old = cache.dirty_stamp(&account, "INBOX").unwrap();
    let second = pool
        .begin(&cache, &account, &["INBOX"], WAIT)
        .await
        .unwrap();
    assert!(cache.clear_dirty_if_verified(&old, 7).is_err());
    drop(second);
    assert!(pool.drain(WAIT).await);

    assert!(cache.clear_dirty_if_verified(&old, 7).is_err());
    assert!(cache.offline_scope_allowed(&account, "INBOX", 7).is_err());
    let new = cache.dirty_stamp(&account, "INBOX").unwrap();
    cache.clear_dirty_if_verified(&new, 7).unwrap();
    assert!(cache.offline_scope_allowed(&account, "INBOX", 7).is_ok());
}

#[tokio::test(start_paused = true)]
async fn sealed_pool_drains_only_when_admitted_tickets_finish() {
    let pool = WriteGuardPool::default();
    let admitted = pool.admit().unwrap();
    pool.seal();
    assert!(pool.admit().is_none());
    assert!(!pool.drain_until(Instant::now() + WAIT).await);
    drop(admitted);
    assert!(pool.drain_until(Instant::now() + WAIT).await);
}
