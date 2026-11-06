use rust_email_mcp::{
    accounts,
    auth::Auth,
    config,
    journal::Journal,
    mail::MailRuntime,
    server::{http_router, serve_until_shutdown},
};
use std::{path::Path, sync::Arc};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut witness = None;
    let result = runtime.block_on(run(&mut witness));
    // Application writes and cache work have drained on this return path.
    runtime.shutdown_background();
    drop(witness);
    result
}

async fn run(witness: &mut Option<accounts::CacheRun>) -> anyhow::Result<()> {
    // Library debug logs may contain protocol payloads. Keep a fixed allowlist filter.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::new("off,rust_email_mcp=info"))
        .with_target(false)
        .init();
    std::panic::set_hook(Box::new(|_| {
        tracing::error!(
            event = "panic",
            message = "Task panicked; inspect operation state"
        )
    }));
    let settings = config::load_runtime()?;
    let cache_settings = config::load_cache_configuration(&settings)?;
    let mut args = std::env::args_os().skip(1);
    let restore = match (args.next(), args.next()) {
        (None, None) => false,
        (Some(flag), None) if flag == "--cache-restored" => true,
        _ => anyhow::bail!("Usage: rust-email-mcp [--cache-restored]"),
    };
    anyhow::ensure!(
        !restore || cache_settings.is_some(),
        "CACHE_CONFIG_PATH is required to mark a restored cache"
    );
    let catalog = accounts::open_catalog(settings)?;
    let run = accounts::CacheRun::begin(catalog.config())?;
    let cache = MailRuntime::prepare_cache_for_run(&catalog, cache_settings, Some(run.identity()))?;
    *witness = Some(run);
    if restore {
        cache.mark_restored()?;
        tracing::info!(event = "cache_restore_marked");
        return Ok(());
    }
    let config = catalog.config().clone();
    let path = Path::new(&config.database);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let journal = Arc::new(Journal::open(path)?);
    let mail = Arc::new(MailRuntime::with_cache(catalog, journal, cache.clone())?);
    let auth = Auth::new(config.clone()).await?;
    let ct = tokio_util::sync::CancellationToken::new();
    let router = http_router(&config, mail.clone(), auth, ct.clone())?;
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    if !serve_until_shutdown(listener, router, mail, cache, ct).await? {
        // Blocking jobs may still hold storage state. Keep the run witness
        // locked until the OS terminates the process and its remaining work.
        std::process::exit(1);
    }
    Ok(())
}
