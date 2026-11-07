//! Integration tests for the HTTP hop probes (`src/http.rs`, CONTRACT.md
//! §6.2) against a fake upstream: what each probe requests, what it never
//! sends, and how it classifies the answers.
//!
//! The fake upstream is an axum server on `127.0.0.1:0` wrapped in a layer
//! that records every request's method, path, headers and body, so a test
//! can assert both on the probe's outcome and on the wire. The probes run
//! directly (`probe.run().await`, no `Readiness`), with a plain
//! `reqwest::Client::new()` that carries no default credential headers.

#![cfg(feature = "http")]

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::Router;
use jsonschema::Validator;
use maxgame_readiness::{
    classify_error_status, AppEnv, AuthEndpointProbe, ChainIdProbe, CheckStatus, Identity,
    JwksProbe, PingProbe, Probe, ProbeOutcome, ReachProbe, Readiness, PROBE_HEADER,
};
use serde_json::Value;

/// The most the recording layer reads of a request body: far above anything
/// a probe sends (`{}` or a JSON-RPC envelope).
const REQUEST_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// The identity every ok case below expects: the key server on the dev
/// cluster.
fn key_server() -> Identity {
    Identity::new("keyServer", "development")
}

/// Validate a `/readyz` body against the contract's JSON Schema
/// (`schema/readyz.schema.json`), as in `tests/readyz.rs`.
fn assert_schema(body: &Value) {
    static SCHEMA: OnceLock<Validator> = OnceLock::new();
    let validator = SCHEMA.get_or_init(|| {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/schema/readyz.schema.json");
        let raw = std::fs::read_to_string(path).expect("readyz schema is readable");
        let schema: Value = serde_json::from_str(&raw).expect("readyz schema parses as JSON");
        jsonschema::validator_for(&schema).expect("readyz schema compiles")
    });
    if let Err(problem) = validator.validate(body) {
        panic!("readyz body failed schema validation: {problem}\nbody: {body}");
    }
}

// ----------------------------------------------------------------------
// Fake upstream
// ----------------------------------------------------------------------

/// One request the fake upstream saw.
#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl SeenRequest {
    /// The first value of header `name`, when it came and parses as UTF-8.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// The request body as lossy UTF-8, for assert messages.
    fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Everything the fake upstream saw, in arrival order.
type Seen = Arc<Mutex<Vec<SeenRequest>>>;

/// A snapshot of everything the fake upstream saw so far.
fn take(seen: &Seen) -> Vec<SeenRequest> {
    seen.lock().unwrap().clone()
}

/// The one request the fake upstream saw.
fn the_one(seen: &Seen) -> SeenRequest {
    let requests = take(seen);
    assert_eq!(requests.len(), 1, "expected exactly one request");
    requests.into_iter().next().expect("length checked above")
}

/// The last request the fake upstream saw.
fn the_last(seen: &Seen) -> SeenRequest {
    take(seen)
        .into_iter()
        .next_back()
        .expect("the fake upstream saw at least one request")
}

/// Serve `router` on `127.0.0.1:0`, recording every request it sees, and
/// return the server's base URL (`http://127.0.0.1:PORT`) and the recording.
async fn fake(router: Router) -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let router = router.layer(from_fn(move |request: Request, next: Next| {
        let seen = Arc::clone(&recorded);
        async move {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, REQUEST_BODY_LIMIT)
                .await
                .expect("the recording layer reads the request body");
            seen.lock().unwrap().push(SeenRequest {
                method: parts.method.as_str().to_owned(),
                path: parts.uri.path().to_owned(),
                headers: parts.headers.clone(),
                body: bytes.to_vec(),
            });
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the fake upstream binds 127.0.0.1:0");
    let addr = listener.local_addr().expect("the fake upstream has a port");
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the fake upstream serves");
    });
    (format!("http://{addr}"), seen)
}

/// A route at `path` answering every method with `status`, `content_type`
/// and `body`. Every method, because the probes GET, POST and HEAD.
fn answering(path: &str, status: StatusCode, content_type: &'static str, body: String) -> Router {
    Router::new().route(
        path,
        axum::routing::any(move || {
            let body = body.clone();
            async move { (status, [(header::CONTENT_TYPE, content_type)], body) }
        }),
    )
}

/// A route at `path` that sleeps `secs` before answering 200 with a valid
/// ping body, for the timeout classification.
fn slow(path: &str, secs: u64) -> Router {
    Router::new().route(
        path,
        axum::routing::any(move || async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"service":"keyServer","env":"development"}"#.to_owned(),
            )
        }),
    )
}

/// Assert the outcome is exactly this failure — status, reason, and no
/// identity fields where none belong.
fn assert_failure(outcome: &ProbeOutcome, status: CheckStatus, reason: &str) {
    assert_eq!(outcome.status, status, "status: {outcome:?}");
    assert_eq!(
        outcome.reason.as_deref(),
        Some(reason),
        "reason: {outcome:?}"
    );
    assert!(
        outcome.expected.is_none() && outcome.observed.is_none(),
        "no identity belongs on this outcome: {outcome:?}"
    );
}

// ----------------------------------------------------------------------
// PingProbe
// ----------------------------------------------------------------------

// 1. The ok case: GET {base}/v1/ping answers 200 with the expected
//    identity; the request carries the caller's key in the probe header and
//    no credential anywhere.
#[tokio::test]
async fn ping_probe_ok_requests_the_joined_path_with_no_credential() {
    let (base, seen) = fake(answering(
        "/keys/v1/ping",
        StatusCode::OK,
        "application/json",
        r#"{"service":"keyServer","env":"development"}"#.to_owned(),
    ))
    .await;

    let outcome = PingProbe::new(
        reqwest::Client::new(),
        format!("{base}/keys"),
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_eq!(outcome.status, CheckStatus::Ok, "{outcome:?}");
    assert_eq!(outcome.reason, None);
    assert_eq!(outcome.http_status, Some(200), "{outcome:?}");

    let request = the_one(&seen);
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.path, "/keys/v1/ping",
        "the callee's BASE_PATH is part of the address (CONTRACT.md §5.1)"
    );
    assert_eq!(
        request.header(PROBE_HEADER),
        Some("launcher"),
        "the caller's own service key marks the probe (CONTRACT.md §6.3 rule 1)"
    );
    assert_eq!(
        request.header("authorization"),
        None,
        "a probe sends no credential (CONTRACT.md §1.1)"
    );
    assert_eq!(
        request.header("x-api-key"),
        None,
        "a probe sends no credential (CONTRACT.md §1.1)"
    );
}

// 2. A trailing slash on the base URL must not become '//': still
//    /keys/v1/ping.
#[tokio::test]
async fn ping_probe_tolerates_a_trailing_slash_on_the_base() {
    let (base, seen) = fake(answering(
        "/keys/v1/ping",
        StatusCode::OK,
        "application/json",
        r#"{"service":"keyServer","env":"development"}"#.to_owned(),
    ))
    .await;

    let outcome = PingProbe::new(
        reqwest::Client::new(),
        format!("{base}/keys/"),
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_eq!(outcome.status, CheckStatus::Ok, "{outcome:?}");
    assert_eq!(the_one(&seen).path, "/keys/v1/ping");
}

// 3. A 200 with the wrong identity is wrong_target: wrong_service (checked
//    before the env) and wrong_env (dev pointed at prod, or the reverse).
#[tokio::test]
async fn ping_probe_classifies_wrong_service_and_wrong_env() {
    let (base, _) = fake(
        answering(
            "/a/v1/ping",
            StatusCode::OK,
            "application/json",
            r#"{"service":"authServer","env":"production"}"#.to_owned(),
        )
        .merge(answering(
            "/b/v1/ping",
            StatusCode::OK,
            "application/json",
            r#"{"service":"keyServer","env":"production"}"#.to_owned(),
        )),
    )
    .await;

    let wrong_service = PingProbe::new(
        reqwest::Client::new(),
        format!("{base}/a"),
        "launcher",
        key_server(),
    )
    .run()
    .await;
    assert_eq!(
        wrong_service.status,
        CheckStatus::WrongTarget,
        "{wrong_service:?}"
    );
    assert_eq!(
        wrong_service.reason.as_deref(),
        Some("wrong_service"),
        "the service is checked before the env"
    );
    assert_eq!(wrong_service.http_status, Some(200));
    assert_eq!(wrong_service.expected.as_ref(), Some(&key_server()));
    assert_eq!(
        wrong_service.observed.as_ref(),
        Some(&Identity::new("authServer", "production"))
    );

    let wrong_env = PingProbe::new(
        reqwest::Client::new(),
        format!("{base}/b"),
        "launcher",
        key_server(),
    )
    .run()
    .await;
    assert_eq!(wrong_env.status, CheckStatus::WrongTarget, "{wrong_env:?}");
    assert_eq!(wrong_env.reason.as_deref(), Some("wrong_env"));
    assert_eq!(wrong_env.expected.as_ref(), Some(&key_server()));
    assert_eq!(
        wrong_env.observed.as_ref(),
        Some(&Identity::new("keyServer", "production"))
    );
}

// 4. A 200 that is not the ping body — the SPA shell a generic host serves
//    for every path — is unexpected with reason bad_body.
#[tokio::test]
async fn ping_probe_rejects_an_html_answer() {
    let (base, _) = fake(answering(
        "/keys/v1/ping",
        StatusCode::OK,
        "text/html",
        "<html><body>the spa shell answers every path</body></html>".to_owned(),
    ))
    .await;

    let outcome = PingProbe::new(
        reqwest::Client::new(),
        format!("{base}/keys"),
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_eq!(outcome.status, CheckStatus::Unexpected, "{outcome:?}");
    assert_eq!(outcome.reason.as_deref(), Some("bad_body"));
    assert_eq!(outcome.http_status, Some(200));
}

// 5. A 404 is unexpected with reason not_found: the path or the callee's
//    BASE_PATH is wrong, which our configuration did.
#[tokio::test]
async fn ping_probe_reports_not_found_on_404() {
    let (base, _) = fake(answering(
        "/keys/v1/ping",
        StatusCode::NOT_FOUND,
        "text/plain",
        "no such route".to_owned(),
    ))
    .await;

    let outcome = PingProbe::new(
        reqwest::Client::new(),
        format!("{base}/keys"),
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_failure(&outcome, CheckStatus::Unexpected, "not_found");
    assert_eq!(outcome.http_status, Some(404));
}

// 6. A 5xx is unreachable with reason upstream_5xx: whether our
//    configuration is right cannot be told from it.
#[tokio::test]
async fn ping_probe_reports_upstream_5xx_on_503() {
    let (base, _) = fake(answering(
        "/keys/v1/ping",
        StatusCode::SERVICE_UNAVAILABLE,
        "text/plain",
        "maintenance".to_owned(),
    ))
    .await;

    let outcome = PingProbe::new(
        reqwest::Client::new(),
        format!("{base}/keys"),
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_failure(&outcome, CheckStatus::Unreachable, "upstream_5xx");
    assert_eq!(outcome.http_status, Some(503));
}

// 7. A probe reads at most 64 KiB of a body (CONTRACT.md §6.2): valid ping
//    JSON padded past that is bad_body, not ok.
#[tokio::test]
async fn ping_probe_reads_at_most_64_kib_of_a_body() {
    let padded = format!(
        r#"{{"service":"keyServer","env":"development","pad":"{}"}}"#,
        "a".repeat(64 * 1024)
    );
    assert!(padded.len() > 64 * 1024, "the body must exceed the bound");
    let (base, _) = fake(answering(
        "/keys/v1/ping",
        StatusCode::OK,
        "application/json",
        padded,
    ))
    .await;

    let outcome = PingProbe::new(
        reqwest::Client::new(),
        format!("{base}/keys"),
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_eq!(outcome.status, CheckStatus::Unexpected, "{outcome:?}");
    assert_eq!(outcome.reason.as_deref(), Some("bad_body"));
    assert_eq!(outcome.http_status, Some(200));
}

// 8. A base URL that is not an absolute http(s) URL is misconfigured before
//    any network call: no request is ever made.
#[tokio::test]
async fn ping_probe_rejects_non_http_base_urls() {
    for base in ["not a url", "ftp://h/keys"] {
        let outcome = PingProbe::new(reqwest::Client::new(), base, "launcher", key_server())
            .run()
            .await;
        assert_eq!(
            outcome.status,
            CheckStatus::Misconfigured,
            "base {base:?}: {outcome:?}"
        );
        assert_eq!(
            outcome.reason.as_deref(),
            Some("invalid_url"),
            "base {base:?}"
        );
        assert_eq!(
            outcome.http_status, None,
            "base {base:?}: no HTTP answer was ever received"
        );
    }
}

// 9. Userinfo in the base URL is a credential: it never reaches the wire
//    (not even as basic auth), and target() never shows it either.
#[tokio::test]
async fn ping_probe_strips_userinfo_from_the_request_and_the_target() {
    let (base, seen) = fake(answering(
        "/keys/v1/ping",
        StatusCode::OK,
        "application/json",
        r#"{"service":"keyServer","env":"development"}"#.to_owned(),
    ))
    .await;
    let host = base.trim_start_matches("http://");

    let probe = PingProbe::new(
        reqwest::Client::new(),
        format!("http://user:pw@{host}/keys"),
        "launcher",
        key_server(),
    );

    let target = Probe::target(&probe).expect("the target sanitizes");
    assert_eq!(target, format!("{base}/keys"));
    assert!(
        !target.contains("user"),
        "no credential in the body: {target}"
    );
    assert!(
        !target.contains("pw"),
        "no credential in the body: {target}"
    );

    let outcome = probe.run().await;
    assert_eq!(outcome.status, CheckStatus::Ok, "{outcome:?}");
    assert_eq!(
        the_one(&seen).header("authorization"),
        None,
        "userinfo must not become basic auth (CONTRACT.md §1.1)"
    );
}

// 10. Connection refused: a port that just stopped listening.
#[tokio::test]
async fn ping_probe_reports_connect_on_connection_refused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port is bound");
    let port = listener.local_addr().expect("the port is known").port();
    drop(listener); // nothing listens there anymore

    let outcome = PingProbe::new(
        reqwest::Client::new(),
        format!("http://127.0.0.1:{port}/keys"),
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_failure(&outcome, CheckStatus::Unreachable, "connect");
    assert_eq!(outcome.http_status, None);
}

// 11. DNS failure: a host that cannot resolve. Resolvers report differently
//     per platform, so dns and connect are both acceptable; the test prints
//     which one this host produced.
#[tokio::test]
async fn ping_probe_reports_dns_on_resolution_failure() {
    let outcome = PingProbe::new(
        reqwest::Client::new(),
        "http://does-not-exist.invalid/keys",
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_eq!(outcome.status, CheckStatus::Unreachable, "{outcome:?}");
    let reason = outcome.reason.as_deref().unwrap_or("<none>");
    println!("resolution of does-not-exist.invalid classified as {reason:?}");
    assert!(
        matches!(reason, "dns" | "connect"),
        "expected dns (or connect on an odd resolver), got {reason:?}"
    );
    assert_eq!(outcome.http_status, None);
}

// 12. A client timeout is unreachable with reason timeout: the fake handler
//     sleeps 5 s, the probe's own client gives up after 200 ms.
#[tokio::test]
async fn ping_probe_reports_timeout_when_the_client_gives_up() {
    let (base, _) = fake(slow("/keys/v1/ping", 5)).await;
    let timed_client = reqwest::Client::builder()
        .timeout(Duration::from_millis(200))
        .build()
        .expect("the timed client builds");

    let started = Instant::now();
    let outcome = PingProbe::new(
        timed_client,
        format!("{base}/keys"),
        "launcher",
        key_server(),
    )
    .run()
    .await;

    assert_failure(&outcome, CheckStatus::Unreachable, "timeout");
    assert_eq!(outcome.http_status, None);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the probe returned after {:?}, not the handler's 5 s",
        started.elapsed()
    );
}

// ----------------------------------------------------------------------
// JwksProbe
// ----------------------------------------------------------------------

// 13. A JWK Set with at least one key is ok; an empty keys array is
//     no_keys; anything without a keys array is bad_body; a 404 is
//     not_found.
#[tokio::test]
async fn jwks_probe_classifies_key_sets() {
    let (base, _) = fake(
        answering(
            "/jwks/ok",
            StatusCode::OK,
            "application/json",
            r#"{"keys":[{"kty":"OKP","crv":"Ed25519"}]}"#.to_owned(),
        )
        .merge(answering(
            "/jwks/empty",
            StatusCode::OK,
            "application/json",
            r#"{"keys":[]}"#.to_owned(),
        ))
        .merge(answering(
            "/jwks/bad",
            StatusCode::OK,
            "application/json",
            r#"{"foo":1}"#.to_owned(),
        ))
        .merge(answering(
            "/jwks/gone",
            StatusCode::NOT_FOUND,
            "text/plain",
            "no such file".to_owned(),
        )),
    )
    .await;

    let probe = |url: String| JwksProbe::new(reqwest::Client::new(), url, "launcher");

    let ok = probe(format!("{base}/jwks/ok")).run().await;
    assert_eq!(ok.status, CheckStatus::Ok, "{ok:?}");
    assert_eq!(ok.reason, None);
    assert_eq!(ok.http_status, Some(200));

    let no_keys = probe(format!("{base}/jwks/empty")).run().await;
    assert_eq!(no_keys.status, CheckStatus::Unexpected, "{no_keys:?}");
    assert_eq!(no_keys.reason.as_deref(), Some("no_keys"));
    assert_eq!(no_keys.http_status, Some(200));

    let bad = probe(format!("{base}/jwks/bad")).run().await;
    assert_eq!(bad.status, CheckStatus::Unexpected, "{bad:?}");
    assert_eq!(bad.reason.as_deref(), Some("bad_body"));
    assert_eq!(bad.http_status, Some(200));

    let gone = probe(format!("{base}/jwks/gone")).run().await;
    assert_eq!(gone.status, CheckStatus::Unexpected, "{gone:?}");
    assert_eq!(gone.reason.as_deref(), Some("not_found"));
    assert_eq!(gone.http_status, Some(404));
}

// ----------------------------------------------------------------------
// AuthEndpointProbe
// ----------------------------------------------------------------------

// 14. The endpoint's own 401/403 is ok (the address is right); a 404 is
//     not_found; a 2xx is unexpected_status, because an auth-protected
//     route answering an unauthenticated request means the URL points
//     somewhere else; a 5xx is upstream_5xx. On the wire: POST, an empty
//     JSON body, and no credential.
#[tokio::test]
async fn auth_endpoint_probe_posts_empty_json_and_classifies_auth_answers() {
    let (base, seen) = fake(
        answering(
            "/verify/401",
            StatusCode::UNAUTHORIZED,
            "application/json",
            "{}".to_owned(),
        )
        .merge(answering(
            "/verify/403",
            StatusCode::FORBIDDEN,
            "application/json",
            "{}".to_owned(),
        ))
        .merge(answering(
            "/verify/404",
            StatusCode::NOT_FOUND,
            "text/plain",
            "no route".to_owned(),
        ))
        .merge(answering(
            "/verify/200",
            StatusCode::OK,
            "application/json",
            "{}".to_owned(),
        ))
        .merge(answering(
            "/verify/500",
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain",
            "boom".to_owned(),
        )),
    )
    .await;

    let probe = |url: String| AuthEndpointProbe::new(reqwest::Client::new(), url, "launcher");

    let unauthorized = probe(format!("{base}/verify/401")).run().await;
    assert_eq!(unauthorized.status, CheckStatus::Ok, "{unauthorized:?}");
    assert_eq!(unauthorized.reason, None);
    assert_eq!(unauthorized.http_status, Some(401));

    let forbidden = probe(format!("{base}/verify/403")).run().await;
    assert_eq!(forbidden.status, CheckStatus::Ok, "{forbidden:?}");
    assert_eq!(forbidden.http_status, Some(403));

    let gone = probe(format!("{base}/verify/404")).run().await;
    assert_failure(&gone, CheckStatus::Unexpected, "not_found");
    assert_eq!(gone.http_status, Some(404));

    let open = probe(format!("{base}/verify/200")).run().await;
    assert_eq!(
        open.status,
        CheckStatus::Unexpected,
        "{open:?}: an unauthenticated 2xx means the URL is wrong"
    );
    assert_eq!(open.reason.as_deref(), Some("unexpected_status"));
    assert_eq!(open.http_status, Some(200));

    let broken = probe(format!("{base}/verify/500")).run().await;
    assert_failure(&broken, CheckStatus::Unreachable, "upstream_5xx");
    assert_eq!(broken.http_status, Some(500));

    let requests = take(&seen);
    assert_eq!(requests.len(), 5, "one probe request per case");
    for request in &requests {
        assert_eq!(request.method, "POST", "{request:?}");
        assert_eq!(
            request.header("content-type"),
            Some("application/json"),
            "an empty JSON body is sent: {request:?}"
        );
        assert_eq!(request.body_str(), "{}", "the body is exactly {{}}");
        assert_eq!(
            request.header("authorization"),
            None,
            "no credential, or the 401/403 would prove nothing: {request:?}"
        );
        assert_eq!(request.header("x-api-key"), None, "{request:?}");
        assert_eq!(
            request.header(PROBE_HEADER),
            Some("launcher"),
            "{request:?}"
        );
    }
}

// ----------------------------------------------------------------------
// ChainIdProbe
// ----------------------------------------------------------------------

// 15. eth_chainId against the configured chain; the §6.3 rule 4 minimum
//     interval of 60 s; and a display target that hides an API key in the
//     URL path while the request still goes to the configured URL.
#[tokio::test]
async fn chain_id_probe_classifies_chains_and_hides_url_secrets() {
    let (base, seen) = fake(
        answering(
            "/rpc/chain",
            StatusCode::OK,
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"result":"0x61"}"#.to_owned(),
        )
        .merge(answering(
            "/rpc/error",
            StatusCode::OK,
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#
                .to_owned(),
        ))
        .merge(answering(
            "/v1/SECRETKEY",
            StatusCode::OK,
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"result":"0x61"}"#.to_owned(),
        )),
    )
    .await;

    let probe =
        |url: String, chain: u64| ChainIdProbe::new(reqwest::Client::new(), url, "launcher", chain);

    let right_chain = probe(format!("{base}/rpc/chain"), 97).run().await;
    assert_eq!(right_chain.status, CheckStatus::Ok, "{right_chain:?}");
    assert_eq!(right_chain.reason, None);
    assert_eq!(right_chain.http_status, Some(200));

    let wrong_chain = probe(format!("{base}/rpc/chain"), 56).run().await;
    assert_eq!(
        wrong_chain.status,
        CheckStatus::WrongTarget,
        "{wrong_chain:?}"
    );
    assert_eq!(wrong_chain.reason.as_deref(), Some("wrong_chain"));
    assert_eq!(wrong_chain.http_status, Some(200));

    let errored = probe(format!("{base}/rpc/error"), 97).run().await;
    assert_failure(&errored, CheckStatus::Unexpected, "bad_body");
    assert_eq!(errored.http_status, Some(200));

    // A chain RPC hop is probed at most every 60 s (CONTRACT.md §6.3
    // rule 4).
    assert_eq!(
        Probe::min_interval(&probe(format!("{base}/rpc/chain"), 97)),
        Some(Duration::from_secs(60))
    );

    // A URL whose path embeds an API key is requested as configured, but
    // the target the body reports is the display one and hides the key.
    let configured = format!("{base}/v1/SECRETKEY");
    let with_display = probe(configured.clone(), 97).with_target("https://bsc.example/");
    let target = Probe::target(&with_display).expect("the display target sanitizes");
    assert_eq!(target, "https://bsc.example/");
    assert!(
        !target.contains("SECRETKEY"),
        "no secret in the body: {target}"
    );

    let outcome = with_display.run().await;
    assert_eq!(
        outcome.status,
        CheckStatus::Ok,
        "{outcome:?}: the request still goes to the configured URL"
    );
    assert_eq!(the_last(&seen).path, "/v1/SECRETKEY");

    // Without the override the configured URL is what shows, which is why
    // the override exists.
    assert!(Probe::target(&probe(configured, 97))
        .expect("the configured target sanitizes")
        .contains("SECRETKEY"));
}

// ----------------------------------------------------------------------
// ReachProbe
// ----------------------------------------------------------------------

// 16. Any HTTP answer below 500 is ok — including the 403 an unsigned
//     object storage request gets, and a 200 from a provider's health
//     endpoint — a 5xx is upstream_5xx, head() really sends HEAD, and the
//     hop can carry its own minimum interval.
#[tokio::test]
async fn reach_probe_accepts_any_answer_below_500() {
    let (base, seen) = fake(
        answering(
            "/storage",
            StatusCode::FORBIDDEN,
            "application/xml",
            "<Error><Code>AccessDenied</Code></Error>".to_owned(),
        )
        .merge(answering(
            "/health",
            StatusCode::OK,
            "application/json",
            r#"{"status":"up"}"#.to_owned(),
        ))
        .merge(answering(
            "/broken",
            StatusCode::BAD_GATEWAY,
            "text/html",
            "<html>bad gateway</html>".to_owned(),
        )),
    )
    .await;

    let head = ReachProbe::head(
        reqwest::Client::new(),
        format!("{base}/storage"),
        "launcher",
    )
    .run()
    .await;
    assert_eq!(head.status, CheckStatus::Ok, "{head:?}");
    assert_eq!(head.reason, None);
    assert_eq!(head.http_status, Some(403));
    assert_eq!(the_one(&seen).method, "HEAD", "head() sends HEAD");

    let get = ReachProbe::get(reqwest::Client::new(), format!("{base}/health"), "launcher")
        .run()
        .await;
    assert_eq!(get.status, CheckStatus::Ok, "{get:?}");
    assert_eq!(get.http_status, Some(200));

    let broken = ReachProbe::get(reqwest::Client::new(), format!("{base}/broken"), "launcher")
        .run()
        .await;
    assert_failure(&broken, CheckStatus::Unreachable, "upstream_5xx");
    assert_eq!(broken.http_status, Some(502));

    // The hop's minimum interval is whatever the builder was given
    // (CONTRACT.md §6.3 rule 4); a plain reach probe has none.
    let plain = ReachProbe::head(
        reqwest::Client::new(),
        format!("{base}/storage"),
        "launcher",
    );
    assert_eq!(Probe::min_interval(&plain), None);
    let unhurried = ReachProbe::head(
        reqwest::Client::new(),
        format!("{base}/storage"),
        "launcher",
    )
    .min_interval(Duration::from_secs(300));
    assert_eq!(
        Probe::min_interval(&unhurried),
        Some(Duration::from_secs(300))
    );
}

// ----------------------------------------------------------------------
// dedupe_key
// ----------------------------------------------------------------------

// 17. Two PingProbes with the same base URL share a dedupe key, so the
//     runner checks that hop once per cycle; a different base URL is a
//     different hop.
#[test]
fn ping_probe_dedupe_key_is_the_base_url() {
    let client = reqwest::Client::new();
    let a = PingProbe::new(
        client.clone(),
        "http://key-server.platform.svc:8092/keys",
        "launcher",
        key_server(),
    );
    let b = PingProbe::new(
        client.clone(),
        "http://key-server.platform.svc:8092/keys",
        "launcher",
        key_server(),
    );
    let c = PingProbe::new(
        client,
        "http://key-server-2.platform.svc:8092/keys",
        "launcher",
        key_server(),
    );

    assert_eq!(
        Probe::dedupe_key(&a),
        Probe::dedupe_key(&b),
        "the same base URL is the same hop"
    );
    assert_ne!(
        Probe::dedupe_key(&a),
        Probe::dedupe_key(&c),
        "a different base URL is a different hop"
    );
}

// ----------------------------------------------------------------------
// Through Readiness
// ----------------------------------------------------------------------

// 18. The same probe reporting through Readiness: one run via
//     run_reporting_once, and the body degrades (a reporting hop never
//     changes the HTTP status) with the probe's classification and
//     sanitized target, validating against schema/readyz.schema.json.
#[tokio::test]
async fn ping_probe_through_readiness_degrades_with_a_schema_valid_body() {
    let (base, _) = fake(answering(
        "/keys/v1/ping",
        StatusCode::NOT_FOUND,
        "text/plain",
        "no such route".to_owned(),
    ))
    .await;

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report(
            "key_server",
            PingProbe::new(
                reqwest::Client::new(),
                format!("{base}/keys"),
                "launcher",
                key_server(),
            ),
        )
        .build();
    readiness.run_reporting_once().await;

    let (code, body) = readiness.body().await;
    assert_eq!(
        code,
        StatusCode::OK,
        "a reporting hop never fails readiness"
    );
    assert_eq!(body["status"], "degraded");
    let check = &body["checks"]["key_server"];
    assert_eq!(check["status"], "unexpected");
    assert_eq!(check["reason"], "not_found");
    assert_eq!(check["httpStatus"], 404);
    assert_eq!(
        check["target"],
        format!("{base}/keys"),
        "the target names where the probe went"
    );
    assert!(
        check["checkedAt"].is_string(),
        "a reporting check says when"
    );
    assert_schema(&body);
}

// ----------------------------------------------------------------------
// The pure classification the probes share
// ----------------------------------------------------------------------

// classify_error_status: a 5xx is unreachable/upstream_5xx, a 404 is
// unexpected/not_found, every other unaccepted answer is
// unexpected/unexpected_status; each outcome carries the status it
// classified.
#[test]
fn classify_error_status_covers_5xx_404_and_the_rest() {
    let upstream = classify_error_status(503);
    assert_eq!(upstream.status, CheckStatus::Unreachable);
    assert_eq!(upstream.reason.as_deref(), Some("upstream_5xx"));
    assert_eq!(upstream.http_status, Some(503));

    let not_found = classify_error_status(404);
    assert_eq!(not_found.status, CheckStatus::Unexpected);
    assert_eq!(not_found.reason.as_deref(), Some("not_found"));
    assert_eq!(not_found.http_status, Some(404));

    // A 3xx the client did not follow, a 2xx that is not the one accepted
    // status, a 4xx that is not 404: all unexpected_status.
    for status in [200, 301, 400, 418, 499] {
        let other = classify_error_status(status);
        assert_eq!(other.status, CheckStatus::Unexpected, "status {status}");
        assert_eq!(
            other.reason.as_deref(),
            Some("unexpected_status"),
            "status {status}"
        );
        assert_eq!(other.http_status, Some(status), "status {status}");
    }
}
