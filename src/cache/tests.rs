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
