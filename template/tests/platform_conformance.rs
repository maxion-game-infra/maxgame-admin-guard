//! Consolidated crosswalk against PLATFORM.md §7's per-repo conformance
//! checklist ("Every Rust repo"). This is the M6 reference template — every
//! item here should still pass after you copy this crate out and rename it;
//! if one stops passing, you changed something the contract requires.
//! Checklist source: `maxgame-admin-guard/contract/PLATFORM.md` §7.
//!
//! ## Crosswalk
//!
//! - **Error envelope** `{statusCode, message, error}` (§1.1) —
//!   `src/inbound/error.rs`'s own unit tests
//!   (`every_variant_answers_the_platform_envelope`,
//!   `a_500_never_leaks_its_internal_detail`,
//!   `a_503_does_say_what_is_unavailable`).
//! - **Pagination** `{items, meta}` with `ListMeta` (§2.1) —
//!   `tests/example.rs::an_admin_with_the_feature_lists_every_item_paginated`
//!   and `an_empty_admin_list_still_reports_one_page`.
//! - **Health at root** — `tests/health.rs`.
//! - **`BASE_PATH` behaviour** — `tests/base_path.rs`.
//! - **Staging guardrails** keyed on `!is_dev()` — `src/config.rs` unit
//!   test `every_deployed_environment_gets_the_same_guardrails`.
//! - **Admin-auth 401-vs-503** — `tests/example.rs`
//!   (`a_mutation_from_a_deactivated_admin_is_401_not_a_stale_pass`,
//!   `a_mutation_the_idp_cannot_verify_is_503_not_401`,
//!   `a_read_when_the_jwks_is_unreachable_is_503_not_401`).
//! - **Empty-grant refusal before any feature check** (contract rule 6) —
//!   `tests/example.rs::the_admin_route_refuses_a_caller_without_the_feature`.
//!
//! ## New assertion below
//!
//! CORS `allow_headers` including `x-request-id` was wired
//! (`src/inbound/router.rs`) but had no HTTP-level test — added.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{mock_idp, state_with_env};
use sqlx::PgPool;
use template_server::inbound::build_router;
use tower::ServiceExt;

#[sqlx::test]
async fn preflight_allows_x_request_id_alongside_the_baseline_headers(pool: PgPool) {
    let idp = mock_idp().await;
    let state = state_with_env(
        pool,
        &idp.uri(),
        &[("CORS_ALLOWED_ORIGINS", "https://backoffice.maxion.game")],
    );
    let router = build_router(state);

    let request = Request::builder()
        .method(Method::OPTIONS)
        .uri("/example")
        .header("origin", "https://backoffice.maxion.game")
        .header("access-control-request-method", "GET")
        .header(
            "access-control-request-headers",
            "authorization,x-request-id",
        )
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(request).await.expect("router call");
    assert_eq!(response.status(), StatusCode::OK);

    let allow_headers = response
        .headers()
        .get("access-control-allow-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    assert!(allow_headers.contains("authorization"));
    assert!(allow_headers.contains("content-type"));
    assert!(allow_headers.contains("accept"));
    assert!(
        allow_headers.contains("x-request-id"),
        "PLATFORM.md §3.2 — got: {allow_headers}"
    );
}

#[sqlx::test]
async fn an_unlisted_origin_gets_no_allow_header(pool: PgPool) {
    let idp = mock_idp().await;
    let state = state_with_env(
        pool,
        &idp.uri(),
        &[("CORS_ALLOWED_ORIGINS", "https://backoffice.maxion.game")],
    );
    let router = build_router(state);

    let request = Request::builder()
        .uri("/healthz")
        .header("origin", "https://evil.example.com")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(request).await.expect("router call");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "CORS never blocks server-side"
    );
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "an unlisted origin must not be granted CORS access"
    );
}
