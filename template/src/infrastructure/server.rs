//! Process lifecycle: bind, serve, drain.
//!
//! Lives in the library, not `main`, so the drain behaviour is reachable
//! from a test (`tests/server.rs`).

use std::future::Future;

use axum::Router;
use tokio::net::TcpListener;

use crate::domain::error::{DomainError, DomainResult};

/// Serve until `shutdown` resolves, then stop accepting connections and let
/// in-flight requests finish.
pub async fn serve<F>(listener: TcpListener, router: Router, shutdown: F) -> DomainResult<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| DomainError::Internal(format!("server error: {e}")))
}

/// Resolves on SIGTERM or Ctrl-C. SIGTERM is what Kubernetes and Compose
/// send; Ctrl-C is what a developer sends. Both mean the same thing here.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "cannot listen for SIGTERM");
                // Without a SIGTERM handler this future must never resolve,
                // or the select below would shut the server down at once.
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received; draining in-flight requests");
}
