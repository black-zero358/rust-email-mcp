use super::*;

fn configured() -> (tempfile::TempDir, Config) {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::fixture().as_ref().clone();
    config.database = root
        .path()
        .join("operations.sqlite")
        .to_str()
        .unwrap()
        .into();
    config.account_registry = Some(root.path().join("accounts.sqlite"));
    super::super::open_default(&config).unwrap();
    (root, config)
}

fn registry(config: &Config) -> &Path {
    config.account_registry.as_deref().unwrap()
}

#[test]
fn each_managed_run_advances_the_witness_even_without_opening_cache() {
    let (_root, config) = configured();
    let one = CacheRun::begin(&config).unwrap();
    let first = one.identity();
    assert_eq!(first.previous, None);
    assert!(CacheRun::begin_inner(&config, Duration::ZERO).is_err());
    drop(one);

    // This run represents off or a cache initialization failure. It does
    // not open or modify the cache root but still advances the witness.
    let two = CacheRun::begin(&config).unwrap();
    let second = two.identity();
    assert_eq!(second.instance, first.instance);
    assert_eq!(second.previous, Some(first.current));
    assert_ne!(second.current, first.current);
    drop(two);

    let three = CacheRun::begin(&config).unwrap().identity();
    assert_eq!(three.previous, Some(second.current));
    assert_ne!(three.previous, Some(first.current));
    assert!(!Path::new(&config.database).exists());
}

#[test]
fn an_adjacent_clean_cache_opens_offline_but_a_failed_run_breaks_its_receipt() {
    use crate::cache::{AccountScope, Cache, CacheError};
    let (root, config) = configured();
    let account = super::super::open_default(&config).unwrap();
    let path = root.path().join("cache");
    let open = |run: &CacheRun| {
        Cache::open_full_with_run(
            &path,
            16 * 1024 * 1024,
            vec![AccountScope {
                account: account.clone(),
                mailboxes: vec!["INBOX".into()],
                max_bytes: 16 * 1024 * 1024,
            }],
            &[],
            run.identity(),
        )
    };
    {
        let run = CacheRun::begin(&config).unwrap();
        let cache = open(&run).unwrap();
        cache
            .observe_folder(&account, "INBOX", 9, Some(2), 100)
            .unwrap();
        cache.mark_clean().unwrap();
    }
    {
        let run = CacheRun::begin(&config).unwrap();
        let cache = open(&run).unwrap();
        cache.offline_scope_allowed(&account, "INBOX", 9).unwrap();
        cache.mark_clean().unwrap();
    }
    {
        let run = CacheRun::begin(&config).unwrap();
        let parked = root.path().join("cache-parked");
        fs::rename(&path, &parked).unwrap();
        fs::write(&path, b"cache path unavailable").unwrap();
        assert!(open(&run).is_err());
        fs::remove_file(&path).unwrap();
        fs::rename(parked, &path).unwrap();
    }
    let run = CacheRun::begin(&config).unwrap();
    let cache = open(&run).unwrap();
    assert!(matches!(
        cache.offline_scope_allowed(&account, "INBOX", 9),
        Err(CacheError::NeedsVerification)
    ));
}

#[test]
fn a_lost_or_relocated_witness_cannot_authorize_an_old_clean_receipt() {
    let (root, mut config) = configured();
    let first = CacheRun::begin(&config).unwrap().identity();
    let witness = sidecar(registry(&config), ".cache-run");
    let record = fs::read(&witness).unwrap();
    fs::remove_file(witness).unwrap();
    let replacement = CacheRun::begin(&config).unwrap().identity();
    assert_ne!(replacement.instance, first.instance);
    assert_eq!(replacement.previous, None);

    let relocated = root.path().join("relocated.sqlite");
    fs::copy(registry(&config), &relocated).unwrap();
    let copied_witness = sidecar(&relocated, ".cache-run");
    private_file_options()
        .create_new(true)
        .open(copied_witness)
        .unwrap()
        .write_all(&record)
        .unwrap();
    config.account_registry = Some(relocated);
    super::super::open_default(&config).unwrap();
    let moved = CacheRun::begin(&config).unwrap().identity();
    assert_ne!(moved.instance, first.instance);
    assert_eq!(moved.previous, None);
}

#[test]
fn a_private_staged_record_is_discarded_but_corrupt_published_state_is_preserved() {
    let (_root, config) = configured();
    let before = CacheRun::begin(&config).unwrap().identity();
    let staged = sidecar(registry(&config), ".cache-run.stage");
    private_file_options()
        .create_new(true)
        .open(&staged)
        .unwrap()
        .write_all(b"interrupted write")
        .unwrap();
    let next = CacheRun::begin(&config).unwrap().identity();
    assert_eq!(next.previous, Some(before.current));
    assert!(!staged.exists());

    let witness = sidecar(registry(&config), ".cache-run");
    let mut record = fs::read(&witness).unwrap();
    record[60] ^= 1;
    fs::write(&witness, &record).unwrap();
    assert_eq!(CacheRun::begin(&config).err().unwrap().to_string(), ERROR);
    assert_eq!(fs::read(witness).unwrap(), record);
    assert!(!Path::new(&config.database).exists());
}

#[cfg(unix)]
#[test]
fn witness_paths_reject_links_and_never_write_operation_files() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (root, mut config) = configured();
    let witness = sidecar(registry(&config), ".cache-run");
    let sentinel = root.path().join("untouched");
    fs::write(&sentinel, b"not a witness").unwrap();
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&sentinel, &witness).unwrap();
    assert!(CacheRun::begin(&config).is_err());
    fs::remove_file(&witness).unwrap();
    fs::hard_link(&sentinel, &witness).unwrap();
    assert!(CacheRun::begin(&config).is_err());
    assert_eq!(fs::read(&sentinel).unwrap(), b"not a witness");
    fs::remove_file(&witness).unwrap();

    config.database = witness.to_str().unwrap().into();
    assert!(CacheRun::begin(&config).is_err());
    assert!(!witness.exists());
}
