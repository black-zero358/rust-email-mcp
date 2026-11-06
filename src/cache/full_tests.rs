use super::*;
use std::{
    fs,
    io::{Read, Write},
};

const MIB: u64 = 1024 * 1024;
const INBOX: &str = "INBOX";
const ARCHIVE: &str = "Archive";
const BODY: ArtifactKey = ArtifactKey::FullBody {
    mime_version: 1,
    html_version: 1,
};

fn scope(account: &AccountKey, max_bytes: u64) -> AccountScope {
    AccountScope {
        account: account.clone(),
        mailboxes: vec![INBOX.into(), ARCHIVE.into()],
        max_bytes,
    }
}

fn open(path: &Path, account: &AccountKey) -> Arc<Cache> {
    Cache::open_full(path, 24 * MIB, vec![scope(account, 16 * MIB)], &[]).unwrap()
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
        .observe_folder(account, INBOX, 9, Some(8), 100)
        .unwrap();
}

fn publish(cache: &Arc<Cache>, reference: Ref<'_>, kind: ArtifactKey, bytes: &[u8]) -> BlobInfo {
    let length = bytes.len() as u64;
    let mut stage = cache
        .begin_blob(reference, kind, Some(length), length)
        .unwrap();
    for chunk in bytes.chunks(3) {
        stage.write_all(chunk).unwrap();
    }
    stage.publish(&|| false).unwrap()
}

fn count_files(cache: &Cache, account: &AccountKey) -> usize {
    let name = disk::namespace_name(&account.reference_namespace());
    fs::read_dir(cache.disk.blob_root(&name).unwrap())
        .unwrap()
        .count()
}

#[test]
fn tombstone_blocks_open_lease_and_defers_physical_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let info = publish(&cache, reference, ArtifactKey::Raw, b"message bytes");
    cache
        .record_artifact_state(reference, BODY, ArtifactState::Derivable { raw: info.id })
        .unwrap();
    let mut lease = cache
        .open_blob(reference, ArtifactKey::Raw, Access::Offline, None)
        .unwrap();
    cache.tombstone(reference).unwrap();
    assert!(lease.read(&mut [0; 1]).is_err());
    assert_eq!(cache.collect_blobs(&account).unwrap(), 0);
    assert_eq!(count_files(&cache, &account), 1);
    drop(lease);
    assert_eq!(cache.collect_blobs(&account).unwrap(), 1);
    assert_eq!(count_files(&cache, &account), 0);
    assert_eq!(
        cache.artifact_state(reference, BODY).unwrap(),
        ArtifactState::Missing
    );
}

#[test]
fn multi_scope_claims_fence_unknown_folders_and_prevent_reusing_old_leases() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    publish(&cache, reference, ArtifactKey::Raw, b"message bytes");
    let mut lease = cache
        .open_blob(reference, ArtifactKey::Raw, Access::Offline, None)
        .unwrap();
    let stamp = cache.dirty_stamp(&account, INBOX).unwrap();
    let claim = cache.begin_dirty(&account, &[INBOX, ARCHIVE]).unwrap();
    assert!(matches!(
        cache.dirty_stamp(&account, INBOX),
        Err(CacheError::NeedsVerification)
    ));
    assert!(matches!(
        cache.clear_dirty_if_verified(&stamp, 9),
        Err(CacheError::NeedsVerification)
    ));
    cache
        .observe_folder(&account, ARCHIVE, 10, Some(1), 110)
        .unwrap();
    cache.finish_dirty(claim).unwrap();
    assert!(matches!(
        cache.clear_dirty_if_verified(&stamp, 9),
        Err(CacheError::NeedsVerification)
    ));
    let current = cache.dirty_stamp(&account, INBOX).unwrap();
    cache.clear_dirty_if_verified(&current, 9).unwrap();
    assert!(lease.read(&mut [0; 1]).is_err());
    assert!(
        cache
            .open_blob(reference, ArtifactKey::Raw, Access::Offline, None)
            .is_ok()
    );
    let archive = cache.dirty_stamp(&account, ARCHIVE).unwrap();
    assert!(matches!(
        cache.clear_dirty_if_verified(&archive, 9),
        Err(CacheError::NeedsVerification)
    ));
    cache.clear_dirty_if_verified(&archive, 10).unwrap();
}
