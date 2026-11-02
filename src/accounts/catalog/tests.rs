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

#[test]
fn labels_aliases_and_passwords_preserve_uuid_but_identity_and_readdition_advance_generation() {
    let dir = tempfile::tempdir().unwrap();
    let mut set = fixture(dir.path());
    let catalog = open_catalog(set.clone()).unwrap();
    let original = catalog.read_account("reader", Some("second")).unwrap().key;
    drop(catalog);
    set.accounts[1].alias = "renamed".into();
    set.accounts[1].display_name = "新显示名".into();
    let mut changed = (*set.accounts[1].config).clone();
    changed.password = "rotated-synthetic".into();
    changed.imap_host = changed.imap_host.to_uppercase();
    set.accounts[1].config = Arc::new(changed);
    let catalog = open_catalog(set.clone()).unwrap();
    assert_eq!(
        catalog.read_account("reader", Some("renamed")).unwrap().key,
        original
    );
    drop(catalog);
    let mut changed = (*set.accounts[1].config).clone();
    changed.imap_port += 1;
    set.accounts[1].config = Arc::new(changed);
    let catalog = open_catalog(set.clone()).unwrap();
    let next = catalog.read_account("reader", Some("renamed")).unwrap().key;
    assert_eq!(next.uuid, original.uuid);
    assert_eq!(next.generation, original.generation + 1);
    drop(catalog);
    let extra = set.accounts.pop().unwrap();
    let catalog = open_catalog(set.clone()).unwrap();
    assert!(catalog.read_key("reader", &next).is_err());
    drop(catalog);
    set.accounts.push(extra);
    let catalog = open_catalog(set).unwrap();
    let readded = catalog.read_account("reader", Some("renamed")).unwrap().key;
    assert_eq!(readded.uuid, original.uuid);
    assert_eq!(readded.generation, next.generation + 1);
    assert!(catalog.read_key("reader", &next).is_err());
}

#[test]
fn failed_migration_or_legacy_rebinding_retains_complete_registry_and_never_touches_journal() {
    let dir = tempfile::tempdir().unwrap();
    let set = fixture(dir.path());
    open_default(&set.service_config).unwrap();
    let registry = dir.path().join("accounts.sqlite");
    let old = fs::read(&registry).unwrap();
    fs::write(
        &set.service_config.database,
        b"existing synthetic send record",
    )
    .unwrap();
    assert!(open_inner(set.clone(), true).is_err());
    assert_eq!(fs::read(&registry).unwrap(), old);
    assert!(!sidecar(&registry, ".staging").exists());
    let mut changed = set.clone();
    let mut legacy = (*changed.service_config).clone();
    legacy.address = "different@example.test".into();
    changed.service_config = Arc::new(legacy);
    changed.accounts[0].config = changed.service_config.clone();
    assert_eq!(
        open_catalog(changed).err().unwrap().to_string(),
        BINDING_ERROR
    );
    assert_eq!(fs::read(&registry).unwrap(), old);
    assert_eq!(
        fs::read(&set.service_config.database).unwrap(),
        b"existing synthetic send record"
    );
    let catalog = open_catalog(set.clone()).unwrap();
    drop(catalog);
    let current = fs::read(&registry).unwrap();
    let mut changed = set;
    let mut extra = (*changed.accounts[1].config).clone();
    extra.imap_port += 1;
    changed.accounts[1].config = Arc::new(extra);
    assert!(open_inner(changed, true).is_err());
    assert_eq!(fs::read(&registry).unwrap(), current);
}

#[test]
fn failed_previous_copy_publication_cleans_staging_and_allows_complete_retry() {
    let dir = tempfile::tempdir().unwrap();
    let mut set = fixture(dir.path());
    drop(open_catalog(set.clone()).unwrap());
    let registry = dir.path().join("accounts.sqlite");
    let original = fs::read(&registry).unwrap();
    let previous = sidecar(&registry, ".previous");
    fs::create_dir(&previous).unwrap();
    let mut extra = (*set.accounts[1].config).clone();
    extra.imap_port += 1;
    set.accounts[1].config = Arc::new(extra);
    assert!(open_catalog(set.clone()).is_err());
    assert_eq!(fs::read(&registry).unwrap(), original);
    assert!(!sidecar(&registry, ".staging").exists());
    assert!(!sidecar(&registry, ".previous-staging").exists());
    fs::remove_dir(&previous).unwrap();
    drop(open_catalog(set).unwrap());
    assert_eq!(fs::read(previous).unwrap(), original);
}

#[test]
fn interrupted_files_are_cleaned_and_preexisting_bad_backups_preserve_the_registry() {
    let dir = tempfile::tempdir().unwrap();
    let set = fixture(dir.path());
    open_default(&set.service_config).unwrap();
    let registry = dir.path().join("accounts.sqlite");
    let original = fs::read(&registry).unwrap();
    let backup = sidecar(&registry, ".v1-backup");
    fs::write(&backup, vec![0; MAX_V2_BYTES as usize + 1]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).unwrap();
    }
    for suffix in [".staging", ".previous-staging"] {
        fs::write(sidecar(&registry, suffix), b"interrupted synthetic stage").unwrap();
    }
    assert!(open_catalog(set.clone()).is_err());
    assert_eq!(fs::read(&registry).unwrap(), original);
    for suffix in [".staging", ".previous-staging"] {
        assert!(!sidecar(&registry, suffix).exists());
    }
    fs::remove_file(backup).unwrap();
    drop(open_catalog(set.clone()).unwrap());
    let published = fs::read(&registry).unwrap();
    for suffix in [".staging", ".previous-staging"] {
        fs::write(sidecar(&registry, suffix), b"interrupted synthetic stage").unwrap();
    }
    drop(open_catalog(set).unwrap());
    assert_eq!(fs::read(registry.clone()).unwrap(), published);
    for suffix in [".staging", ".previous-staging"] {
        assert!(!sidecar(&registry, suffix).exists());
    }
}

#[test]
fn registry_does_not_store_credentials_or_raw_mailbox_identities() {
    let dir = tempfile::tempdir().unwrap();
    let set = fixture(dir.path());
    let catalog = open_catalog(set.clone()).unwrap();
    drop(catalog);
    let bytes = fs::read(dir.path().join("accounts.sqlite")).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for account in set.accounts {
        for value in [
            &account.config.address,
            &account.config.password,
            &account.config.imap_host,
            &account.config.database,
        ] {
            assert!(!text.contains(value));
        }
    }
}
