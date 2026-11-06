use super::*;
use crate::cache::{AccountScope, Ref};
use std::{cell::Cell, path::Path, sync::Arc};

const INBOX: &str = "INBOX";
const MIB: u64 = 1024 * 1024;

fn open(path: &Path, account: &AccountKey) -> Arc<Cache> {
    let scopes = vec![AccountScope {
        account: account.clone(),
        mailboxes: vec![INBOX.into()],
        max_bytes: 32 * MIB,
    }];
    Cache::open_full(path, 40 * MIB, scopes, &[]).unwrap()
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
fn dirty_reconcile_needs_a_pre_examine_fence_and_atomic_current_flags() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
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
    let page = cache.sync_known_page(&scan.stamp, 200).unwrap();
    assert_eq!(page.uids, [7, 8, 9]);
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
}
