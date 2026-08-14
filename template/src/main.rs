//! Server entrypoint: load config, connect, migrate, serve.

use std::sync::Arc;

use template_server::adapters::postgres;
use template_server::config::{AppEnv, Config};
use template_server::domain::error::{DomainError, DomainResult};
use template_server::inbound::{build_router, AppState};
use template_server::infrastructure::server::{serve, shutdown_signal};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // Tracing has to come up before anything can fail, but the format
    // depends on config we have not read yet. Read APP_ENV directly for
    // this one decision; everything else goes through `Config`.
    let production = std::env::var("APP_ENV")
        .ok()
        .and_then(|value| value.parse::<AppEnv>().ok())
        .is_some_and(|env| env.is_production());
    init_tracing(production);

    if let Err(err) = run().await {
        // A boot failure is always a configuration or dependency problem —
        // say which one on the way out rather than panicking with a
        // backtrace.
        tracing::error!(error = %err, "startup failed");
        std::process::exit(1);
    }
}

async fn run() -> DomainResult<()> {
    let config = Arc::new(Config::from_env()?);

    let pool = postgres::connect(&config.database_url, config.database_max_connections).await?;
    postgres::migrate(&pool).await?;

    let addr = config.bind_address();
    let http = reqwest::Client::new();
    let state = AppState::new(config.clone(), pool, http);
    let router = build_router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| DomainError::Internal(format!("cannot bind {addr}: {e}")))?;

    tracing::info!(
        address = %addr,
        env = config.app_env.as_str(),
        base_path = %config.base_path,
        issuer = %config.admin_idp.issuer,
        "template server listening"
    );

    serve(listener, router, shutdown_signal()).await
}

/// Structured JSON in production so a log pipeline can index the fields;
/// human-readable everywhere else.
fn init_tracing(production: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,template_server=debug"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if production {
        builder.json().init();
    } else {
        builder.init();
    }
}
