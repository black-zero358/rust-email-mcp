use super::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn environment() -> BTreeMap<String, OsString> {
    [
        ("EMAIL_ADDRESS", "owner@example.test"),
        ("EMAIL_PASSWORD", "synthetic-old-secret"),
        ("IMAP_HOST", "imap.example.test"),
        ("SMTP_HOST", "smtp.example.test"),
        ("MCP_RESOURCE", "https://mail.example.test/mcp"),
        ("OIDC_ISSUER", "https://auth.example.test/"),
        ("OIDC_AUDIENCE", "https://mail.example.test/mcp"),
        ("OIDC_SUBJECTS", "owner,new-reader,journal-only"),
        ("EXTRA_PASSWORD", "synthetic-new-secret"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect()
}
fn extra() -> Value {
    json!({"registration_key":"finance-internal","id":"finance","display_name":"财务","address":"finance@example.test","imap":{"host":"imap2.example.test"},"password":{"env":"EXTRA_PASSWORD"},"read_subjects":["new-reader"]})
}
fn document() -> Value {
    json!({"version":1,"mode":"extend_legacy","default_account_id":"finance","legacy_default":{"display_name":"原邮箱","read_subjects":["owner"],"write_subjects":["owner"]},"accounts":[extra()]})
}
fn load_document(
    document: &Value,
    environment: &BTreeMap<String, OsString>,
) -> anyhow::Result<ValidatedAccountSet> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("accounts.json");
    fs::write(&path, serde_json::to_vec(document)?)?;
    let mut environment = environment.clone();
    environment.insert("ACCOUNT_CONFIG_PATH".into(), path.into_os_string());
    load_checked(&|key| environment.get(key).cloned())
}

#[test]
fn strict_json_extends_legacy_without_granting_new_global_subjects_old_mail_access() {
    let set = load_document(&document(), &environment()).unwrap();
    assert_eq!(set.source, AccountConfigSource::ExtendLegacy);
    assert_eq!(set.accounts.len(), 2);
    assert_eq!(set.default_read_account, "finance");
    assert_eq!(set.accounts[0].write_subjects, vec!["owner"]);
    assert_eq!(set.accounts[0].read_subjects, vec!["owner"]);
    assert!(set.service_config.subjects.contains(&"new-reader".into()));
    assert_eq!(set.accounts[1].config.mode, "read");
    assert!(set.accounts[1].write_subjects.is_empty());
    assert_eq!(set.service_config.address, "owner@example.test");
    assert_eq!(set.accounts[1].config.address, "finance@example.test");
}

#[test]
fn shipped_configuration_example_matches_strict_schema() {
    let document: Value =
        serde_json::from_str(include_str!("../../../deploy/accounts.json.example")).unwrap();
    let mut environment = environment();
    environment.insert(
        "FINANCE_MAIL_PASSWORD".into(),
        "synthetic-finance-password".into(),
    );
    let set = load_document(&document, &environment).unwrap();
    assert_eq!(set.accounts.len(), 2);
    assert_eq!(set.accounts[1].alias, "finance");
    assert_eq!(set.accounts[1].config.mode, "read");
}

#[test]
fn attachment_staging_path_is_a_shared_service_setting_and_does_not_block_config_loading() {
    let mut environment = environment();
    environment.insert(
        "ATTACHMENT_STAGING_DIR".into(),
        "./synthetic-downloads".into(),
    );
    let legacy = load_checked(&|key| environment.get(key).cloned()).unwrap();
    assert_eq!(
        legacy.service_config.attachment_staging_dir.as_deref(),
        Some(Path::new("./synthetic-downloads"))
    );
    let extended = load_document(&document(), &environment).unwrap();
    for account in &extended.accounts {
        assert_eq!(
            account.config.attachment_staging_dir,
            extended.service_config.attachment_staging_dir
        );
    }
    environment.insert("ATTACHMENT_STAGING_DIR".into(), OsString::new());
    let loaded = load_checked(&|key| environment.get(key).cloned()).unwrap();
    assert_eq!(
        loaded.service_config.attachment_staging_dir.as_deref(),
        Some(Path::new(""))
    );
}

#[test]
fn strict_json_rejects_unknown_fields_duplicates_acl_versions_and_raw_secrets_privately() {
    let mut invalid = vec![];
    let mut value = document();
    value["private-secret-key"] = json!("private-secret-value");
    invalid.push(value);
    let mut value = document();
    value["version"] = json!(2);
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["mode"] = json!("full");
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["password"] = json!("private-secret-value");
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["password"] = json!({"env":"EXTRA_PASSWORD","file":"secret"});
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["id"] = json!("default");
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["registration_key"] = json!("legacy-default");
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["read_subjects"] = json!(["new-reader", "new-reader"]);
    invalid.push(value);
    let mut value = document();
    value["legacy_default"]["read_subjects"] = json!([]);
    invalid.push(value);
    let mut value = document();
    value["default_account_id"] = json!("missing");
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["id"] = json!("含中文");
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["display_name"] = json!("中".repeat(129));
    invalid.push(value);
    let mut value = document();
    value["accounts"][0]["imap"]["host"] = json!("https://private-imap.example.test");
    invalid.push(value);
    for value in invalid {
        let error = load_document(&value, &environment())
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error, CONFIG_ERROR);
        assert!(!error.contains("private-secret") && !error.contains("example.test"));
    }
    let mut value = document();
    value["accounts"] = json!([extra(), extra()]);
    assert!(load_document(&value, &environment()).is_err());
    let mut bad_environment = environment();
    bad_environment.insert("MCP_RESOURCE".into(), "https://".into());
    assert!(load_document(&document(), &bad_environment).is_err());
    bad_environment = environment();
    bad_environment.insert(
        "SMTP_HOST".into(),
        "https://private-smtp.example.test".into(),
    );
    assert!(load_document(&document(), &bad_environment).is_err());
}

#[test]
fn json_count_and_file_bytes_are_bounded_before_registry_work() {
    let mut value = document();
    value["accounts"] = Value::Array(
        (0..16)
            .map(|index| {
                let mut account = extra();
                account["id"] = json!(format!("a{index}"));
                account["registration_key"] = json!(format!("r{index}"));
                account
            })
            .collect(),
    );
    assert!(load_document(&value, &environment()).is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("too-large.json");
    fs::write(&path, vec![b' '; MAX_ACCOUNT_CONFIG_BYTES + 1]).unwrap();
    let mut environment = environment();
    environment.insert("ACCOUNT_CONFIG_PATH".into(), path.into_os_string());
    assert!(load_checked(&|key| environment.get(key).cloned()).is_err());
}

#[test]
fn file_only_requires_anchor_and_refuses_legacy_environment_precedence() {
    let value = json!({"version":1,"mode":"file_only","default_account_id":"finance","legacy_default":{"display_name":"原邮箱","address":"owner@example.test","imap":{"host":"imap.example.test","port":993},"password":{"env":"PRIMARY_PASSWORD"},"smtp":{"host":"smtp.example.test","port":465,"starttls":false},"permission_mode":"full","read_subjects":["owner"],"write_subjects":["owner"]},"accounts":[extra()]});
    assert!(load_document(&value, &environment()).is_err());
    let mut environment = environment();
    for key in ["EMAIL_ADDRESS", "EMAIL_PASSWORD", "IMAP_HOST", "SMTP_HOST"] {
        environment.remove(key);
    }
    environment.insert("PRIMARY_PASSWORD".into(), "synthetic-primary".into());
    let set = load_document(&value, &environment).unwrap();
    assert_eq!(set.source, AccountConfigSource::FileOnly);
    assert_eq!(set.default_read_account, "finance");
    assert_eq!(set.service_config.address, "owner@example.test");
    assert_eq!(set.service_config.mode, "full");
    let mut no_smtp = value.clone();
    no_smtp["legacy_default"]["smtp"] = Value::Null;
    assert!(load_document(&no_smtp, &environment).is_err());
    no_smtp["legacy_default"]["permission_mode"] = json!("read");
    no_smtp["legacy_default"]["write_subjects"] = json!([]);
    let read_only = load_document(&no_smtp, &environment).unwrap();
    assert_eq!(read_only.service_config.mode, "read");
    assert!(read_only.service_config.smtp_host.is_empty());
    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("legacy_default");
    assert!(load_document(&missing, &environment).is_err());
}

#[test]
fn ordinary_legacy_environment_preserves_duplicate_subjects_and_prior_parse_boundaries() {
    let mut environment = environment();
    environment.insert("OIDC_SUBJECTS".into(), "owner,owner".into());
    environment.insert("EMAIL_ADDRESS".into(), "x".repeat(600).into());
    environment.insert("IMAP_HOST".into(), "h".repeat(300).into());
    let set = load_checked(&|key| environment.get(key).cloned()).unwrap();
    set.validate().unwrap();
    assert_eq!(set.source, AccountConfigSource::Legacy);
    assert_eq!(set.accounts[0].read_subjects, vec!["owner", "owner"]);
    assert_eq!(set.service_config.address.len(), 600);
}

#[cfg(unix)]
#[test]
fn secret_file_refs_are_private_bounded_and_cannot_escape_root() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("mail.secret");
    fs::write(&secret, b"synthetic-password\r\n").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let mut environment = environment();
    environment.insert("MAIL_SECRET_ROOT".into(), dir.path().as_os_str().to_owned());
    let lookup = |key: &str| environment.get(key).cloned();
    let from_file = |path: &str| {
        SecretRef::File(FileSecret {
            file: PathBuf::from(path),
        })
    };
    assert_eq!(
        resolve_secret(from_file("mail.secret"), &lookup).unwrap(),
        "synthetic-password"
    );
    assert!(resolve_secret(from_file("../mail.secret"), &lookup).is_err());
    assert!(resolve_secret(from_file(secret.to_str().unwrap()), &lookup).is_err());
    symlink(&secret, dir.path().join("link.secret")).unwrap();
    assert!(resolve_secret(from_file("link.secret"), &lookup).is_err());
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(resolve_secret(from_file("mail.secret"), &lookup).is_err());
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&secret, vec![b'x'; MAX_SECRET_BYTES + 1]).unwrap();
    assert!(resolve_secret(from_file("mail.secret"), &lookup).is_err());
    let nested = dir.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o777)).unwrap();
    let nested_secret = nested.join("mail.secret");
    fs::write(&nested_secret, b"synthetic-password").unwrap();
    fs::set_permissions(&nested_secret, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(resolve_secret(from_file("nested/mail.secret"), &lookup).is_err());
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(resolve_secret(from_file("nested/mail.secret"), &lookup).is_ok());
}

#[cfg(not(unix))]
#[test]
fn file_secret_refs_require_native_permission_validation_but_environment_refs_work() {
    let environment = environment();
    let lookup = |key: &str| environment.get(key).cloned();
    assert!(
        resolve_secret(
            SecretRef::File(FileSecret {
                file: "mail.secret".into()
            }),
            &lookup
        )
        .is_err()
    );
    assert_eq!(
        resolve_secret(
            SecretRef::Environment(EnvironmentSecret {
                env: "EXTRA_PASSWORD".into()
            }),
            &lookup
        )
        .unwrap(),
        "synthetic-new-secret"
    );
}
