use std::{env, sync::Arc, time::Duration};

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
    })
}
impl Config {
    pub fn load() -> anyhow::Result<Arc<Self>> {
        let required = |key: &str| env::var(key).map_err(|_| anyhow::anyhow!("Missing {key}"));
        let value = |key: &str, fallback: &str| env::var(key).unwrap_or_else(|_| fallback.into());
        let c = Self {
            address: required("EMAIL_ADDRESS")?,
            password: required("EMAIL_PASSWORD")?,
            imap_host: required("IMAP_HOST")?,
            imap_port: value("IMAP_PORT", "993").parse()?,
            smtp_host: required("SMTP_HOST")?,
            smtp_port: value("SMTP_PORT", "465").parse()?,
            smtp_starttls: value("SMTP_STARTTLS", "false").parse()?,
            drafts: value("DRAFTS_MAILBOX", "Drafts"),
            trash: value("TRASH_MAILBOX", "Trash"),
            mode: value("MAIL_MODE", "read"),
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
            scope: value("OIDC_SCOPE", "email:mcp"),
            bind: value("BIND", "127.0.0.1:18590"),
            database: value("DATABASE_PATH", "data/operations.sqlite"),
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
