use axum::{Json, Router, middleware, routing::get};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rust_email_mcp::{
    accounts,
    auth::{Auth, require_auth},
    config,
    journal::Journal,
    mail::{MailRuntime, attachments},
    server::{EmailServer, limit_rpc_id},
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
    let cache = MailRuntime::prepare_cache(&catalog, cache_settings)?;
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
    let host = reqwest::Url::parse(&config.resource)?
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Missing resource hostname"))?
        .to_owned();
    let transport = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(256 * 1024)
        .with_allowed_hosts([host, "127.0.0.1".into(), "localhost".into()])
        .with_allowed_origins(["https://chatgpt.com:443".to_string()])
        .with_cancellation_token(ct.child_token());
    let download_mail = mail.clone();
    let service = StreamableHttpService::new(
        move || Ok(EmailServer(mail.clone())),
        Arc::new(LocalSessionManager::default()),
        transport,
    );
    let secured = Router::new()
        .route("/mcp/attachments/{token}", get(attachments::download))
        .with_state(download_mail)
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(limit_rpc_id))
        .layer(middleware::from_fn_with_state(auth, require_auth))
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .layer(tower_http::limit::RequestBodyLimitLayer::new(256 * 1024));
    let meta = serde_json::json!({"resource":config.resource,"authorization_servers":[config.issuer],"scopes_supported":[config.scope],"bearer_methods_supported":["header"]});
    let router = secured
        .route(
            "/healthz",
            get(|| async {
                Json(serde_json::json!({"status":"ok","checks":["process","configuration"]}))
            }),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(move || {
                let m = meta.clone();
                async move { Json(m) }
            }),
        );
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!(event = "started", version = env!("CARGO_PKG_VERSION"));
    axum::serve(listener, router)
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
            tokio::time::sleep(std::time::Duration::from_secs(35)).await;
        })
        .await?;
    cache.finish().await;
    Ok(())
}
