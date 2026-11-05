//! Opt-in release acceptance. The fixture host, HTTP service and load driver
//! run in separate processes so resource samples describe the service alone.
use super::*;
use crate::{
    accounts::{CacheRun, open_catalog},
    auth::Auth,
    config::{AccountConfigSource, ValidatedAccountConfig},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio_util::sync::CancellationToken;

mod fixture;

const TEST_PREFIX: &str = "server::output_tests::r13_acceptance::";
const ACCOUNTS: [&str; 2] = ["default", "second"];
const PRINCIPALS: [&str; 2] = ["primary", "secondary"];
const CACHE_QUOTA: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct Startup {
    imap_ports: [u16; 2],
    issuer: String,
}

#[derive(Deserialize)]
struct Control {
    seq: u64,
    op: String,
}

fn root() -> Result<PathBuf> {
    let path = PathBuf::from(std::env::var_os("R13_DIR").context("Set private R13_DIR")?);
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        path.is_absolute() && metadata.is_dir() && metadata.permissions().mode() & 0o777 == 0o700,
        "R13_DIR must be an absolute private directory"
    );
    Ok(path)
}

fn publish(root: &Path, name: &str, data: &[u8]) -> Result<()> {
    let pending = root.join(format!(".{name}.next"));
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&pending)?
        .write_all(data)?;
    fs::rename(pending, root.join(name))?;
    Ok(())
}

fn publish_json(root: &Path, name: &str, data: &impl Serialize) -> Result<()> {
    publish(root, name, &serde_json::to_vec(data)?)
}

fn read_json<T: DeserializeOwned>(root: &Path, name: &str) -> Result<Option<T>> {
    const LIMIT: u64 = 128 * 1024;
    match fs::File::open(root.join(name)) {
        Ok(file) => {
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.permissions().mode() & 0o777 == 0o600,
                "Invalid private fixture file"
            );
            ensure!(
                metadata.len() <= LIMIT,
                "Fixture file exceeds control budget"
            );
            let mut bytes = Vec::new();
            file.take(LIMIT + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= LIMIT,
                "Fixture file exceeds control budget"
            );
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "R13 offline release load fixture, driven by scripts/run-r13-acceptance.py"]
async fn fixture_host() -> Result<()> {
    let root = root()?;
    ensure!(
        fs::read_dir(&root)?.next().is_none(),
        "R13_DIR must be empty"
    );
    let fixture = fixture::Fixture::start().await?;
    publish(&root, "ca.der", &fixture.certificate)?;
    publish_json(&root, "startup.json", &fixture.startup)?;
    publish_json(
        &root,
        "cache-config.json",
        &json!({
            "version": 1, "mode": "full", "directory": "cache", "max_bytes": CACHE_QUOTA,
            "accounts": ACCOUNTS.map(|id| json!({
                "account_id": id, "mailboxes": ["INBOX", "Archive"], "max_bytes": CACHE_QUOTA / 2,
            })),
            "sync": {"enabled": true, "interval_seconds": 1, "reconcile_interval_seconds": 45, "protocol": "auto"},
        }),
    )?;

    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            &format!("{TEST_PREFIX}service_child"),
            "--ignored",
        ])
        .env_clear()
        .env("R13_DIR", &root)
        .env("R13_SERVICE_CHILD", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let service_pid = child.id().context("Missing service child PID")?;
    let http_base: String = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(base) = read_json(&root, "service-address.json")? {
                break Ok::<_, anyhow::Error>(base);
            }
            ensure!(
                child.try_wait()?.is_none(),
                "Service child exited during setup"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    let resource = protected_resource(&http_base);
    publish_json(
        &root,
        "ready.json",
        &json!({
            "schema": 1, "service_pid": service_pid, "http_base": http_base,
            "cache_root": root.join("cache"), "cache_max_bytes": CACHE_QUOTA, "stage_root": root.join("staging"),
            "resource": resource, "operations_db": root.join("operations.sqlite"),
            "jwt": fixture.tokens(&resource)?, "accounts": fixture.accounts(),
            "attachments": fixture.attachments(),
            "cases": {
                "search":{"account_id":ACCOUNTS[0],"mailbox":"INBOX","uid_validity":fixture::inbox_validity(),"query":"中文关键字","limit":5},
                "conversation_seed":{"account_id":ACCOUNTS[0],"mailbox":"INBOX","uid":1,"uid_validity":fixture::inbox_validity(),"mailboxes":["INBOX","Archive"]},
            },
        }),
    )?;

    let mut sequence = 0;
    loop {
        ensure!(
            child.try_wait()?.is_none(),
            "Service child exited before shutdown"
        );
        if let Some(request) = read_json::<Control>(&root, "control.request.json")?
            && request.seq > sequence
        {
            sequence = request.seq;
            let reply = match request.op.as_str() {
                "sample" => Ok(fixture.sample()),
                "hold_live_gets" => fixture.arm(),
                "release_live_gets" => Ok(fixture.release()),
                "append_tail" => Ok(fixture.append_tail()),
                "shutdown" => {
                    fixture.release();
                    publish_json(&root, "service-stop.json", &true)?;
                    let status =
                        tokio::time::timeout(Duration::from_secs(50), child.wait()).await??;
                    ensure!(status.success(), "Service child shutdown failed");
                    publish_json(
                        &root,
                        "control.response.json",
                        &json!({
                            "seq": sequence, "ok": true, "data": fixture.sample(),
                        }),
                    )?;
                    break;
                }
                _ => Err(anyhow::anyhow!("Unknown fixture operation")),
            };
            let response = match reply {
                Ok(data) => json!({"seq": sequence, "ok": true, "data": data}),
                Err(error) => {
                    json!({"seq": sequence, "ok": false, "data": {"error": error.to_string()}})
                }
            };
            publish_json(&root, "control.response.json", &response)?;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "R13 isolated HTTP service subprocess"]
async fn service_child() -> Result<()> {
    ensure!(
        std::env::var("R13_SERVICE_CHILD").as_deref() == Ok("1"),
        "Start fixture_host first"
    );
    let root = root()?;
    let startup: Startup =
        read_json(&root, "startup.json")?.context("Missing startup configuration")?;
    let issuer = reqwest::Url::parse(&startup.issuer)?;
    ensure!(
        issuer.scheme() == "https"
            && issuer.host_str() == Some("127.0.0.1")
            && issuer.port().is_some(),
        "Fixture issuer must use loopback HTTPS"
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let settings = service_settings(&root, startup, &base);
    let cache_settings = config::load_cache_file(&root.join("cache-config.json"), &settings)?;
    let catalog = open_catalog(settings)?;
    let witness = CacheRun::begin(catalog.config())?;
    let cache =
        MailRuntime::prepare_cache_for_run(&catalog, cache_settings, Some(witness.identity()))?;
    let journal = Arc::new(Journal::open(&root.join("operations.sqlite"))?);
    let certificate = fs::read(root.join("ca.der"))?;
    let mut roots = RootCertStore::empty();
    roots.add(certificate.clone().into())?;
    let runtime = Arc::new(
        MailRuntime::with_cache(catalog, journal, cache.clone())?.with_test_imap_roots(roots),
    );
    let auth = Auth::discover_with_test_ca(
        runtime.config().clone(),
        reqwest::Certificate::from_der(&certificate)?,
    )
    .await?;
    let stop = CancellationToken::new();
    let app = http_router(runtime.config(), runtime.clone(), auth, stop.clone())?;
    runtime.start_sync();
    publish_json(&root, "service-address.json", &base)?;
    let poll = {
        let root = root.clone();
        let runtime = runtime.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let _cancel = stop.clone().drop_guard();
            for sequence in 1u64.. {
                if root.join("service-stop.json").exists() {
                    stop.cancel();
                    runtime.stop_sync();
                    break;
                }
                let mut sample = runtime.scheduler_sample_for_test();
                sample["sequence"] = json!(sequence);
                sample["transfers"] = json!(runtime.attachments.transfers_for_test());
                publish_json(&root, "service-sample.json", &sample)?;
                tokio::time::sleep(Duration::from_millis(125)).await;
            }
            Ok::<(), anyhow::Error>(())
        })
    };
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(stop.cancelled_owned())
        .await;
    poll.await??;
    runtime.finish_cache(cache).await;
    drop(runtime);
    drop(witness);
    served?;
    Ok(())
}

fn service_settings(root: &Path, startup: Startup, base: &str) -> ValidatedAccountSet {
    let mut primary = (*config::fixture()).clone();
    primary.imap_host = "127.0.0.1".into();
    primary.imap_port = startup.imap_ports[0];
    primary.smtp_host.clear();
    primary.address = "primary@example.test".into();
    primary.account_registry = Some(root.join("accounts.sqlite"));
    primary.attachment_staging_dir = Some(root.join("staging"));
    primary.database = root
        .join("operations.sqlite")
        .to_string_lossy()
        .into_owned();
    primary.timeout = Duration::from_secs(60);
    primary.max_message = 32 * 1024 * 1024;
    primary.max_output = 2 * 1024 * 1024;
    primary.issuer = startup.issuer;
    primary.resource = protected_resource(base);
    primary.audience.clone_from(&primary.resource);
    primary.subjects = PRINCIPALS.map(str::to_owned).into();
    let mut secondary = primary.clone();
    secondary.address = "secondary@example.test".into();
    secondary.imap_port = startup.imap_ports[1];
    secondary.mode = "read".into();
    let mut settings = ValidatedAccountSet::legacy(Arc::new(primary));
    settings.source = AccountConfigSource::ExtendLegacy;
    settings.accounts[0].read_subjects = vec![PRINCIPALS[0].into()];
    settings.accounts[0].write_subjects = vec![PRINCIPALS[0].into()];
    settings.accounts.push(ValidatedAccountConfig {
        registration_key: "r13-secondary".into(),
        alias: ACCOUNTS[1].into(),
        display_name: "Synthetic secondary".into(),
        config: Arc::new(secondary),
        read_subjects: PRINCIPALS.map(str::to_owned).into(),
        write_subjects: Vec::new(),
        is_legacy: false,
    });
    settings
}

fn protected_resource(base: &str) -> String {
    format!("{}/mcp", base.replacen("http://", "https://", 1))
}
