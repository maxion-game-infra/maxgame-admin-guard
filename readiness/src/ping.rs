//! The callee side of the identity endpoint: `GET {BASE_PATH}/v1/ping`
//! (`CONTRACT.md` §5).
//!
//! Unlike `/healthz` and `/readyz`, which sit at the pod root
//! (`PLATFORM.md` §3.1), this router MUST be nested under the service's
//! `BASE_PATH`, because validating the prefix is its whole purpose
//! (`CONTRACT.md` §5.1, §5.2): a caller holds a base URL that includes the
//! prefix, and the callee's `/readyz` is unreachable through the gateway by
//! construction.
//!
//! The route carries no state and needs no auth: it touches no datastore,
//! verifies no credential and calls no other service (`CONTRACT.md` §6.3
//! rule 7), and answers exactly the two identity keys — no version, no
//! hostnames, no configuration — because the path is reachable from the
//! internet through the gateway (`CONTRACT.md` §5.2).

use axum::http::{header, HeaderValue};
use serde::Serialize;

use crate::env::AppEnv;

/// The `/v1/ping` body (`CONTRACT.md` §5.2): exactly `service` and `env`,
/// and nothing else. `env` is the parsed tier, never the raw `APP_ENV`
/// spelling (`CONTRACT.md` §3.4).
#[derive(Clone, Serialize)]
struct PingBody {
    service: String,
    env: AppEnv,
}

/// A router serving `GET /v1/ping` → 200 `{"service": ..., "env": ...}`
/// with `Cache-Control: no-store` and `Content-Type: application/json`
/// (`CONTRACT.md` §5.2).
///
/// Nest it under the service's `BASE_PATH`, unlike the `/readyz` router:
///
/// ```text
/// let app = Router::new()
///     .merge(readiness.router::<()>())          // at the pod root
///     .nest(&base_path, ping_router(service_key, env));
/// ```
///
/// No state, no auth, no dependency access: the handler only formats the
/// two strings it was built with.
pub fn ping_router<S>(service: impl Into<String>, env: AppEnv) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let body = PingBody {
        service: service.into(),
        env,
    };
    axum::Router::new().route(
        "/v1/ping",
        axum::routing::get(move || {
            let body = body.clone();
            async move {
                (
                    [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
                    axum::Json(body),
                )
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The body has exactly the two keys of CONTRACT.md §5.2: no version, no
    // hostnames, no configuration, because the path is reachable from the
    // internet through the gateway.
    #[test]
    fn ping_body_is_exactly_service_and_env() {
        let body = PingBody {
            service: "keyServer".to_owned(),
            env: AppEnv::Development,
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({ "service": "keyServer", "env": "development" })
        );

        let body = PingBody {
            service: "launcher".to_owned(),
            env: AppEnv::Production,
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({ "service": "launcher", "env": "production" })
        );
    }
}
