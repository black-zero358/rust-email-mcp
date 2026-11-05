use super::*;
use crate::cache::{
    AccountScope, CacheError,
    cursor::{CursorBinding, CursorError, CursorScope, CursorWrite},
};

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

#[tokio::test]
async fn exhausted_or_expired_fence_admission_closes_offline_before_online_write() {
    let (_directory, account, cache) = storage();
    let pool = WriteGuardPool::with_capacity(1);
    let fence = pool
        .begin(&cache, &account, &["INBOX"], WAIT)
        .await
        .unwrap();
    assert!(cache.offline_scope_allowed(&account, "Drafts", 7).is_ok());

    assert!(
        pool.begin(&cache, &account, &["Drafts"], WAIT)
            .await
            .is_none()
    );
    assert!(cache.offline_scope_allowed(&account, "Drafts", 7).is_err());
    drop(fence);
    assert!(pool.drain(WAIT).await);

    let (_directory, account, cache) = storage();
    assert!(cache.offline_scope_allowed(&account, "Drafts", 7).is_ok());
    assert!(
        pool.begin(&cache, &account, &["Drafts"], Duration::ZERO)
            .await
            .is_none()
    );
    assert!(cache.offline_scope_allowed(&account, "Drafts", 7).is_err());
}

#[tokio::test]
async fn storage_rejection_does_not_return_an_armed_guard_or_hold_the_permit() {
    let (_directory, account, cache) = storage();
    let pool = WriteGuardPool::with_capacity(1);
    assert!(
        pool.begin(&cache, &account, &["not-configured"], WAIT)
            .await
            .is_none()
    );
    assert!(cache.offline_scope_allowed(&account, "Drafts", 7).is_err());
    assert!(pool.drain(WAIT).await);
}

async fn block_storage(
    cache: &Arc<Cache>,
    account: &AccountKey,
) -> (
    tokio::task::JoinHandle<Result<(), CursorError>>,
    std::sync::mpsc::Sender<()>,
) {
    let (locked, lock_ready) = tokio::sync::oneshot::channel();
    let (release, unlocked) = std::sync::mpsc::channel();
    let backend = cache.cursor_backend();
    let binding = CursorBinding {
        namespace: account.reference_namespace(),
        principal: [9; 32],
    };
    let blocker = tokio::task::spawn_blocking(move || {
        backend.save(
            CursorWrite {
                binding,
                expected: None,
                active: Some([1; 16]),
                replay: None,
                codec_version: 1,
                owned_bytes: 128,
                encoded_len: 1,
                expires_ms: 600_001,
                last_progress_ms: 1,
                now_ms: 1,
                frozen: &[CursorScope {
                    mailbox: "INBOX",
                    uid_validity: 7,
                }],
                cancelled: &|| false,
            },
            |output| {
                locked.send(()).map_err(|_| CursorError::Storage)?;
                unlocked
                    .recv_timeout(WAIT)
                    .map_err(|_| CursorError::Storage)?;
                output.write_all(&[1]).map_err(|_| CursorError::Storage)
            },
        )
    });
    tokio::time::timeout(WAIT, lock_ready)
        .await
        .unwrap()
        .unwrap();
    (blocker, release)
}

#[tokio::test]
async fn a_timed_out_blocking_fence_keeps_its_permit_until_cleanup() {
    let (_directory, account, cache) = storage();
    let pool = WriteGuardPool::with_capacity(1);
    let (blocker, release) = block_storage(&cache, &account).await;
    assert!(
        pool.begin(&cache, &account, &["Drafts"], Duration::from_millis(20))
            .await
            .is_none()
    );
    assert!(!pool.drain(Duration::ZERO).await);
    release.send(()).unwrap();
    blocker.await.unwrap().unwrap();
    assert!(pool.drain(WAIT).await);
    assert!(cache.offline_scope_allowed(&account, "INBOX", 7).is_err());
    assert!(cache.dirty_stamp(&account, "Drafts").is_ok());
}

#[tokio::test]
async fn shutdown_seals_future_writes_and_waits_for_already_queued_work() {
    let pool = WriteGuardPool::default();
    let queued = pool.admit().unwrap();
    assert!(!pool.seal_and_drain(Duration::ZERO).await);
    assert!(pool.admit().is_none());
    drop(queued);
    assert!(pool.seal_and_drain(WAIT).await);
    assert!(pool.admit().is_none());
}

#[tokio::test]
async fn cancelled_write_keeps_detached_fence_inside_shutdown_barrier() {
    let (_directory, account, cache) = storage();
    let pool = Arc::new(WriteGuardPool::with_capacity(1));
    let (blocker, release) = block_storage(&cache, &account).await;
    let write = {
        let pool = pool.clone();
        let cache = cache.clone();
        let account = account.clone();
        let admitted = pool.admit().unwrap();
        tokio::spawn(async move {
            let _admitted = admitted;
            pool.begin(&cache, &account, &["Drafts"], WAIT).await
        })
    };
    tokio::time::timeout(WAIT, async {
        while pool.permits.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    write.abort();
    assert!(matches!(write.await, Err(error) if error.is_cancelled()));
    assert!(!pool.seal_and_drain(Duration::ZERO).await);
    assert!(pool.admit().is_none());
    release.send(()).unwrap();
    blocker.await.unwrap().unwrap();
    assert!(pool.seal_and_drain(WAIT).await);
    assert!(cache.offline_scope_allowed(&account, "Drafts", 7).is_err());
    assert!(cache.dirty_stamp(&account, "Drafts").is_ok());
}
