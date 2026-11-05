use super::*;
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

const MIB: u64 = 1024 * 1024;
const INBOX: &str = "INBOX";
const ARCHIVE: &str = "Archive";
const BODY: ArtifactKey = ArtifactKey::FullBody {
    mime_version: 1,
    html_version: 1,
};
const ATTACHMENT: ArtifactKey = ArtifactKey::Attachment {
    original_index: 2,
    mime_version: 1,
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

fn bytes(lease: &mut BlobLease) -> Vec<u8> {
    let mut bytes = Vec::new();
    lease.read_to_end(&mut bytes).unwrap();
    bytes
}

fn count_files(cache: &Cache, account: &AccountKey) -> usize {
    let name = disk::namespace_name(&account.reference_namespace());
    fs::read_dir(cache.disk.blob_root(&name).unwrap())
        .unwrap()
        .count()
}

#[test]
fn full_upgrade_preserves_metadata_and_metadata_mode_can_reopen_v2() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let reference = message(&account, 7);
    {
        let cache = Cache::open(&path, 24 * MIB, vec![scope(&account, 16 * MIB)], &[]).unwrap();
        observe(&cache, &account);
        cache
            .remember_headers(reference, true, b"header", Some("中文主题"))
            .unwrap();
        cache.remember_parts(reference, b"manifest").unwrap();
        assert!(matches!(
            cache.begin_blob(reference, ArtifactKey::Raw, None, 20),
            Err(CacheError::OutOfScope)
        ));
        cache.mark_clean().unwrap();
    }
    {
        let cache = open(&path, &account);
        assert_eq!(
            cache.header(reference).unwrap().as_deref(),
            Some(&b"header"[..])
        );
        assert_eq!(
            cache.parts(reference).unwrap().as_deref(),
            Some(&b"manifest"[..])
        );
        assert_eq!(
            cache.artifact_state(reference, ArtifactKey::Raw).unwrap(),
            ArtifactState::Missing
        );
        publish(&cache, reference, ArtifactKey::Raw, b"raw email");
        cache.mark_clean().unwrap();
    }
    let cache = Cache::open(&path, 24 * MIB, vec![scope(&account, 16 * MIB)], &[]).unwrap();
    assert_eq!(
        cache.header(reference).unwrap().as_deref(),
        Some(&b"header"[..])
    );
    assert!(matches!(
        cache.open_blob(reference, ArtifactKey::Raw, Access::Offline, None),
        Err(CacheError::OutOfScope)
    ));
    assert_eq!(count_files(&cache, &account), 1);
}

#[test]
fn failed_v2_migration_rolls_back_all_ddl_and_preserves_v1_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let namespace = account.reference_namespace();
    let reference = message(&account, 7);
    let open_v1 = || Cache::open(&path, 24 * MIB, vec![scope(&account, 16 * MIB)], &[]).unwrap();
    {
        let cache = open_v1();
        observe(&cache, &account);
        cache
            .remember_headers(reference, true, b"header", None)
            .unwrap();
        cache.lock().unwrap()[&namespace]
            .connection
            .execute("CREATE TABLE mutation_scopes(other INTEGER)", [])
            .unwrap();
        cache.mark_clean().unwrap();
    }
    assert!(Cache::open_full(&path, 24 * MIB, vec![scope(&account, 16 * MIB)], &[]).is_err());
    let cache = open_v1();
    let dbs = cache.lock().unwrap();
    let db = &dbs[&namespace].connection;
    let version: u32 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
    let partial: u32 = db
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name IN ('blobs','artifacts','scope_fences')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(partial, 0);
    drop(dbs);
    assert_eq!(
        cache.header(reference).unwrap().as_deref(),
        Some(&b"header"[..])
    );
}

#[test]
fn layered_artifacts_are_immutable_and_independent_of_raw_availability() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let raw = publish(&cache, reference, ArtifactKey::Raw, b"raw email");
    assert_eq!(raw.sha256, <[u8; 32]>::from(Sha256::digest(b"raw email")));
    assert_eq!(
        publish(&cache, reference, ArtifactKey::Raw, b"raw email"),
        raw
    );
    let mut conflict = cache
        .begin_blob(reference, ArtifactKey::Raw, Some(3), 3)
        .unwrap();
    conflict.write_all(b"bad").unwrap();
    assert!(matches!(
        conflict.publish(&|| false),
        Err(CacheError::Corrupt)
    ));
    assert_eq!(count_files(&cache, &account), 1);
    assert_eq!(
        cache.artifact_state(reference, ArtifactKey::Raw).unwrap(),
        ArtifactState::Materialized(raw)
    );
    cache
        .record_artifact_state(reference, BODY, ArtifactState::Derivable { raw: raw.id })
        .unwrap();
    cache
        .record_artifact_state(reference, BODY, ArtifactState::Omitted(Omission::Quota))
        .unwrap();
    assert_eq!(
        cache.artifact_state(reference, BODY).unwrap(),
        ArtifactState::Derivable { raw: raw.id }
    );
    cache
        .record_artifact_state(reference, ATTACHMENT, ArtifactState::Missing)
        .unwrap();
    let other = message(&account, 8);
    assert!(matches!(
        cache.record_artifact_state(other, BODY, ArtifactState::Derivable { raw: raw.id }),
        Err(CacheError::OutOfScope)
    ));
    cache
        .record_artifact_state(
            other,
            ArtifactKey::Raw,
            ArtifactState::Omitted(Omission::RawLimit),
        )
        .unwrap();
    cache
        .record_artifact_state(
            other,
            BODY,
            ArtifactState::Unsupported(Omission::UnsupportedEncoding),
        )
        .unwrap();
    cache
        .record_artifact_state(other, BODY, ArtifactState::Missing)
        .unwrap();
    cache
        .record_artifact_state(other, ArtifactKey::Raw, ArtifactState::Missing)
        .unwrap();
    let part = publish(&cache, other, ATTACHMENT, b"independent attachment");
    assert!(matches!(
        cache.open_blob(reference, ArtifactKey::Raw, Access::Offline, Some(part.id)),
        Err(CacheError::OutOfScope)
    ));
    let mut lease = cache
        .open_blob(other, ATTACHMENT, Access::Offline, Some(part.id))
        .unwrap();
    assert_eq!(bytes(&mut lease), b"independent attachment");

    let raw_coverage = cache
        .layer_coverage(&account, INBOX, 9, ArtifactLayer::Raw)
        .unwrap();
    assert_eq!(
        raw_coverage,
        LayerCoverage {
            materialized: 1,
            materialized_bytes: raw.bytes,
            omitted: 1,
            ..LayerCoverage::default()
        }
    );
    let body_coverage = cache
        .layer_coverage(
            &account,
            INBOX,
            9,
            ArtifactLayer::FullBody {
                mime_version: 1,
                html_version: 1,
            },
        )
        .unwrap();
    assert_eq!(
        body_coverage,
        LayerCoverage {
            derivable_from_raw: 1,
            unsupported: 1,
            ..LayerCoverage::default()
        }
    );
    let part_coverage = cache
        .layer_coverage(
            &account,
            INBOX,
            9,
            ArtifactLayer::Attachments { mime_version: 1 },
        )
        .unwrap();
    assert_eq!(
        part_coverage,
        LayerCoverage {
            materialized: 1,
            materialized_bytes: part.bytes,
            missing: 1,
            ..LayerCoverage::default()
        }
    );
}

#[test]
fn cross_account_stage_reservations_and_per_file_limit_are_enforced_before_write() {
    let dir = tempfile::tempdir().unwrap();
    let a = AccountKey::synthetic();
    let b = AccountKey::synthetic();
    let cache = Cache::open_full(
        &dir.path().join("cache"),
        MIB,
        vec![scope(&a, MIB), scope(&b, MIB)],
        &[],
    )
    .unwrap();
    observe(&cache, &a);
    observe(&cache, &b);
    let before = cache.status(&a).unwrap().global_used_bytes;
    let stage = cache
        .begin_blob(message(&a, 7), ArtifactKey::Raw, None, MIB / 4)
        .unwrap();
    assert!(cache.status(&a).unwrap().global_used_bytes >= before + MIB / 4);
    assert!(matches!(
        cache.begin_blob(message(&b, 7), ArtifactKey::Raw, None, MIB / 4),
        Err(CacheError::Quota)
    ));
    assert!(matches!(
        cache.begin_blob(message(&a, 7), ArtifactKey::Raw, None, 32 * MIB + 1),
        Err(CacheError::Quota)
    ));
    drop(stage);
    assert!(!cache.disk.has_staged_blobs().unwrap());
    assert_eq!(count_files(&cache, &a), 0);
    assert!(
        cache
            .begin_blob(message(&b, 7), ArtifactKey::Raw, None, MIB / 4)
            .is_ok()
    );
}

#[test]
fn cancel_after_rename_and_overlong_input_leave_no_published_blob() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let mut stage = cache
        .begin_blob(reference, ArtifactKey::Raw, Some(3), 3)
        .unwrap();
    assert!(stage.write_all(b"long").is_err());
    assert!(matches!(stage.publish(&|| false), Err(CacheError::Corrupt)));
    assert_eq!(count_files(&cache, &account), 0);

    let mut stage = cache
        .begin_blob(reference, ArtifactKey::Raw, Some(3), 3)
        .unwrap();
    stage.write_all(b"raw").unwrap();
    let calls = Cell::new(0);
    assert!(matches!(
        stage.publish(&|| {
            calls.set(calls.get() + 1);
            calls.get() == 2
        }),
        Err(CacheError::OutOfScope)
    ));
    assert_eq!(calls.get(), 2);
    assert_eq!(
        cache.artifact_state(reference, ArtifactKey::Raw).unwrap(),
        ArtifactState::Missing
    );
    assert_eq!(count_files(&cache, &account), 0);
    assert!(!cache.disk.has_staged_blobs().unwrap());
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
    let status = cache.status(&account).unwrap();
    assert!(status.folders.iter().all(|folder| folder.dirty));
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
    assert!(
        cache
            .status(&account)
            .unwrap()
            .folders
            .iter()
            .all(|folder| folder.dirty)
    );
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

#[test]
fn clean_restart_and_restored_or_interrupted_runs_use_different_offline_gates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let reference = message(&account, 7);
    {
        let cache = open(&path, &account);
        observe(&cache, &account);
        publish(&cache, reference, ArtifactKey::Raw, b"raw");
        cache.mark_clean().unwrap();
    }
    {
        let cache = open(&path, &account);
        assert!(cache.offline_scope_allowed(&account, INBOX, 9).is_ok());
        let _interrupted = cache.begin_dirty(&account, &[INBOX]).unwrap();
        cache.mark_clean().unwrap();
    }
    let cache = open(&path, &account);
    assert!(matches!(
        cache.offline_scope_allowed(&account, INBOX, 9),
        Err(CacheError::NeedsVerification)
    ));
    let stamp = cache.dirty_stamp(&account, INBOX).unwrap();
    cache.clear_dirty_if_verified(&stamp, 9).unwrap();
    let mut lease = cache
        .open_blob(reference, ArtifactKey::Raw, Access::Offline, None)
        .unwrap();
    let mut live = cache
        .open_blob(reference, ArtifactKey::Raw, Access::LiveVerified, None)
        .unwrap();
    let mut old_stage = cache
        .begin_blob(message(&account, 8), ArtifactKey::Raw, Some(3), 3)
        .unwrap();
    old_stage.write_all(b"raw").unwrap();
    cache.mark_restored().unwrap();
    assert!(lease.read(&mut [0; 1]).is_err());
    assert!(live.read(&mut [0; 1]).is_err());
    assert!(matches!(
        old_stage.publish(&|| false),
        Err(CacheError::NeedsVerification)
    ));
    assert!(
        cache
            .open_blob(reference, ArtifactKey::Raw, Access::LiveVerified, None)
            .is_ok()
    );
    assert!(matches!(
        cache.clear_dirty_if_verified(&stamp, 9),
        Err(CacheError::NeedsVerification)
    ));
}

#[test]
fn recovery_removes_only_owned_orphans_and_invalidates_missing_references() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let name = disk::namespace_name(&account.reference_namespace());
    let reference = message(&account, 7);
    {
        let cache = open(&path, &account);
        observe(&cache, &account);
        let raw = publish(&cache, reference, ArtifactKey::Raw, b"raw");
        publish(&cache, reference, ATTACHMENT, b"derived attachment");
        cache
            .record_artifact_state(reference, BODY, ArtifactState::Derivable { raw: raw.id })
            .unwrap();
        let active = fs::read_dir(cache.disk.blob_root(&name).unwrap())
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.metadata().unwrap().len() == 3)
            .unwrap();
        assert_eq!(
            active.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(active.path()).unwrap();
    }
    let partition = path.join(name).join("blobs");
    for orphan in [
        ".stage-00000000000000000000000000000000",
        "00000000000000000000000000000001.blob",
    ] {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(partition.join(orphan))
            .unwrap()
            .write_all(b"orphan")
            .unwrap();
    }
    let cache = open(&path, &account);
    assert_eq!(count_files(&cache, &account), 1);
    assert_eq!(
        cache.artifact_state(reference, ArtifactKey::Raw).unwrap(),
        ArtifactState::Missing
    );
    assert_eq!(
        cache.artifact_state(reference, BODY).unwrap(),
        ArtifactState::Missing
    );
    let mut different = cache
        .begin_blob(reference, ArtifactKey::Raw, Some(3), 3)
        .unwrap();
    different.write_all(b"new").unwrap();
    assert!(matches!(
        different.publish(&|| false),
        Err(CacheError::Corrupt)
    ));
    let mut existing = cache
        .open_blob(reference, ATTACHMENT, Access::LiveVerified, None)
        .unwrap();
    assert_eq!(bytes(&mut existing), b"derived attachment");
}

#[test]
fn wrong_digest_is_rejected_before_a_lease_and_unknown_files_survive_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let name = disk::namespace_name(&account.reference_namespace());
    let reference = message(&account, 7);
    let cache = open(&path, &account);
    observe(&cache, &account);
    publish(&cache, reference, ArtifactKey::Raw, b"raw");
    let blobs = cache.disk.blob_root(&name).unwrap();
    let file = fs::read_dir(&blobs)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(file, b"bad").unwrap();
    assert!(matches!(
        cache.open_blob(reference, ArtifactKey::Raw, Access::Offline, None),
        Err(CacheError::Corrupt)
    ));
    assert_eq!(
        cache.artifact_state(reference, ArtifactKey::Raw).unwrap(),
        ArtifactState::Missing
    );
    drop(cache);
    let unknown = blobs.join("foreign.dat");
    fs::write(&unknown, b"keep").unwrap();
    assert!(Cache::open_full(&path, 24 * MIB, vec![scope(&account, 16 * MIB)], &[]).is_err());
    assert_eq!(fs::read(unknown).unwrap(), b"keep");
}

#[test]
fn same_digest_publication_repairs_a_corrupted_blob_and_invalidates_its_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let old = publish(&cache, reference, ArtifactKey::Raw, b"raw");
    cache
        .record_artifact_state(reference, BODY, ArtifactState::Derivable { raw: old.id })
        .unwrap();
    let name = disk::namespace_name(&account.reference_namespace());
    let file = fs::read_dir(cache.disk.blob_root(&name).unwrap())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(file, b"bad").unwrap();
    let new = publish(&cache, reference, ArtifactKey::Raw, b"raw");
    assert_ne!(new.id, old.id);
    assert_eq!(new.sha256, old.sha256);
    assert_eq!(
        cache.artifact_state(reference, BODY).unwrap(),
        ArtifactState::Missing
    );
    assert!(matches!(
        cache.open_blob(reference, ArtifactKey::Raw, Access::Offline, Some(old.id)),
        Err(CacheError::OutOfScope)
    ));
    let mut lease = cache
        .open_blob(reference, ArtifactKey::Raw, Access::Offline, Some(new.id))
        .unwrap();
    assert_eq!(bytes(&mut lease), b"raw");
    drop(lease);
    assert_eq!(cache.collect_blobs(&account).unwrap(), 1);
    assert_eq!(count_files(&cache, &account), 1);
    let file = fs::read_dir(cache.disk.blob_root(&name).unwrap())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(file, b"bad").unwrap();
    assert!(matches!(
        cache.open_blob(reference, ArtifactKey::Raw, Access::Offline, None),
        Err(CacheError::Corrupt)
    ));
    let mut different = cache
        .begin_blob(reference, ArtifactKey::Raw, Some(3), 3)
        .unwrap();
    different.write_all(b"new").unwrap();
    assert!(matches!(
        different.publish(&|| false),
        Err(CacheError::Corrupt)
    ));
}

#[test]
fn eviction_and_corruption_preserve_known_attachment_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let raw = publish(&cache, reference, ArtifactKey::Raw, b"raw");
    let part = publish(&cache, reference, ATTACHMENT, b"materialized attachment");
    let attachment_layer = ArtifactLayer::Attachments { mime_version: 1 };
    let mut lease = cache
        .open_blob(reference, ATTACHMENT, Access::Offline, Some(part.id))
        .unwrap();
    assert!(
        !cache
            .evict_derived(reference, ATTACHMENT, part.id, raw.id)
            .unwrap()
    );
    assert_eq!(bytes(&mut lease), b"materialized attachment");
    drop(lease);
    assert!(
        cache
            .evict_derived(reference, ATTACHMENT, part.id, raw.id)
            .unwrap()
    );
    assert_eq!(cache.collect_blobs(&account).unwrap(), 1);
    assert_eq!(
        cache
            .layer_coverage(&account, INBOX, 9, attachment_layer)
            .unwrap(),
        LayerCoverage {
            derivable_from_raw: 1,
            ..LayerCoverage::default()
        }
    );

    let name = disk::namespace_name(&account.reference_namespace());
    let file = fs::read_dir(cache.disk.blob_root(&name).unwrap())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(file, b"bad").unwrap();
    assert!(matches!(
        cache.open_blob(reference, ArtifactKey::Raw, Access::Offline, None),
        Err(CacheError::Corrupt)
    ));
    assert_eq!(
        cache
            .layer_coverage(&account, INBOX, 9, attachment_layer)
            .unwrap(),
        LayerCoverage {
            missing: 1,
            ..LayerCoverage::default()
        }
    );
}

#[test]
fn eviction_keeps_materialized_bytes_when_raw_verification_fails() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    let raw = publish(&cache, reference, ArtifactKey::Raw, b"raw");
    let part = publish(&cache, reference, ATTACHMENT, b"materialized attachment");
    let name = disk::namespace_name(&account.reference_namespace());
    let file = fs::read_dir(cache.disk.blob_root(&name).unwrap())
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.metadata().unwrap().len() == 3)
        .unwrap()
        .path();
    fs::write(file, b"bad").unwrap();
    assert!(matches!(
        cache.evict_derived(reference, ATTACHMENT, part.id, raw.id),
        Err(CacheError::Corrupt)
    ));
    assert_eq!(
        cache.artifact_state(reference, ATTACHMENT).unwrap(),
        ArtifactState::Materialized(part)
    );
}

#[test]
fn a_stamp_from_another_open_cache_cannot_clear_a_matching_scope() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let a = open(&dir.path().join("cache-a"), &account);
    let b = open(&dir.path().join("cache-b"), &account);
    for cache in [&a, &b] {
        observe(cache, &account);
        let claim = cache.begin_dirty(&account, &[INBOX]).unwrap();
        cache.finish_dirty(claim).unwrap();
    }
    let first = a.dirty_stamp(&account, INBOX).unwrap();
    let second = b.dirty_stamp(&account, INBOX).unwrap();
    assert!(matches!(
        b.clear_dirty_if_verified(&first, 9),
        Err(CacheError::NeedsVerification)
    ));
    assert!(b.offline_scope_allowed(&account, INBOX, 9).is_err());
    b.clear_dirty_if_verified(&second, 9).unwrap();
    assert!(b.offline_scope_allowed(&account, INBOX, 9).is_ok());
}

#[test]
fn a_failed_stage_cleanup_keeps_the_disk_reservation_until_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache");
    let account = AccountKey::synthetic();
    let name = disk::namespace_name(&account.reference_namespace());
    {
        let cache = open(&path, &account);
        observe(&cache, &account);
        let reference = message(&account, 7);
        let mut stage = cache
            .begin_blob(reference, ArtifactKey::Raw, None, 1024)
            .unwrap();
        stage.write_all(b"interrupted").unwrap();
        let blobs = cache.disk.blob_root(&name).unwrap();
        fs::set_permissions(&blobs, fs::Permissions::from_mode(0o500)).unwrap();
        drop(stage);
        assert!(cache.disk.has_staged_blobs().unwrap());
        fs::set_permissions(&blobs, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(count_files(&cache, &account), 1);
        cache.mark_clean().unwrap();
    }
    let cache = open(&path, &account);
    assert!(cache.status(&account).unwrap().startup_unclean);
    assert_eq!(count_files(&cache, &account), 0);
    assert!(!cache.disk.has_staged_blobs().unwrap());
}

#[test]
fn publication_survives_a_post_commit_health_failure_and_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&dir.path().join("cache"), &account);
    observe(&cache, &account);
    let reference = message(&account, 7);
    cache.fail_post_commit_check.store(true, Ordering::Relaxed);
    let info = publish(&cache, reference, ArtifactKey::Raw, b"raw");
    assert_eq!(
        cache.artifact_state(reference, ArtifactKey::Raw).unwrap(),
        ArtifactState::Materialized(info)
    );
    assert_eq!(count_files(&cache, &account), 1);
    assert!(cache.offline_scope_allowed(&account, INBOX, 9).is_err());
    let mut lease = cache
        .open_blob(
            reference,
            ArtifactKey::Raw,
            Access::LiveVerified,
            Some(info.id),
        )
        .unwrap();
    assert_eq!(bytes(&mut lease), b"raw");
}
