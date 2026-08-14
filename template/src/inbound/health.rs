//! `/healthz` (liveness) and `/readyz` (readiness). Mounted at the router's
//! **root** by `inbound::router::build_router`, always — even when
//! `BASE_PATH` is set, per `PLATFORM.md` §3.1/§5.2: a k8s probe hits the
//! pod directly, never through a gateway prefix.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use serde_json::json;

use crate::inbound::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}

/// Never touches the database: an upstream blip must not restart a
/// perfectly healthy process.
async fn healthz() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Checks the database. A load balancer drains traffic on a failing
/// `/readyz`, which is what should happen while Postgres is unreachable —
/// unlike `/healthz`, lying here would route real requests at a dependency
/// that cannot serve them.
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    match sqlx::query("SELECT 1").fetch_one(&state.pool).await {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ready" }))),
        Err(err) => {
            tracing::warn!(error = %err, "readyz: database unreachable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "unavailable", "dependency": "postgres" })),
            )
        }
    }
}
