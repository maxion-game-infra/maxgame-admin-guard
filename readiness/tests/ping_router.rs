//! Integration tests for the callee side of the identity endpoint
//! (`src/ping.rs`, CONTRACT.md §5): the nested router's answer through
//! `tower::ServiceExt::oneshot`, and the caller side (`PingProbe`) against
//! the same router on a real port, end to end.

#![cfg(feature = "http")]

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use maxgame_readiness::{ping_router, AppEnv, CheckStatus, Identity, PingProbe, Probe};
use tower::ServiceExt;

/// The app every test below serves: the ping router nested under the
/// callee's `BASE_PATH`, as CONTRACT.md §5.1 requires (unlike `/readyz`,
/// which sits at the pod root).
fn nested() -> axum::Router {
    axum::Router::new().nest("/keys", ping_router::<()>("keyServer", AppEnv::Development))
}

// 19. GET {BASE_PATH}/v1/ping answers 200 with exactly the two identity
//     fields and the contract's headers; without the prefix there is no
//     route to find.
#[tokio::test]
async fn nested_ping_router_answers_two_fields_and_404_without_the_prefix() {
    let response = nested()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/keys/v1/ping")
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await
        .expect("the router answers");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "the identity answer must not be cached"
    );
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.starts_with("application/json"),
        "unexpected content type {content_type}"
    );

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("the body collects")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("the body is JSON");
    let object = body.as_object().expect("the body is an object");
    assert_eq!(
        object.len(),
        2,
        "exactly service and env, nothing else: {object:?}"
    );
    assert_eq!(object["service"], "keyServer");
    assert_eq!(
        object["env"], "development",
        "the parsed tier, not the raw APP_ENV"
    );

    // The route exists only under BASE_PATH (CONTRACT.md §5.1): an
    // unprefixed request finds nothing.
    let response = nested()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/ping")
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// 20. End to end: the same nested router on a real port, and a real
//     PingProbe pointed at its base URL, is ok.
#[tokio::test]
async fn a_ping_probe_against_the_nested_router_is_ok() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the listener binds");
    let addr = listener.local_addr().expect("a local address");
    let app = nested();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("the router serves");
    });

    let outcome = PingProbe::new(
        reqwest::Client::new(),
        format!("http://{addr}/keys"),
        "launcher",
        Identity::new("keyServer", "development"),
    )
    .run()
    .await;

    assert_eq!(outcome.status, CheckStatus::Ok, "{outcome:?}");
    assert_eq!(outcome.reason, None);
    assert_eq!(outcome.http_status, Some(200));
}
