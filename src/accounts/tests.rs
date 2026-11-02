use super::*;

fn config(directory: &Path) -> Config {
    let mut config = (*crate::config::fixture()).clone();
    config.database = directory.join("operations.sqlite").to_str().unwrap().into();
    config.imap_host = "imap.example.test".into();
    config
}

#[test]
fn default_registry_persists_key_and_ignores_credentials_smtp_and_display_settings() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    let key = open_default(&config).unwrap();
    assert_eq!(key.generation, 1);
    assert!(!key.uuid.is_nil());
    config.password = "rotated-synthetic-secret".into();
    config.smtp_host = "different.example.test".into();
    config.smtp_port = 587;
    config.smtp_starttls = true;
    config.imap_host = "IMAP.EXAMPLE.TEST".into();
    config.drafts = "草稿".into();
    config.trash = "废纸篓".into();
    config.mode = "read".into();
    assert_eq!(open_default(&config).unwrap(), key);
    let registry = dir.path().join("accounts.sqlite");
    let raw = fs::read(&registry).unwrap();
    let raw = String::from_utf8_lossy(&raw);
    for secret in [
        &config.password,
        &config.address,
        &config.imap_host,
        &config.database,
    ] {
        assert!(!raw.contains(secret));
    }
    assert!(!sidecar(&registry, ".staging").exists());
    assert!(!sidecar(&registry, ".staging-journal").exists());
}

#[test]
fn legacy_identity_and_operation_path_changes_require_explicit_migration() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let key = open_default(&config).unwrap();
    for field in ["address", "imap_host", "imap_port", "database"] {
        let mut changed = config.clone();
        match field {
            "address" => changed.address = "another@example.test".into(),
            "imap_host" => changed.imap_host = "another.example.test".into(),
            "imap_port" => changed.imap_port = 1993,
            _ => changed.database = dir.path().join("different.sqlite").to_str().unwrap().into(),
        }
        assert_eq!(
            open_default(&changed).unwrap_err().to_string(),
            BINDING_ERROR,
            "{field}"
        );
        assert_eq!(open_default(&config).unwrap(), key);
    }
}

#[test]
fn memory_journal_keeps_a_persistent_key_and_cannot_rebind_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.account_registry = Some(dir.path().join("memory-accounts.sqlite"));
    let persistent = config.clone();
    config.database = ":memory:".into();
    let key = open_default(&config).unwrap();
    assert_eq!(open_default(&config).unwrap(), key);
    assert_eq!(
        open_default(&persistent).unwrap_err().to_string(),
        BINDING_ERROR
    );
    let second = tempfile::tempdir().unwrap();
    let mut other = self::config(second.path());
    other.account_registry = Some(second.path().join("accounts.sqlite"));
    open_default(&other).unwrap();
    other.database = ":memory:".into();
    assert_eq!(open_default(&other).unwrap_err().to_string(), BINDING_ERROR);
}

#[test]
fn concurrent_initializers_share_one_identity_and_leave_no_stage() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    fs::write(&config.database, b"existing synthetic operations file").unwrap();
    let gate = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let gate = gate.clone();
            let config = config.clone();
            std::thread::spawn(move || {
                gate.wait();
                open_default(&config).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let keys = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert!(keys.iter().all(|key| key == &keys[0]));
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        3,
        "only the operation file, accounts.sqlite and its persistent lock remain"
    );
}

#[test]
fn interrupted_stage_is_recovered_under_lock_without_replacing_published_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let registry = dir.path().join("accounts.sqlite");
    fs::write(sidecar(&registry, ".staging"), b"interrupted sqlite").unwrap();
    fs::write(
        sidecar(&registry, ".staging-journal"),
        b"interrupted journal",
    )
    .unwrap();
    let key = open_default(&config).unwrap();
    fs::hard_link(&registry, sidecar(&registry, ".staging")).unwrap();
    assert_eq!(open_default(&config).unwrap(), key);
    assert!(!sidecar(&registry, ".staging").exists());
    assert!(!sidecar(&registry, ".staging-journal").exists());
}

#[test]
fn existing_empty_corrupt_and_unknown_registries_are_never_reinitialized() {
    for kind in [
        "empty",
        "corrupt",
        "unknown",
        "unknown_layout",
        "missing_identity",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let registry = dir.path().join("accounts.sqlite");
        if kind == "empty" || kind == "corrupt" {
            let mut file = private_file_options()
                .create_new(true)
                .open(&registry)
                .unwrap();
            if kind == "corrupt" {
                use std::io::Write;
                file.write_all(b"private invalid registry").unwrap();
            }
        } else {
            open_default(&config).unwrap();
            let db = Connection::open(&registry).unwrap();
            db.execute_batch(match kind {
                "unknown" => "PRAGMA user_version=9",
                "unknown_layout" => "CREATE TABLE sqliteXextra (value TEXT)",
                _ => "DELETE FROM legacy_default",
            })
            .unwrap();
        }
        let before = fs::read(&registry).unwrap();
        assert_eq!(
            open_default(&config).unwrap_err().to_string(),
            REGISTRY_ERROR,
            "{kind}"
        );
        assert_eq!(fs::read(&registry).unwrap(), before, "{kind}");
    }
}

#[test]
fn active_initializer_stage_is_not_cleaned_before_its_lock_is_released() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let key = open_default(&config).unwrap();
    let registry = dir.path().join("accounts.sqlite");
    let lock = private_file_options()
        .open(sidecar(&registry, ".lock"))
        .unwrap();
    lock.lock().unwrap();
    let stage = sidecar(&registry, ".staging");
    fs::write(&stage, b"active initializer").unwrap();
    let handle = std::thread::spawn(move || open_default(&config).unwrap());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(fs::read(&stage).unwrap(), b"active initializer");
    drop(lock);
    assert_eq!(handle.join().unwrap(), key);
    assert!(!stage.exists());
}

#[test]
fn registry_and_its_work_files_cannot_alias_the_operation_database() {
    for suffix in ["", "-wal", ".lock", ".staging", ".staging-journal"] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        let registry = dir.path().join("accounts.sqlite");
        config.account_registry = Some(registry.clone());
        config.database = sidecar(&registry, suffix).to_str().unwrap().into();
        assert_eq!(
            open_default(&config).unwrap_err().to_string(),
            REGISTRY_ERROR,
            "{suffix}"
        );
        assert!(!registry.exists());
    }
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        fs::write(&config.database, b"synthetic operation database").unwrap();
        let alias = dir.path().join("alias.sqlite");
        fs::hard_link(&config.database, &alias).unwrap();
        config.account_registry = Some(alias);
        assert_eq!(
            open_default(&config).unwrap_err().to_string(),
            REGISTRY_ERROR
        );
        assert_eq!(
            fs::read(&config.database).unwrap(),
            b"synthetic operation database"
        );
    }
}

#[cfg(unix)]
#[test]
fn registry_files_and_new_directories_are_private_and_permissive_files_are_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir.path().join("new/data"));
    open_default(&config).unwrap();
    let registry = Path::new(&config.database)
        .parent()
        .unwrap()
        .join("accounts.sqlite");
    assert_eq!(
        fs::metadata(&registry).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(registry.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        open_default(&config).unwrap_err().to_string(),
        REGISTRY_ERROR
    );
}

#[test]
fn binding_failure_leaves_pending_send_state_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    open_default(&config).unwrap();
    let journal = crate::journal::Journal::open(Path::new(&config.database)).unwrap();
    journal
        .claim("synthetic-owner", "key", "fingerprint", "id")
        .unwrap();
    drop(journal);
    config.address = "another@example.test".into();
    assert_eq!(
        open_default(&config).unwrap_err().to_string(),
        BINDING_ERROR
    );
    let db = Connection::open(&config.database).unwrap();
    let status: String = db
        .query_row("SELECT status FROM operations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(status, "sending");
}
