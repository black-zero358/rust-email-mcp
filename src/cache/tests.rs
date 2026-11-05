use super::{
    cursor::{CursorBinding, CursorError, CursorExpected, CursorScope, CursorWrite},
    *,
};

const MIB: u64 = 1024 * 1024;
const MAILBOX: &str = "收件箱";

fn open(path: &Path, accounts: &[AccountKey]) -> Arc<Cache> {
    Cache::open(path, 32 * MIB, scopes(accounts, 16 * MIB), &[]).unwrap()
}

fn scopes(accounts: &[AccountKey], max_bytes: u64) -> Vec<AccountScope> {
    accounts
        .iter()
        .map(|account| AccountScope {
            account: account.clone(),
            mailboxes: vec![MAILBOX.into()],
            max_bytes,
        })
        .collect()
}

fn message(account: &AccountKey, uid_validity: u32, uid: u32) -> Ref<'_> {
    Ref {
        account,
        mailbox: MAILBOX,
        uid_validity,
        uid,
    }
}

fn binding(account: &AccountKey, principal: u8) -> CursorBinding {
    CursorBinding {
        namespace: account.reference_namespace(),
        principal: [principal; 32],
    }
}

fn new_cursor(binding: CursorBinding, token: [u8; 16]) -> CursorWrite<'static> {
    CursorWrite {
        binding,
        expected: None,
        active: Some(token),
        replay: None,
        codec_version: 1,
        owned_bytes: 128,
        encoded_len: 6,
        expires_ms: 600_100,
        last_progress_ms: 100,
        now_ms: 100,
        frozen: &[CursorScope {
            mailbox: MAILBOX,
            uid_validity: 9,
        }],
        cancelled: &|| false,
    }
}

fn payload(output: &mut dyn std::io::Write) -> Result<(), CursorError> {
    output
        .write_all(b"cursor")
        .map_err(|_| CursorError::Storage)
}

#[test]
fn metadata_and_parts_respect_account_generation_tombstones_and_clean_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache");
    let a = AccountKey::synthetic();
    let b = AccountKey::synthetic();
    let accounts = [a.clone(), b.clone()];
    let cache = open(&path, &accounts);
    for (account, header) in [(&a, b"one"), (&b, b"two")] {
        cache
            .observe_folder(account, MAILBOX, 9, Some(12), 100)
            .unwrap();
        cache
            .remember_headers(message(account, 9, 7), true, header, Some("同名目录"))
            .unwrap();
    }
    cache
        .remember_headers(message(&a, 9, 7), false, b"partial", None)
        .unwrap();
    cache
        .remember_parts(message(&a, 9, 7), b"verified")
        .unwrap();
    assert_eq!(
        cache.header(message(&a, 9, 7)).unwrap().as_deref(),
        Some(&b"one"[..])
    );
    assert_eq!(
        cache.header(message(&b, 9, 7)).unwrap().as_deref(),
        Some(&b"two"[..])
    );
    assert!(cache.parts(message(&b, 9, 7)).unwrap().is_none());
    cache.tombstone(message(&a, 9, 7)).unwrap();
    assert!(cache.header(message(&a, 9, 7)).unwrap().is_none());
    assert!(cache.parts(message(&a, 9, 7)).unwrap().is_none());
    assert!(matches!(
        cache.remember_headers(message(&a, 9, 7), true, b"old", None),
        Err(CacheError::OutOfScope)
    ));
    cache.mark_clean().unwrap();
    drop(cache);

    let cache = open(&path, &accounts);
    assert!(!cache.status(&a).unwrap().startup_unclean);
    assert_eq!(cache.status(&b).unwrap().folders[0].indexed_headers, 1);
    assert!(cache.header(message(&a, 9, 7)).unwrap().is_none());
    cache.observe_folder(&b, MAILBOX, 11, None, 200).unwrap();
    assert!(cache.header(message(&b, 9, 7)).unwrap().is_none());
    assert!(matches!(
        cache.observe_folder(&b, MAILBOX, 9, Some(12), 250),
        Err(CacheError::OutOfScope)
    ));
    cache
        .remember_headers(message(&b, 11, 7), true, b"new", None)
        .unwrap();
    assert_eq!(
        cache.header(message(&b, 11, 7)).unwrap().as_deref(),
        Some(&b"new"[..])
    );
    assert!(cache.status(&b).unwrap().folders[0].dirty);
}

#[test]
fn bundled_trigram_handles_chinese_and_short_scan_resumes_past_empty_page() {
    let directory = tempfile::tempdir().unwrap();
    let a = AccountKey::synthetic();
    let cache = open(&directory.path().join("cache"), std::slice::from_ref(&a));
    cache
        .observe_folder(&a, MAILBOX, 9, Some(202), 100)
        .unwrap();
    for uid in 1..=202 {
        let text = if uid == 1 {
            "办公室会议议程"
        } else {
            "其他项目"
        };
        cache
            .remember_headers(message(&a, 9, uid), true, b"header", Some(text))
            .unwrap();
    }
    let trigram = cache
        .search_metadata(&a, MAILBOX, 9, "会议议", None, 10)
        .unwrap();
    assert_eq!(trigram.indexed, 202);
    assert_eq!(trigram.pending, 0);
    assert_eq!(
        trigram.hits,
        [SearchHit {
            uid_validity: 9,
            uid: 1,
            matched: "metadata"
        }]
    );
    let first = cache
        .search_metadata(&a, MAILBOX, 9, "会议", None, 10)
        .unwrap();
    assert!(first.hits.is_empty());
    assert!(first.next_before_row.is_some());
    let next = cache
        .search_metadata(&a, MAILBOX, 9, "会议", first.next_before_row, 10)
        .unwrap();
    assert_eq!(next.hits, trigram.hits);
    assert!(next.next_before_row.is_none());
    cache.tombstone(message(&a, 9, 1)).unwrap();
    assert!(
        cache
            .search_metadata(&a, MAILBOX, 9, "会议议", None, 10)
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn trigram_posting_scan_is_bounded_before_folder_filtering() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let other = "Archive";
    let cache = Cache::open(
        &directory.path().join("cache"),
        32 * MIB,
        vec![AccountScope {
            account: account.clone(),
            mailboxes: vec![MAILBOX.into(), other.into()],
            max_bytes: 16 * MIB,
        }],
        &[],
    )
    .unwrap();
    let text = Some("中文检索");
    cache
        .observe_folder(&account, MAILBOX, 9, Some(1), 100)
        .unwrap();
    cache
        .remember_headers(message(&account, 9, 1), true, b"target", text)
        .unwrap();
    cache
        .observe_folder(&account, other, 9, Some(200), 100)
        .unwrap();
    for uid in 1..=200 {
        cache
            .remember_headers(
                Ref {
                    account: &account,
                    mailbox: other,
                    uid_validity: 9,
                    uid,
                },
                true,
                b"other folder",
                text,
            )
            .unwrap();
    }
    let first = cache
        .search_metadata(&account, MAILBOX, 9, "中文检", None, 1)
        .unwrap();
    assert!(first.hits.is_empty());
    assert_eq!((first.indexed, first.pending), (1, 0));
    assert!(first.next_before_row.is_some());
    let second = cache
        .search_metadata(&account, MAILBOX, 9, "中文检", first.next_before_row, 1)
        .unwrap();
    assert_eq!(
        second.hits,
        [SearchHit {
            uid_validity: 9,
            uid: 1,
            matched: "metadata"
        }]
    );
    assert!(second.next_before_row.is_none());
}

#[test]
fn persisted_cursor_stream_survives_unclean_restart_and_cas_replay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache");
    let a = AccountKey::synthetic();
    let key = binding(&a, 1);
    let account = std::slice::from_ref(&a);
    let token = [1; 16];
    let next = [2; 16];
    let cache = open(&path, account);
    cache.observe_folder(&a, MAILBOX, 9, Some(7), 100).unwrap();
    cache
        .cursor_backend()
        .save(new_cursor(key, token), payload)
        .unwrap();
    drop(cache);

    let cache = open(&path, account);
    assert!(cache.status(&a).unwrap().folders[0].dirty);
    let backend = cache.cursor_backend();
    assert_eq!(backend.reservations(100).unwrap().len(), 1);
    let value = backend
        .load(key, token, 100, |row, input| {
            assert_eq!(row.revision, 1);
            let mut payload = [0; 6];
            input
                .read_exact(&mut payload)
                .map_err(|_| CursorError::Corrupt)?;
            Ok(payload)
        })
        .unwrap();
    assert_eq!(&value, b"cursor");
    assert_eq!(
        backend.load(
            binding(&a, 2),
            token,
            100,
            |_, _| -> Result<(), CursorError> { panic!() }
        ),
        Err(CursorError::Unavailable)
    );
    assert_eq!(
        backend.validate(key, token, 600_100),
        Err(CursorError::Unavailable)
    );

    let mut write = new_cursor(key, next);
    write.expected = Some(CursorExpected { token, revision: 1 });
    write.replay = Some(token);
    backend.save(write, payload).unwrap();
    let row = backend.validate(key, next, 100).unwrap();
    assert_eq!(row, backend.validate(key, token, 100).unwrap());
    assert_eq!(row.revision, 2);
    assert_eq!(
        backend.invalidate(key, token, 1),
        Err(CursorError::Conflict)
    );
    cache.observe_folder(&a, MAILBOX, 10, None, 200).unwrap();
    assert_eq!(
        backend.validate(key, token, 200),
        Err(CursorError::Unavailable)
    );
    assert_eq!(
        backend.validate(key, next, 200),
        Err(CursorError::Unavailable)
    );
}

#[test]
fn cursor_bad_length_digest_and_cancellation_leave_old_checkpoint() {
    use std::cell::Cell;
    let directory = tempfile::tempdir().unwrap();
    let a = AccountKey::synthetic();
    let cache = open(&directory.path().join("cache"), std::slice::from_ref(&a));
    let backend = cache.cursor_backend();
    let key = binding(&a, 1);
    let token = [1; 16];
    backend.save(new_cursor(key, token), payload).unwrap();
    let next = [2; 16];
    let replacement = || {
        let mut write = new_cursor(key, next);
        write.expected = Some(CursorExpected { token, revision: 1 });
        write.replay = Some(token);
        write
    };
    assert_eq!(
        backend.save(replacement(), |_| Ok(())),
        Err(CursorError::Corrupt)
    );
    let cancelled = Cell::new(false);
    let cancellation = || cancelled.get();
    let mut write = replacement();
    write.cancelled = &cancellation;
    assert_eq!(
        backend.save(write, |output| {
            payload(output)?;
            cancelled.set(true);
            Ok(())
        }),
        Err(CursorError::Unavailable)
    );
    assert_eq!(backend.validate(key, token, 100).unwrap().revision, 1);
    assert_eq!(
        backend.validate(key, next, 100),
        Err(CursorError::Unavailable)
    );

    {
        let dbs = cache.lock().unwrap();
        let db = &dbs[&key.namespace].connection;
        db.execute(
            "UPDATE conversation_cursors SET payload=x'000000000000'",
            [],
        )
        .unwrap();
    }
    assert_eq!(
        backend.load(key, token, 100, |_, input| {
            input
                .read_exact(&mut [0; 6])
                .map_err(|_| CursorError::Corrupt)
        }),
        Err(CursorError::Corrupt)
    );
}

#[test]
fn cursor_commit_remains_replayable_after_a_post_commit_health_failure() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(
        &directory.path().join("cache"),
        std::slice::from_ref(&account),
    );
    let key = binding(&account, 1);
    let token = [1; 16];
    let next = [2; 16];
    let backend = cache.cursor_backend();
    backend.save(new_cursor(key, token), payload).unwrap();
    let mut write = new_cursor(key, next);
    write.expected = Some(CursorExpected { token, revision: 1 });
    write.replay = Some(token);
    cache.fail_post_commit_check.store(true, Ordering::Relaxed);
    backend.save(write, payload).unwrap();
    let row = backend
        .load(key, token, 100, |row, input| {
            let mut bytes = [0; 6];
            input
                .read_exact(&mut bytes)
                .map_err(|_| CursorError::Corrupt)?;
            assert_eq!(&bytes, b"cursor");
            Ok(row)
        })
        .unwrap();
    assert_eq!(row.revision, 2);
    assert_eq!(row.active, Some(next));
    assert_eq!(row.replay, Some(token));
    assert_eq!(backend.validate(key, next, 100).unwrap(), row);
    assert!(matches!(cache.status(&account), Err(CacheError::Storage)));
    assert_eq!(
        backend.save(new_cursor(key, [3; 16]), payload),
        Err(CursorError::Storage)
    );
}

#[test]
fn cursor_quota_is_shared_across_account_partitions_and_restore_clears_cursors() {
    let directory = tempfile::tempdir().unwrap();
    let a = AccountKey::synthetic();
    let b = AccountKey::synthetic();
    let cache = open(&directory.path().join("cache"), &[a.clone(), b.clone()]);
    let backend = cache.cursor_backend();
    let first = binding(&a, 1);
    let second = binding(&b, 1);
    cache.observe_folder(&a, MAILBOX, 9, Some(8), 100).unwrap();
    let mut write = new_cursor(first, [1; 16]);
    write.owned_bytes = 8 * MIB as usize;
    backend.save(write, payload).unwrap();
    assert_eq!(
        backend.save(new_cursor(second, [2; 16]), payload),
        Err(CursorError::Quota)
    );
    cache.mark_restored().unwrap();
    assert!(backend.reservations(100).unwrap().is_empty());
    assert!(cache.status(&a).unwrap().folders[0].dirty);
    backend.save(new_cursor(second, [2; 16]), payload).unwrap();
}

#[test]
fn private_root_rejects_overlap_symlinks_unknown_entries_and_second_owner() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let operations = root.join("operations.sqlite");
    let account = AccountKey::synthetic();
    let set = std::slice::from_ref(&account);
    assert!(matches!(
        Cache::open(&root, 32 * MIB, scopes(set, 16 * MIB), &[operations]),
        Err(CacheError::OutOfScope)
    ));
    assert!(!root.exists());
    let cache = open(&root, set);
    assert!(Cache::open(&root, 32 * MIB, scopes(set, 16 * MIB), &[]).is_err());
    drop(cache);
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    assert!(Cache::open(&link, 32 * MIB, scopes(set, 16 * MIB), &[]).is_err());
    let foreign = root.join("unknown.txt");
    std::fs::write(&foreign, b"keep").unwrap();
    assert!(Cache::open(&root, 32 * MIB, scopes(set, 16 * MIB), &[]).is_err());
    assert_eq!(std::fs::read(foreign).unwrap(), b"keep");
}

#[test]
fn a_namespace_shaped_partition_without_an_owner_manifest_is_never_adopted() {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cache");
    let account = AccountKey::synthetic();
    let name = disk::namespace_name(&account.reference_namespace());
    let partition = root.join(name);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&partition)
        .unwrap();
    let file = partition.join("cache.sqlite");
    let mut db = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&file)
        .unwrap();
    std::io::Write::write_all(&mut db, b"unowned").unwrap();
    assert!(matches!(
        Cache::open(&root, 32 * MIB, scopes(&[account], 16 * MIB), &[]),
        Err(CacheError::Corrupt)
    ));
    assert_eq!(std::fs::read(file).unwrap(), b"unowned");
    assert!(!root.join("manifest.json").exists());
}

#[test]
fn restore_marker_invalidates_previously_inactive_partitions_on_later_open() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache");
    let a = AccountKey::synthetic();
    let b = AccountKey::synthetic();
    let token = [1; 16];
    let key = binding(&b, 1);
    {
        let cache = open(&path, &[a.clone(), b.clone()]);
        cache.observe_folder(&b, MAILBOX, 9, Some(8), 100).unwrap();
        cache
            .cursor_backend()
            .save(new_cursor(key, token), payload)
            .unwrap();
        cache.mark_clean().unwrap();
    }
    {
        let cache = open(&path, std::slice::from_ref(&a));
        cache.mark_restored().unwrap();
        cache.mark_clean().unwrap();
    }
    let cache = open(&path, &[a, b.clone()]);
    assert!(cache.status(&b).unwrap().folders[0].dirty);
    assert_eq!(
        cache.cursor_backend().validate(key, token, 100),
        Err(CursorError::Unavailable)
    );
}

#[test]
fn global_cursor_slot_cap_counts_both_accounts_and_releases_expired_slots() {
    let directory = tempfile::tempdir().unwrap();
    let a = AccountKey::synthetic();
    let b = AccountKey::synthetic();
    let cache = open(&directory.path().join("cache"), &[a.clone(), b.clone()]);
    let backend = cache.cursor_backend();
    for index in 1..=64 {
        let key = binding(if index % 2 == 0 { &a } else { &b }, 1);
        backend.save(new_cursor(key, [index; 16]), payload).unwrap();
    }
    let key = binding(&a, 1);
    let mut write = new_cursor(key, [65; 16]);
    assert_eq!(backend.save(write, payload), Err(CursorError::Quota));
    write = new_cursor(key, [65; 16]);
    write.now_ms = 600_100;
    write.last_progress_ms = 600_100;
    write.expires_ms = 1_200_100;
    backend.save(write, payload).unwrap();
    assert_eq!(backend.reservations(600_100).unwrap().len(), 1);
}

#[test]
fn first_mailbox_observation_invalidates_persisted_different_generation() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(
        &directory.path().join("cache"),
        std::slice::from_ref(&account),
    );
    let key = binding(&account, 1);
    let token = [1; 16];
    let backend = cache.cursor_backend();
    backend.save(new_cursor(key, token), payload).unwrap();
    cache
        .observe_folder(&account, MAILBOX, 10, None, 200)
        .unwrap();
    assert_eq!(
        backend.validate(key, token, 200),
        Err(CursorError::Unavailable)
    );
    assert!(backend.reservations(200).unwrap().is_empty());
}

#[test]
fn a_late_first_observation_cannot_replace_a_newer_mailbox_generation() {
    let directory = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(
        &directory.path().join("cache"),
        std::slice::from_ref(&account),
    );
    cache
        .observe_folder(&account, MAILBOX, 10, Some(8), 100)
        .unwrap();
    let reference = message(&account, 10, 7);
    cache
        .remember_headers(reference, true, b"new", None)
        .unwrap();
    let key = binding(&account, 1);
    let token = [1; 16];
    let backend = cache.cursor_backend();
    let mut write = new_cursor(key, token);
    let scopes = [CursorScope {
        mailbox: MAILBOX,
        uid_validity: 10,
    }];
    write.frozen = &scopes;
    backend.save(write, payload).unwrap();

    assert!(matches!(
        cache.observe_folder(&account, MAILBOX, 9, Some(30), 200),
        Err(CacheError::OutOfScope)
    ));
    let status = cache.status(&account).unwrap();
    assert_eq!(status.folders[0].uid_validity, Some(10));
    assert_eq!(status.folders[0].high_uid, Some(8));
    assert_eq!(status.folders[0].checked_at_ms, Some(100));
    assert_eq!(
        cache.header(reference).unwrap().as_deref(),
        Some(&b"new"[..])
    );
    assert!(backend.validate(key, token, 200).is_ok());

    // A greater generation remains authoritative after a wall-clock rollback.
    cache
        .observe_folder(&account, MAILBOX, 11, None, 99)
        .unwrap();
    let status = cache.status(&account).unwrap();
    assert_eq!(status.folders[0].uid_validity, Some(11));
    assert_eq!(status.folders[0].checked_at_ms, Some(99));
    assert!(cache.header(reference).unwrap().is_none());
    assert_eq!(
        backend.validate(key, token, 200),
        Err(CursorError::Unavailable)
    );
}

#[test]
fn account_and_global_disk_reservation_reject_large_transactions() {
    let directory = tempfile::tempdir().unwrap();
    let a = AccountKey::synthetic();
    let b = AccountKey::synthetic();
    let cache = Cache::open(
        &directory.path().join("cache"),
        2 * MIB,
        scopes(&[a.clone(), b.clone()], MIB),
        &[],
    )
    .unwrap();
    let backend = cache.cursor_backend();
    let key = binding(&a, 1);
    let mut write = new_cursor(key, [1; 16]);
    write.encoded_len = MIB as usize;
    assert_eq!(
        backend.save(write, |_| panic!("quota should fail before streaming")),
        Err(CursorError::Quota)
    );
    assert!(backend.reservations(100).unwrap().is_empty());
    let status = cache.status(&a).unwrap();
    assert!(status.used_bytes <= status.max_bytes);
    assert!(status.global_used_bytes <= status.global_max_bytes);
}

#[test]
fn global_disk_reservation_charges_a_retired_partitions_database() {
    fn save(
        backend: &cursor::CursorBackend,
        key: CursorBinding,
        pages: usize,
    ) -> Result<(), CursorError> {
        let mut write = new_cursor(key, [1; 16]);
        write.encoded_len = pages * 4096;
        backend.save(write, |output| {
            for _ in 0..pages {
                output
                    .write_all(&[b'x'; 4096])
                    .map_err(|_| CursorError::Storage)?;
            }
            Ok(())
        })
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache");
    let a = AccountKey::synthetic();
    let b = AccountKey::synthetic();
    {
        let cache = Cache::open(&path, MIB, scopes(std::slice::from_ref(&a), MIB), &[]).unwrap();
        save(&cache.cursor_backend(), binding(&a, 1), 60).unwrap();
        cache.mark_clean().unwrap();
    }
    let cache = Cache::open(&path, MIB, scopes(std::slice::from_ref(&b), MIB), &[]).unwrap();
    assert_eq!(
        save(&cache.cursor_backend(), binding(&b, 1), 70),
        Err(CursorError::Quota)
    );
    let status = cache.status(&b).unwrap();
    assert!(status.global_used_bytes > status.used_bytes);
    let isolated = Cache::open(
        &directory.path().join("other"),
        MIB,
        scopes(std::slice::from_ref(&b), MIB),
        &[],
    )
    .unwrap();
    save(&isolated.cursor_backend(), binding(&b, 1), 70).unwrap();
}

#[test]
fn unsupported_schema_version_is_preserved_for_explicit_rebuild() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache");
    let a = AccountKey::synthetic();
    let account = std::slice::from_ref(&a);
    {
        let cache = open(&path, account);
        cache.lock().unwrap()[&a.reference_namespace()]
            .connection
            .pragma_update(None, "user_version", 99)
            .unwrap();
    }
    assert!(matches!(
        Cache::open(&path, 32 * MIB, scopes(account, 16 * MIB), &[]),
        Err(CacheError::Corrupt)
    ));
    let file = path
        .join(disk::namespace_name(&a.reference_namespace()))
        .join("cache.sqlite");
    let db =
        rusqlite::Connection::open_with_flags(file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let version: u32 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 99);
}
