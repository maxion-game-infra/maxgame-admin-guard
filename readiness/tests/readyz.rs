//! Integration tests for `/readyz` (CONTRACT.md §3 and §6).
//!
//! Every body these tests observe — from [`Readiness::body`] or from the
//! router — is validated against `schema/readyz.schema.json`.

use std::future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use jsonschema::Validator;
use maxgame_readiness::{AppEnv, CheckStatus, FnProbe, Probe, ProbeOutcome, Readiness};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Validate a `/readyz` body against the contract's JSON Schema
/// (`schema/readyz.schema.json`).
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

/// A probe that counts its runs in `counter` and returns `outcome()`
/// (`fn` so several hops can share one counter). Optionally carries a
/// dedupe key (CONTRACT.md §6.3 rule 3).
fn counted(
    counter: &Arc<AtomicUsize>,
    outcome: fn() -> ProbeOutcome,
    dedupe_key: Option<&str>,
) -> impl Probe {
    let counter = Arc::clone(counter);
    let mut probe = FnProbe::new(move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            outcome()
        }
    });
    if let Some(key) = dedupe_key {
        probe = probe.dedupe_key(key);
    }
    probe
}

/// A probe that counts its runs and then panics, to test panic safety.
fn counted_panic(counter: &Arc<AtomicUsize>) -> impl Probe {
    let counter = Arc::clone(counter);
    FnProbe::new(move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            panic!("probe exploded");
        }
    })
}

/// A probe that always succeeds, but only after sleeping for `secs`.
fn sleeping_ok(secs: u64) -> impl Probe {
    FnProbe::new(move || async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        ProbeOutcome::ok()
    })
}

/// A concrete `FnProbe` type, so `optional(name, None::<UnitOkProbe>)` can
/// name what "no probe" is.
type UnitOkProbe = FnProbe<fn() -> future::Ready<ProbeOutcome>>;

fn unit_ok() -> UnitOkProbe {
    let f: fn() -> future::Ready<ProbeOutcome> = || future::ready(ProbeOutcome::ok());
    FnProbe::new(f)
}

// 1. Everything ok after one background run: 200 "ready", service and env
//    present, env serialized as the parsed tier.
#[tokio::test]
async fn all_ok_after_one_run_is_ready() {
    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .version("1.2.3")
        .gate("postgres", FnProbe::new(|| async { ProbeOutcome::ok() }))
        .report("key_server", FnProbe::new(|| async { ProbeOutcome::ok() }))
        .build();

    readiness.run_reporting_once().await;
    let (code, body) = readiness.body().await;

    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["service"], "launcher");
    assert_eq!(body["env"], "development", "env is the parsed tier");
    assert_eq!(body["version"], "1.2.3");
    assert!(body["dependency"].is_null());
    assert_eq!(body["checks"]["postgres"]["status"], "ok");
    assert_eq!(body["checks"]["postgres"]["gate"], true);
    assert_eq!(body["checks"]["key_server"]["status"], "ok");
    assert_eq!(body["checks"]["key_server"]["gate"], false);
    assert_eq!(body["checks"]["key_server"]["required"], true);
    assert!(
        body["checks"]["key_server"]["checkedAt"].is_string(),
        "a reporting check says when it was checked"
    );
    assert_schema(&body);
}

// 2. A required reporting hop answering 404 on the probe path is
//    `unexpected` with reason `not_found` (CONTRACT.md §3.5) and degrades,
//    but `/readyz` stays 200.
#[tokio::test]
async fn required_reporting_unexpected_degrades_but_stays_200() {
    let readiness = Readiness::builder("launcher", AppEnv::Development).report(
        "key_server",
        FnProbe::new(|| async {
            ProbeOutcome::fail(CheckStatus::Unexpected, "not_found").with_http_status(404)
        })
        .target("http://key-server.platform.svc:8092/keys?api_key=mxs_secret#frag"),
    );

    let readiness = readiness.build();
    readiness.run_reporting_once().await;
    let (code, body) = readiness.body().await;

    assert_eq!(code, StatusCode::OK, "degraded is still HTTP 200");
    assert_eq!(body["status"], "degraded");
    let check = &body["checks"]["key_server"];
    assert_eq!(check["status"], "unexpected");
    assert_eq!(check["reason"], "not_found");
    assert_eq!(check["httpStatus"], 404);
    assert_eq!(
        check["target"], "http://key-server.platform.svc:8092/keys",
        "userinfo, query and fragment are stripped from target"
    );
    assert!(check["required"].is_boolean() && check["required"] == json!(true));
    assert_schema(&body);
}

// 3. A failing gate answers 503 `unavailable`, and `dependency` is the
//    first failing gate in registration order.
#[tokio::test]
async fn dependency_is_the_first_failing_gate_in_registration_order() {
    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .gate(
            "postgres",
            FnProbe::new(|| async { ProbeOutcome::fail(CheckStatus::Unreachable, "connect") }),
        )
        .gate(
            "redis",
            FnProbe::new(|| async { ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body") }),
        )
        .report("key_server", FnProbe::new(|| async { ProbeOutcome::ok() }))
        .build();

    readiness.run_reporting_once().await;
    let (code, body) = readiness.body().await;

    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "unavailable");
    assert_eq!(
        body["dependency"], "postgres",
        "the first failing gate in registration order names the dependency"
    );
    assert_eq!(body["checks"]["postgres"]["status"], "unreachable");
    assert_eq!(body["checks"]["postgres"]["reason"], "connect");
    assert_eq!(body["checks"]["redis"]["status"], "unexpected");
    assert_eq!(body["checks"]["redis"]["reason"], "bad_body");
    assert_eq!(body["checks"]["key_server"]["status"], "ok");
    assert_schema(&body);
}

// 4. A gate probe that blocks is bounded by the probe timeout: `unreachable`
//    with reason `timeout`, and the request itself returns quickly.
#[tokio::test]
async fn gate_timeout_is_bounded_and_reports_unreachable() {
    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .gate("postgres", sleeping_ok(10))
        .probe_timeout(Duration::from_millis(200))
        .build();

    let started = Instant::now();
    let (code, body) = readiness.body().await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "body() must be bounded by the probe timeout, took {elapsed:?}"
    );
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "unavailable");
    assert_eq!(body["dependency"], "postgres");
    assert_eq!(body["checks"]["postgres"]["status"], "unreachable");
    assert_eq!(body["checks"]["postgres"]["reason"], "timeout");
    assert_schema(&body);
}

// 5. Draining answers exactly {"status": "draining"} with 503 and runs no
//    probe at all.
#[tokio::test]
async fn draining_answers_exactly_and_runs_no_probe() {
    let gate_runs = Arc::new(AtomicUsize::new(0));
    let report_runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .gate("postgres", counted(&gate_runs, ProbeOutcome::ok, None))
        .report("key_server", counted(&report_runs, ProbeOutcome::ok, None))
        .build();

    readiness.run_reporting_once().await;
    let (code, _) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    let gate_runs_before = gate_runs.load(Ordering::SeqCst);
    let report_runs_before = report_runs.load(Ordering::SeqCst);
    assert!(report_runs_before >= 1);

    readiness.set_draining();
    assert!(readiness.is_draining());

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, json!({ "status": "draining" }), "exactly this body");
    assert_schema(&body);

    // A background cycle while draining also runs nothing.
    readiness.run_reporting_once().await;
    assert_eq!(gate_runs.load(Ordering::SeqCst), gate_runs_before);
    assert_eq!(report_runs.load(Ordering::SeqCst), report_runs_before);
}

// 6. Before any run (no spawn, no run_reporting_once) a reporting hop is
//    `pending`, which does not degrade.
#[tokio::test]
async fn before_the_first_run_a_reporting_hop_is_pending() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .build();

    let (code, body) = readiness.body().await;

    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "ready", "pending does not degrade");
    assert_eq!(body["checks"]["key_server"]["status"], "pending");
    assert_eq!(body["checks"]["key_server"]["gate"], false);
    assert_eq!(body["checks"]["key_server"]["required"], true);
    assert_eq!(runs.load(Ordering::SeqCst), 0, "no probe has run");
    assert_schema(&body);
}

// 7. A cached result older than three intervals is `stale`, which degrades.
#[tokio::test]
async fn stale_after_three_intervals_degrades() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .interval_unclamped(Duration::from_millis(50))
        .build();

    readiness.run_reporting_once().await;
    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["checks"]["key_server"]["status"], "ok");

    // More than 3 x interval without a re-run and without a runner.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let runs_after = runs.load(Ordering::SeqCst);
    let (code, body) = readiness.body().await;
    assert_eq!(runs.load(Ordering::SeqCst), runs_after, "no probe re-ran");
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["checks"]["key_server"]["status"], "stale");
    assert_schema(&body);
}

// 8. Hops whose probes share a dedupe key share one run per cycle; keyless
//    hops never join a group (CONTRACT.md §6.3 rule 3).
#[tokio::test]
async fn hops_sharing_a_dedupe_key_run_one_probe() {
    let shared_runs = Arc::new(AtomicUsize::new(0));
    let left_runs = Arc::new(AtomicUsize::new(0));
    let right_runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report(
            "key_server",
            counted(&shared_runs, ProbeOutcome::ok, Some("same")),
        )
        .report(
            "key_server_internal",
            counted(&shared_runs, ProbeOutcome::ok, Some("same")),
        )
        .report("postgres_read", counted(&left_runs, ProbeOutcome::ok, None))
        .report("redis_cache", counted(&right_runs, ProbeOutcome::ok, None))
        .build();

    readiness.run_reporting_once().await;

    assert_eq!(
        shared_runs.load(Ordering::SeqCst),
        1,
        "two hops with dedupe_key(\"same\") share one probe run"
    );
    assert_eq!(left_runs.load(Ordering::SeqCst), 1);
    assert_eq!(right_runs.load(Ordering::SeqCst), 1);

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    for hop in [
        "key_server",
        "key_server_internal",
        "postgres_read",
        "redis_cache",
    ] {
        assert_eq!(
            body["checks"][hop]["status"], "ok",
            "{hop} got the shared result"
        );
    }
    assert_schema(&body);
}

// 9. A request to /readyz never makes an upstream call (CONTRACT.md §6.1):
//    ten bodies, zero additional probe runs.
#[tokio::test]
async fn body_reads_the_cache_and_never_probes() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .build();

    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    for attempt in 0..10 {
        let (code, body) = readiness.body().await;
        assert_eq!(code, StatusCode::OK, "attempt {attempt}");
        assert_eq!(body["status"], "ready");
        assert_eq!(body["checks"]["key_server"]["status"], "ok");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "body() must serve the cache, not the upstream"
        );
        assert_schema(&body);
    }
}

// 10. An optional hop without a probe is `not_configured` with
//     `required: false` and never degrades; with a probe it is a required
//     reporting hop.
#[tokio::test]
async fn optional_hop_without_probe_is_not_configured() {
    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .optional("r2", None::<UnitOkProbe>)
        .optional("google_oauth", Some(unit_ok()))
        .build();

    readiness.run_reporting_once().await;
    let (code, body) = readiness.body().await;

    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        body["status"], "ready",
        "a switched-off optional hop never degrades"
    );
    assert_eq!(body["checks"]["r2"]["status"], "not_configured");
    assert_eq!(body["checks"]["r2"]["required"], false);
    assert_eq!(body["checks"]["r2"]["gate"], false);
    assert_eq!(body["checks"]["google_oauth"]["status"], "ok");
    assert_eq!(body["checks"]["google_oauth"]["required"], true);
    assert_eq!(body["checks"]["google_oauth"]["gate"], false);
    assert_schema(&body);
}

// 11. A panicking probe is `unexpected` with reason `probe_panicked`, does
//     not take the runner down, and later cycles still run.
#[tokio::test]
async fn panicking_probe_is_unexpected_and_survivable() {
    let panic_runs = Arc::new(AtomicUsize::new(0));
    let ok_runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted_panic(&panic_runs))
        .report("auth_server", counted(&ok_runs, ProbeOutcome::ok, None))
        // Short enough that a second run_reporting_once finds the hops due.
        .interval_unclamped(Duration::from_millis(2))
        .build();

    readiness.run_reporting_once().await;
    let (code, body) = readiness.body().await;

    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["checks"]["key_server"]["status"], "unexpected");
    assert_eq!(body["checks"]["key_server"]["reason"], "probe_panicked");
    assert_eq!(
        body["checks"]["auth_server"]["status"], "ok",
        "the other hops of the same cycle still ran"
    );
    assert_eq!(panic_runs.load(Ordering::SeqCst), 1);
    assert_eq!(ok_runs.load(Ordering::SeqCst), 1);
    assert_schema(&body);

    // The panic did not poison anything: the next cycle runs both again.
    tokio::time::sleep(Duration::from_millis(10)).await;
    readiness.run_reporting_once().await;
    assert_eq!(panic_runs.load(Ordering::SeqCst), 2);
    assert_eq!(ok_runs.load(Ordering::SeqCst), 2);

    let (_, body) = readiness.body().await;
    assert_eq!(body["checks"]["key_server"]["status"], "unexpected");
    assert_eq!(body["checks"]["auth_server"]["status"], "ok");
    assert_schema(&body);
}

// 12. The background runner repeats on its schedule and stops when the
//     process drains.
#[tokio::test]
async fn spawn_repeats_the_schedule_and_stops_on_draining() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .interval_unclamped(Duration::from_millis(20))
        .build();

    let runner = readiness.spawn();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let observed = runs.load(Ordering::SeqCst);
    assert!(
        observed > 1,
        "expected more than one background run in ~300 ms, saw {observed}"
    );

    readiness.set_draining();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("the runner finishes soon after draining")
        .expect("the runner task did not panic");
}

// 13. The router serves the same answer as body(), with the headers the
//     contract requires.
#[tokio::test]
async fn router_serves_the_body_with_no_store() {
    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .version("1.2.3")
        .gate("postgres", FnProbe::new(|| async { ProbeOutcome::ok() }))
        .report(
            "key_server",
            FnProbe::new(|| async {
                ProbeOutcome::fail(CheckStatus::Unexpected, "not_found").with_http_status(404)
            }),
        )
        .build();

    readiness.run_reporting_once().await;
    let (expected_code, expected_body) = readiness.body().await;
    assert_schema(&expected_body);

    let response = readiness
        .router::<()>()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/readyz")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("the router answers");

    assert_eq!(
        response.status(),
        expected_code,
        "same status code as body()"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "readyz answers must not be cached"
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
        .expect("body collects")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("the router body is JSON");
    assert_eq!(body["status"], expected_body["status"]);
    assert_eq!(body["service"], expected_body["service"]);
    assert_eq!(body["env"], expected_body["env"]);
    assert_eq!(body["checks"]["key_server"]["status"], "unexpected");
    assert_schema(&body);
}

// A service whose `/readyz` is reachable from the internet (an ingress that
// routes `path: /` to it) can drop `target` from every check, so the body
// names no internal host. Everything else in the check stays.
#[tokio::test]
async fn omit_targets_drops_target_from_every_check_and_keeps_the_rest() {
    let target = "http://admin-auth.platform.svc:8090/.well-known/jwks.json";
    let build = |omit: bool| {
        let builder = Readiness::builder("csServer", AppEnv::Development)
            .gate(
                "postgres",
                FnProbe::new(|| async { ProbeOutcome::ok() }).target(target),
            )
            .report(
                "admin_jwks",
                FnProbe::new(|| async { ProbeOutcome::fail(CheckStatus::Unreachable, "timeout") })
                    .target(target),
            );
        if omit {
            builder.omit_targets().build()
        } else {
            builder.build()
        }
    };

    let shown = build(false);
    shown.run_reporting_once().await;
    let (_, body) = shown.body().await;
    assert_eq!(body["checks"]["admin_jwks"]["target"], target, "{body}");

    let hidden = build(true);
    hidden.run_reporting_once().await;
    let (code, body) = hidden.body().await;
    assert_eq!(code, StatusCode::OK);
    assert!(!body.to_string().contains("platform.svc"), "{body}");
    for name in ["postgres", "admin_jwks"] {
        assert!(body["checks"][name].get("target").is_none(), "{body}");
    }
    let jwks = &body["checks"]["admin_jwks"];
    assert_eq!(jwks["status"], "unreachable", "{body}");
    assert_eq!(jwks["reason"], "timeout", "{body}");
    assert_eq!(body["status"], "degraded", "{body}");
    assert_schema(&body);
}
