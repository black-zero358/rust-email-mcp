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
