//! Route table. Copy this file's shape into a new service; only the routes
//! themselves (`example::public_routes()` / `example::admin_routes()`) are
//! specific to the template.

use std::time::Duration;

use axum::http::{header, HeaderName, HeaderValue, Method};
use axum::middleware::from_fn;
use axum::Router;
use maxion_admin_guard::require_admin;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::inbound::{example, health, AppState};

pub fn build_router(state: AppState) -> Router {
    let cors = cors_layer(&state.config);

    // Admin routes are gated by the shared `AdminGuard` — the eight rules in
    // `maxion-admin-guard/contract/README.md`, run the same way every other
    // admin-facing service on the platform runs them. `route_layer` runs the
    // *last* layer added first; there's only one layer here because
    // `require_admin` already does the whole contract (offline verify, live
    // introspect on mutations, site/feature check) in one call.
    let admin = example::admin_routes().route_layer(from_fn(require_admin(
        state.admin_guard.clone(),
        example::EXAMPLE_SITE,
        example::EXAMPLE_FEATURE,
    )));

    let public = example::public_routes();

    // Everything except health lives here, so it can be nested under
    // `BASE_PATH` (`PLATFORM.md` §5) as one unit below.
    let prefixed = public.merge(admin);

    // An empty `BASE_PATH` (the default) mounts at root, exactly as before —
    // nesting under `""` isn't the same operation as no nesting at all, so
    // it's skipped rather than passed to `Router::nest`.
    let prefixed = match state.config.base_path.as_str() {
        "" => prefixed,
        base_path => Router::new().nest(base_path, prefixed),
    };

    // `/healthz` and `/readyz` are never moved under `BASE_PATH`
    // (`PLATFORM.md` §5.2 / §3.1): a k8s probe hits the pod directly, never
    // through the gateway.
    let router = Router::new().merge(health::routes()).merge(prefixed);

    router
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

/// Browser access, per environment. Credentials are not allowed: the back
/// office holds its token itself and sends it as a bearer header, so a
/// cookie would only add CSRF exposure.
fn cors_layer(config: &Config) -> CorsLayer {
    let origins: Vec<HeaderValue> = config
        .cors_allowed_origins
        .iter()
        .filter_map(|origin| match origin.parse::<HeaderValue>() {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::warn!(origin = %origin, "ignoring unparseable CORS origin");
                None
            }
        })
        .collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            // Platform-standard (`PLATFORM.md` §3.2) — the SPA attaches this
            // on every instance whose CORS allows it. Add repo-specific
            // extras (e.g. utility's `x-api-key`) here, not by replacing
            // this list.
            HeaderName::from_static("x-request-id"),
        ])
        .max_age(Duration::from_secs(600))
}
