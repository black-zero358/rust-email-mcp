use crate::mail::{MailRuntime, PreparedCache};
use axum::Router;
use std::{future::IntoFuture, io, sync::Arc};
use tokio::{net::TcpListener, time::Instant};
use tokio_util::sync::CancellationToken;

pub async fn serve_until_shutdown(
    listener: TcpListener,
    router: Router,
    mail: Arc<MailRuntime>,
    cache: PreparedCache,
    shutdown: CancellationToken,
) -> io::Result<bool> {
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let signal = async {
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    };

    let server = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .into_future();
    tokio::pin!(server);
    mail.start_sync();
    tracing::info!(event = "started", version = env!("CARGO_PKG_VERSION"));
    let early = tokio::select! {
        result = &mut server => Some(result),
        _ = signal => None,
    };

    let deadline = Instant::now() + mail.shutdown_budget();
    mail.begin_shutdown();
    shutdown.cancel();
    let (http, drained) = tokio::join!(
        async {
            match early {
                Some(result) => Ok(result),
                None => tokio::time::timeout_at(deadline, &mut server).await,
            }
        },
        tokio::time::timeout_at(deadline, mail.drain_shutdown(deadline)),
    );

    if matches!(http, Ok(Ok(())))
        && matches!(drained, Ok(true))
        && Instant::now() < deadline
        && mail.finish_cache(cache, deadline).await
    {
        return Ok(true);
    }
    tracing::warn!(event = "shutdown_incomplete");
    Ok(false)
}
