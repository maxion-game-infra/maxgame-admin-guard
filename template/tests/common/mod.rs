//! Shared integration-test helpers.
//!
//! The mock IdP is wiremock at the HTTP boundary (JWKS *and* live
//! introspection), not a fake port swapped into `AdminGuard` — these tests
//! exercise the real `maxion-admin-guard` HTTP clients, the pieces most
//! likely to be wrong in a way a fake would hide.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use template_server::config::Config;
use template_server::inbound::{build_router, AppState};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const TEST_ISSUER: &str = "template-server.test.maxion.game";
pub const TEST_KID: &str = "ed-test-1";
pub const JWKS_PATH: &str = "/.well-known/jwks.json";
pub const INTROSPECT_PATH: &str = "/api/v1/oauth/introspect";
pub const INTROSPECT_API_KEY: &str = "test-introspect-key-not-a-real-secret";

/// Config with every required variable filled in and everything else on its
/// default.
pub fn test_config(idp_uri: &str, overrides: &[(&str, &str)]) -> Config {
    let mut env: BTreeMap<String, String> = [
        ("APP_ENV", "development"),
        ("DATABASE_URL", "postgres://unused-by-these-tests"),
        ("ADMIN_IDP_BASE_URL", idp_uri),
        ("ADMIN_JWT_ISSUER", TEST_ISSUER),
        ("ADMIN_INTROSPECT_API_KEY", INTROSPECT_API_KEY),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in overrides {
        env.insert(k.to_string(), v.to_string());
    }
    Config::load(&env).expect("test config should load")
}

/// An [`AppState`] wired to `pool` and a mock IdP at `idp_uri`.
pub fn state_for(pool: PgPool, idp_uri: &str) -> AppState {
    state_with_env(pool, idp_uri, &[])
}

pub fn state_with_env(pool: PgPool, idp_uri: &str, overrides: &[(&str, &str)]) -> AppState {
    let config = Arc::new(test_config(idp_uri, overrides));
    AppState::new(config, pool, reqwest::Client::new())
}

pub fn router_for(pool: PgPool, idp_uri: &str) -> Router {
    build_router(state_for(pool, idp_uri))
}

// ── the mock admin IdP ────────────────────────────────────────────────

/// The keypair the mock IdP signs with. Fixed bytes so a failure is
/// reproducible.
pub fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[9u8; 32])
}

/// Starts a `MockServer` that publishes `signing_key()`'s public half at
/// [`JWKS_PATH`], and answers [`INTROSPECT_PATH`] with `active: true` for
/// every admin id passed to [`mount_active_introspection`] — call that
/// after this if a test drives a mutating (introspected) route.
pub async fn mock_idp() -> MockServer {
    let server = MockServer::start().await;
    let key = signing_key();
    let x = URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes());
    Mock::given(method("GET"))
        .and(path(JWKS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [{ "kty": "OKP", "crv": "Ed25519", "kid": TEST_KID, "x": x }]
        })))
        .mount(&server)
        .await;
    server
}

/// Mount an introspection verdict of `active: true` for every call — the
/// live verdict a mutating admin-route test needs to see the same identity
/// the token itself carries.
pub async fn mount_active_introspection(
    server: &MockServer,
    admin_id: &str,
    role: &str,
    site_access: Value,
) {
    Mock::given(method("POST"))
        .and(path(INTROSPECT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "active": true,
            "adminId": admin_id,
            "role": role,
            "siteAccess": site_access,
        })))
        .mount(server)
        .await;
}

/// Mount an unreachable/erroring introspection endpoint — for the 503 case
/// (contract rule 5).
pub async fn mount_broken_introspection(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(INTROSPECT_PATH))
        .respond_with(ResponseTemplate::new(502))
        .mount(server)
        .await;
}

/// Sign a raw admin JWT with [`signing_key`], for tests that need a
/// well-formed token without a full IdP flow.
pub fn mint_token(admin_id: &str, role: &str, site_access: Value) -> String {
    mint_raw_token(json!({
        "sub": admin_id,
        "iss": TEST_ISSUER,
        "role": role,
        "type": "admin",
        "siteAccess": site_access,
        "exp": (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
    }))
}

/// Sign arbitrary claims — for cases a well-formed token cannot express (an
/// expired `exp`, a foreign `type`).
pub fn mint_raw_token(claims: Value) -> String {
    let key = signing_key();
    let header = URL_SAFE_NO_PAD
        .encode(json!({ "alg": "EdDSA", "typ": "JWT", "kid": TEST_KID }).to_string());
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signing_input = format!("{header}.{payload}");
    let signature = key.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

// ── HTTP helpers ──────────────────────────────────────────────────────

pub struct TestResponse {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Value,
}

pub async fn call(router: &Router, request: Request<Body>) -> TestResponse {
    let response = router.clone().oneshot(request).await.expect("router call");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    TestResponse {
        status,
        headers,
        body,
    }
}

pub fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

pub fn get_as(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

pub fn post_as(uri: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
