use super::*;
use crate::config::ValidatedAccountConfig;

fn fixture(directory: &Path) -> ValidatedAccountSet {
    let mut config = (*crate::config::fixture()).clone();
    config.database = directory.join("operations.sqlite").to_str().unwrap().into();
    config.subjects = vec!["owner".into(), "reader".into(), "journal-only".into()];
    let mut set = ValidatedAccountSet::legacy(Arc::new(config));
    set.source = AccountConfigSource::ExtendLegacy;
    set.accounts[0].read_subjects = vec!["owner".into()];
    set.accounts[0].write_subjects = vec!["owner".into()];
    let mut extra = (*set.service_config).clone();
    extra.address = "second@example.test".into();
    extra.imap_host = "second.example.test".into();
    extra.mode = "read".into();
    set.accounts.push(ValidatedAccountConfig {
        registration_key: "second-stable-key".into(),
        alias: "second".into(),
        display_name: "第二邮箱".into(),
        config: Arc::new(extra),
        read_subjects: vec!["reader".into(), "owner".into()],
        write_subjects: vec![],
        is_legacy: false,
    });
    set
}

#[test]
fn catalog_authorization_intersects_global_scope_and_keeps_history_only_subject_valid() {
    let dir = tempfile::tempdir().unwrap();
    let mut set = fixture(dir.path());
    set.default_read_account = "second".into();
    set.accounts[1]
        .read_subjects
        .push("not-globally-admitted".into());
    let catalog = AccountCatalog::from_test_accounts(set).unwrap();
    assert_eq!(catalog.read_account("owner", None).unwrap().alias, "second");
    assert!(catalog.read_account("reader", Some("default")).is_err());
    assert!(
        catalog
            .read_account("not-globally-admitted", Some("second"))
            .is_err()
    );
    assert!(catalog.visible_accounts("journal-only").is_empty());
    let unknown = catalog
        .read_account("reader", Some("unknown-private-alias"))
        .err()
        .unwrap();
    let denied = catalog
        .read_account("reader", Some("default"))
        .err()
        .unwrap();
    assert_eq!(
        serde_json::to_value(unknown).unwrap(),
        serde_json::to_value(denied).unwrap()
    );
    assert_eq!(catalog.write_account("owner").unwrap().alias, "default");
    assert!(catalog.write_account("reader").is_err());
    let old = catalog.read_account("reader", Some("second")).unwrap();
    catalog.revoke_read_for_test("second", "reader").unwrap();
    assert!(old.cancellation.is_cancelled());
    assert!(catalog.read_key("reader", &old.key).is_err());
    assert!(catalog.read_key("owner", &old.key).is_ok());
    catalog.remove_account_for_test("second").unwrap();
    assert!(catalog.read_key("owner", &old.key).is_err());
}

#[test]
fn v1_migration_keeps_legacy_uuid_and_send_store_bytes_and_records_a_recovery_copy() {
    let dir = tempfile::tempdir().unwrap();
    let set = fixture(dir.path());
    let legacy = open_default(&set.service_config).unwrap();
    let registry = dir.path().join("accounts.sqlite");
    let old = fs::read(&registry).unwrap();
    fs::write(
        &set.service_config.database,
        b"synthetic untouched journal bytes",
    )
    .unwrap();
    let catalog = open_catalog(set.clone()).unwrap();
    assert_eq!(
        catalog.read_account("owner", Some("default")).unwrap().key,
        legacy
    );
    assert_eq!(fs::read(sidecar(&registry, ".v1-backup")).unwrap(), old);
    assert_eq!(
        fs::read(&set.service_config.database).unwrap(),
        b"synthetic untouched journal bytes"
    );
    let key = catalog.read_account("reader", Some("second")).unwrap().key;
    drop(catalog);
    let catalog = open_catalog(set.clone()).unwrap();
    assert_eq!(
        catalog.read_account("reader", Some("second")).unwrap().key,
        key
    );
    drop(catalog);
    assert!(open_catalog(ValidatedAccountSet::legacy(set.service_config.clone())).is_err());
}
