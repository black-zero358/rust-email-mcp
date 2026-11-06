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
