use rust_email_mcp::{
    accounts, auth::Auth, config, journal::Journal, mail::MailRuntime, server::http_router,
};
use std::{path::Path, sync::Arc};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let shutdown_mail = mail.clone();
    let signal_mail = mail.clone();
    let router = http_router(&config, mail, auth, ct.clone())?;
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    shutdown_mail.start_sync();
    tracing::info!(event = "started", version = env!("CARGO_PKG_VERSION"));
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("signal handler");
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            ct.cancel();
            signal_mail.stop_sync();
            tokio::time::sleep(std::time::Duration::from_secs(35)).await;
        })
        .await;
    shutdown_mail.finish_cache(cache).await;
    served?;
    drop(run);
    Ok(())
}
