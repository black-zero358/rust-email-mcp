use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

#[path = "config/account_config.rs"]
mod account_config;
pub use account_config::{
    AccountConfigSource, ValidatedAccountConfig, ValidatedAccountSet, load_runtime,
};

pub const DEFAULT_MAX_MESSAGE: usize = 32 * 1024 * 1024;
pub const DEFAULT_MAX_OUTPUT: usize = 2 * 1024 * 1024;
pub const MAX_DISPLAY_BODY: usize = 512 * 1024;

#[derive(Clone)]
pub struct Config {
    pub address: String,
    pub password: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_starttls: bool,
    pub drafts: String,
    pub trash: String,
    pub mode: String,
    pub timeout: Duration,
    pub max_message: usize,
    pub max_output: usize,
    pub resource: String,
    pub issuer: String,
    pub audience: String,
    pub subjects: Vec<String>,
    pub scope: String,
    pub bind: String,
    pub database: String,
    pub account_registry: Option<PathBuf>,
}

#[cfg(test)]
pub(crate) fn fixture() -> Arc<Config> {
    Arc::new(Config {
        address: "owner@example.com".into(),
        password: "test-only".into(),
        imap_host: "127.0.0.1".into(),
        imap_port: 993,
        smtp_host: "127.0.0.1".into(),
        smtp_port: 465,
        smtp_starttls: false,
        drafts: "Drafts".into(),
        trash: "Trash".into(),
        mode: "full".into(),
        timeout: Duration::from_secs(2),
        max_message: 8 * 1024 * 1024,
        max_output: 256 * 1024,
        resource: "https://mail.example.com/mcp".into(),
        issuer: "https://auth.example.com/".into(),
        audience: "https://mail.example.com/mcp".into(),
        subjects: vec!["owner".into()],
        scope: "email:mcp".into(),
        bind: "127.0.0.1:0".into(),
        database: ":memory:".into(),
        account_registry: None,
    })
}
impl Config {
    pub fn load() -> anyhow::Result<Arc<Self>> {
        Ok(load_runtime()?.service_config)
    }

    fn legacy_from_environment(
        lookup: &dyn Fn(&str) -> Option<OsString>,
    ) -> anyhow::Result<Arc<Self>> {
        let required =
            |key: &str| lookup_string(lookup, key)?.ok_or_else(|| anyhow::anyhow!("Missing {key}"));
        let value = |key: &str, fallback: &str| -> anyhow::Result<String> {
            Ok(lookup_string(lookup, key)?.unwrap_or_else(|| fallback.into()))
        };
        Self::from_mail(
            MailValues {
                address: required("EMAIL_ADDRESS")?,
                password: required("EMAIL_PASSWORD")?,
                imap_host: required("IMAP_HOST")?,
                imap_port: value("IMAP_PORT", "993")?.parse()?,
                smtp_host: required("SMTP_HOST")?,
                smtp_port: value("SMTP_PORT", "465")?.parse()?,
                smtp_starttls: value("SMTP_STARTTLS", "false")?.parse()?,
                drafts: value("DRAFTS_MAILBOX", "Drafts")?,
                trash: value("TRASH_MAILBOX", "Trash")?,
                mode: value("MAIL_MODE", "read")?,
            },
            lookup,
        )
    }

    fn from_mail(
        mail: MailValues,
        lookup: &dyn Fn(&str) -> Option<OsString>,
    ) -> anyhow::Result<Arc<Self>> {
        let required =
            |key: &str| lookup_string(lookup, key)?.ok_or_else(|| anyhow::anyhow!("Missing {key}"));
        let value = |key: &str, fallback: &str| -> anyhow::Result<String> {
            Ok(lookup_string(lookup, key)?.unwrap_or_else(|| fallback.into()))
        };
        let c = Self {
            address: mail.address,
            password: mail.password,
            imap_host: mail.imap_host,
            imap_port: mail.imap_port,
            smtp_host: mail.smtp_host,
            smtp_port: mail.smtp_port,
            smtp_starttls: mail.smtp_starttls,
            drafts: mail.drafts,
            trash: mail.trash,
            mode: mail.mode,
            timeout: Duration::from_secs(60),
            max_message: DEFAULT_MAX_MESSAGE,
            max_output: DEFAULT_MAX_OUTPUT,
            resource: required("MCP_RESOURCE")?,
            issuer: required("OIDC_ISSUER")?,
            audience: required("OIDC_AUDIENCE")?,
            subjects: required("OIDC_SUBJECTS")?
                .split(',')
                .map(str::to_owned)
                .collect(),
            scope: value("OIDC_SCOPE", "email:mcp")?,
            bind: value("BIND", "127.0.0.1:18590")?,
            database: value("DATABASE_PATH", "data/operations.sqlite")?,
            account_registry: lookup("ACCOUNT_REGISTRY_PATH").map(PathBuf::from),
        };
        anyhow::ensure!(
            ["read", "draft", "send", "full"].contains(&c.mode.as_str()),
            "Invalid MAIL_MODE"
        );
        anyhow::ensure!(
            c.resource.starts_with("https://") && c.issuer.starts_with("https://"),
            "HTTPS resource and issuer required"
        );
        anyhow::ensure!(
            !c.subjects.iter().any(String::is_empty),
            "Empty subject allowlist"
        );
        Ok(Arc::new(c))
    }
}

fn lookup_string(
    lookup: &dyn Fn(&str) -> Option<OsString>,
    key: &str,
) -> anyhow::Result<Option<String>> {
    lookup(key)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("Invalid configuration encoding"))
        })
        .transpose()
}

struct MailValues {
    address: String,
    password: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    smtp_starttls: bool,
    drafts: String,
    trash: String,
    mode: String,
}
