//! Integration tests for the §6.4 rule: a real request that fails with a
//! 5xx, a timeout or a connection/DNS/TLS error re-checks its hop ahead of
//! the interval, at most once per cooldown, and never blocks the request.
//!
//! Since 138cbbd, §6.4 also says only a hop that still looks healthy is
//! re-checked (`ok`, `pending` or `stale` cached status), and the cooldown
//! backs off exponentially: it doubles after a triggered check that comes
//! back `ok`, capped at the hop's effective interval, and returns to its
//! base value after a triggered check that fails or after one interval
//! without a signal.
//!
//! Every body these tests observe is validated against
//! `schema/readyz.schema.json`, as in `tests/readyz.rs`.
//!
//! The backoff tests (8–14) run with tokio's clock paused and drive the
//! runner by hand with [`Readiness::run_reporting_once`] instead of
//! [`Readiness::spawn`], so every sleep moves virtual time by exactly the
//! duration asked for: a cooldown is passed, or not passed, by the same
//! margin on a loaded CI as on an idle laptop. Margins are still at least
//! 2x wherever the window allows one (see 9 for the one that cannot).

use std::future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::http::StatusCode;
use jsonschema::Validator;
use maxgame_readiness::{
    should_trigger, AppEnv, CheckStatus, FnProbe, Probe, ProbeOutcome, Readiness,
};
use serde_json::Value;

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

/// Like `counted`, but the outcome depends on how often the probe has run,
/// so a test can watch a hop's cached state change.
fn counted_nth(
    counter: &Arc<AtomicUsize>,
    outcome: fn(usize) -> ProbeOutcome,
    dedupe_key: Option<&str>,
) -> impl Probe {
    let counter = Arc::clone(counter);
    let mut probe = FnProbe::new(move || {
        let counter = Arc::clone(&counter);
        async move {
            let nth = counter.fetch_add(1, Ordering::SeqCst);
            outcome(nth)
        }
    });
    if let Some(key) = dedupe_key {
        probe = probe.dedupe_key(key);
    }
    probe
}

/// A concrete `FnProbe` type, so `optional(name, None::<UnitOkProbe>)` can
/// name what "no probe" is.
type UnitOkProbe = FnProbe<fn() -> future::Ready<ProbeOutcome>>;

/// Poll `counter` until it reaches `target`, and give up after `budget`.
async fn wait_for_runs(counter: &AtomicUsize, target: usize, budget: Duration, what: &str) {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let seen = counter.load(Ordering::SeqCst);
        if seen >= target {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{what}: expected {target} probe runs within {budget:?}, saw {seen}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A probe that counts its runs and answers `unreachable` while `down` is
/// set, `ok` otherwise, so a test can make a triggered check confirm the
/// failure the request path saw (§6.4).
fn switchable(runs: &Arc<AtomicUsize>, down: Arc<AtomicBool>) -> impl Probe {
    let runs = Arc::clone(runs);
    FnProbe::new(move || {
        let runs = Arc::clone(&runs);
        let down = Arc::clone(&down);
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            if down.load(Ordering::SeqCst) {
                ProbeOutcome::fail(CheckStatus::Unreachable, "connect")
            } else {
                ProbeOutcome::ok()
            }
        }
    })
}

/// Base trigger cooldown of the backoff tests below (§6.4).
const BASE: Duration = Duration::from_millis(10);
/// Effective interval of the backoff tests below. 200 ms is also the cap the
/// doubling runs into: 10 → 20 → 40 → 80 → 160 → 200.
const INTERVAL: Duration = Duration::from_millis(200);

/// A one-reporting-hop reporter with the backoff-test numbers. Production
/// code would use the clamped `interval` and `trigger_cooldown`.
fn single_hop_with(probe: impl Probe) -> Readiness {
    Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", probe)
        .interval_unclamped(INTERVAL)
        .trigger_cooldown_unclamped(BASE)
        .build()
}

/// [`single_hop_with`] with a counting probe of a fixed outcome.
fn single_hop(runs: &Arc<AtomicUsize>, outcome: fn() -> ProbeOutcome) -> Readiness {
    single_hop_with(counted(runs, outcome, None))
}

/// The hop's current §6.4 cooldown, which only tests look at.
fn cooldown(readiness: &Readiness) -> Duration {
    readiness
        .current_trigger_cooldown("key_server")
        .expect("key_server is a reporting hop")
}

/// Sleep for virtual time, so the §6.4 clock moves by exactly `for_how_long`.
async fn advance(for_how_long: Duration) {
    tokio::time::sleep(for_how_long).await;
}

// 1. A signal makes the hop due long before its next scheduled run
//    (CONTRACT.md §6.4). Without spawn(), the next run_reporting_once picks
//    it up.
#[tokio::test]
async fn a_failed_request_re_checks_the_hop_before_the_interval() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .interval(Duration::from_secs(30))
        .trigger_cooldown_unclamped(Duration::from_millis(10))
        .build();

    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // The next scheduled run is a jittered 30 s away, so nothing is due.
    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the hop is not due yet");

    tokio::time::sleep(Duration::from_millis(20)).await; // let the cooldown elapse
    readiness.hop_failed("key_server");

    readiness.run_reporting_once().await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the signal made the hop due ahead of its interval"
    );

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["checks"]["key_server"]["status"], "ok");
    assert!(
        body["checks"]["key_server"]["checkedAt"].is_string(),
        "the triggered run updated checkedAt like a scheduled one"
    );
    assert_schema(&body);
}

// 2. The cooldown bounds a failing hop to one probe run however many
//    requests fail (CONTRACT.md §6.4): 100 signals inside a 10 s cooldown
//    cause one probe run in total, not 100.
#[tokio::test]
async fn signals_inside_the_cooldown_are_ignored() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .interval(Duration::from_secs(30))
        .trigger_cooldown(Duration::from_secs(10))
        .build();

    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    for _ in 0..100 {
        readiness.hop_failed("key_server");
    }
    readiness.run_reporting_once().await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "100 signals inside the cooldown must cause one probe run, not 100"
    );

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["checks"]["key_server"]["status"], "ok");
    assert_schema(&body);
}

// 3. A signal checks every hop sharing the hop's dedupe key, with one probe
//    run for the whole group (CONTRACT.md §6.3 rule 3, §6.4).
#[tokio::test]
async fn a_signal_checks_every_hop_sharing_the_dedupe_key() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report(
            "key_server",
            counted_nth(&runs, key_server_outcome, Some("key-server:8092")),
        )
        .report(
            "key_server_internal",
            counted_nth(&runs, key_server_outcome, Some("key-server:8092")),
        )
        .interval(Duration::from_secs(30))
        .trigger_cooldown_unclamped(Duration::from_millis(10))
        .build();

    readiness.run_reporting_once().await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "two hops with one dedupe key share one probe run"
    );
    let (code, first) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(first["status"], "ready");
    assert_eq!(first["checks"]["key_server"]["status"], "ok");
    assert_eq!(first["checks"]["key_server_internal"]["status"], "ok");
    assert_schema(&first);

    tokio::time::sleep(Duration::from_millis(20)).await;
    readiness.hop_failed("key_server"); // one of the two hops only
    readiness.run_reporting_once().await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the triggered run is still one probe run for the whole group"
    );
    let (code, second) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(second["status"], "degraded");
    for hop in ["key_server", "key_server_internal"] {
        assert_eq!(
            second["checks"][hop]["status"], "unexpected",
            "{hop} got the triggered run's new outcome"
        );
        assert_eq!(second["checks"][hop]["reason"], "bad_body");
        assert_eq!(second["checks"][hop]["httpStatus"], 404);
        assert!(second["checks"][hop]["checkedAt"].is_string());
    }
    assert_schema(&second);
}

/// ok on the first run, an unexpected 404 afterwards.
fn key_server_outcome(nth: usize) -> ProbeOutcome {
    if nth == 0 {
        ProbeOutcome::ok()
    } else {
        ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body").with_http_status(404)
    }
}

// 4. With spawn(), a signal wakes the background runner instead of waiting
//    out the 30 s interval (CONTRACT.md §6.4).
#[tokio::test]
async fn a_signal_wakes_the_background_runner() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .interval(Duration::from_secs(30))
        .trigger_cooldown_unclamped(Duration::from_millis(10))
        .build();

    let runner = readiness.spawn();
    wait_for_runs(&runs, 1, Duration::from_secs(1), "the first cycle").await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    tokio::time::sleep(Duration::from_millis(20)).await; // let the cooldown elapse
    readiness.hop_failed("key_server");

    // The next scheduled run is a jittered 24–36 s away, so reaching 2 this
    // fast can only be the wakeup.
    wait_for_runs(&runs, 2, Duration::from_millis(500), "the triggered cycle").await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the wakeup ran the hop once, it did not spin"
    );

    readiness.set_draining();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("the runner finishes soon after draining")
        .expect("the runner task did not panic");
}

// 5. An unknown hop name, a gating hop and a not_configured hop are ignored:
//    no panic, no probe, nothing marked due (CONTRACT.md §6.4).
#[tokio::test]
async fn unknown_gating_and_not_configured_signals_are_ignored() {
    let gate_runs = Arc::new(AtomicUsize::new(0));
    let report_runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .gate("postgres", counted(&gate_runs, ProbeOutcome::ok, None))
        .report("key_server", counted(&report_runs, ProbeOutcome::ok, None))
        .optional("r2", None::<UnitOkProbe>)
        .interval(Duration::from_secs(30))
        .trigger_cooldown_unclamped(Duration::from_millis(10))
        .build();

    readiness.run_reporting_once().await;
    let (code, _) = readiness.body().await; // runs the gate once
    assert_eq!(code, StatusCode::OK);
    let gate_before = gate_runs.load(Ordering::SeqCst);
    let report_before = report_runs.load(Ordering::SeqCst);
    assert!(report_before >= 1);

    for hop in ["no_such_hop", "postgres", "r2"] {
        readiness.hop_failed(hop);
    }
    readiness.run_reporting_once().await;

    assert_eq!(
        report_runs.load(Ordering::SeqCst),
        report_before,
        "no ignored signal made anything due"
    );
    assert_eq!(
        gate_runs.load(Ordering::SeqCst),
        gate_before,
        "a gating hop is never triggered"
    );

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["checks"]["key_server"]["status"], "ok");
    assert_eq!(body["checks"]["r2"]["status"], "not_configured");
    assert_schema(&body);
}

// 6. Draining turns a signal into a no-op (CONTRACT.md §6.4).
#[tokio::test]
async fn a_signal_while_draining_is_a_no_op() {
    let runs = Arc::new(AtomicUsize::new(0));

    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .interval(Duration::from_secs(30))
        .trigger_cooldown_unclamped(Duration::from_millis(10))
        .build();

    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    tokio::time::sleep(Duration::from_millis(20)).await;
    readiness.set_draining();
    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "nothing runs while draining"
    );

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "draining");
    assert_schema(&body);
}

// 7. The §6.4 classification of a failed request: no answer at all and any
//    5xx trigger; a 4xx is an answer about that request and never does.
#[test]
fn should_trigger_classifies_request_failures() {
    assert!(
        should_trigger(None),
        "no answer: transport error or timeout"
    );
    for status in [500, 503, 599] {
        assert!(should_trigger(Some(status)), "{status} is upstream_5xx");
    }
    for status in [200, 400, 401, 404, 499] {
        assert!(
            !should_trigger(Some(status)),
            "{status} is an answer about the request, not about the hop"
        );
    }
}

// 8. Only a hop that still looks healthy is re-checked (§6.4): with a
//    failure already cached the outage is known, a second probe adds nothing,
//    and recovery is picked up by the scheduled run within one interval.
#[tokio::test(start_paused = true)]
async fn a_hop_that_is_already_failing_ignores_signals() {
    let runs = Arc::new(AtomicUsize::new(0));
    let unreachable = || ProbeOutcome::fail(CheckStatus::Unreachable, "connect");

    let readiness = single_hop(&runs, unreachable);

    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["checks"]["key_server"]["status"], "unreachable");
    assert_eq!(body["checks"]["key_server"]["reason"], "connect");
    assert_schema(&body);

    // Well past the cooldown, and long before the next scheduled run (a
    // jittered 160-240 ms away), so the signal is the only thing that could
    // have made the hop due.
    advance(2 * BASE).await;
    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "a signal for a hop that is already failing runs nothing"
    );
    assert_eq!(
        cooldown(&readiness),
        BASE,
        "an ignored signal cannot back the cooldown off"
    );

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["checks"]["key_server"]["status"], "unreachable");
    assert_schema(&body);
}

// 9. A triggered check that comes back ok doubles the cooldown, capped at the
//    hop's effective interval (§6.4): requests fail but the probe cannot see
//    why, so the runner backs off towards its steady rate.
#[tokio::test(start_paused = true)]
async fn a_probe_that_stays_ok_doubles_the_cooldown_up_to_the_interval() {
    let runs = Arc::new(AtomicUsize::new(0));
    let readiness = single_hop(&runs, ProbeOutcome::ok);

    // The first run is scheduled, not triggered, so it leaves the cooldown
    // at its base value.
    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(cooldown(&readiness), BASE);

    for step in 0..5 {
        let before = cooldown(&readiness);
        // Sleep 2x the current cooldown, so the signal is past it with
        // margin. The last round cannot have a 2x margin in both directions:
        // the cooldown of 160 ms has to be passed while staying under the
        // 200 ms interval, one quiet interval of which would reset the
        // cooldown to its base, so 180 ms splits that 40 ms window instead.
        // Paused time moves by exactly what is asked for, so 20 ms of margin
        // each way is enough.
        let wait = if before.saturating_mul(2) <= INTERVAL {
            before * 2
        } else {
            INTERVAL - Duration::from_millis(20)
        };
        advance(wait).await;
        readiness.hop_failed("key_server");
        readiness.run_reporting_once().await;

        assert_eq!(
            runs.load(Ordering::SeqCst),
            2 + step,
            "the signal re-checked the hop"
        );
        assert_eq!(
            cooldown(&readiness),
            (before * 2).min(INTERVAL),
            "the cooldown after triggered run {}",
            step + 1
        );
    }

    assert_eq!(
        cooldown(&readiness),
        INTERVAL,
        "the backoff is capped at the hop's effective interval"
    );
}

// 10. A signal inside the doubled cooldown is ignored: one probe per
//     cooldown however many requests fail (§6.4).
#[tokio::test(start_paused = true)]
async fn a_signal_inside_the_doubled_cooldown_is_ignored() {
    let runs = Arc::new(AtomicUsize::new(0));
    let readiness = single_hop(&runs, ProbeOutcome::ok);

    readiness.run_reporting_once().await;
    // Two triggered runs double the cooldown from 10 ms to 40 ms.
    for _ in 0..2 {
        advance(cooldown(&readiness) * 2).await;
        readiness.hop_failed("key_server");
        readiness.run_reporting_once().await;
    }
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    assert_eq!(cooldown(&readiness), Duration::from_millis(40));

    // 15 ms into a 40 ms cooldown: ignored. The next scheduled run is a
    // jittered 160-240 ms away, so nothing else could have run either.
    advance(Duration::from_millis(15)).await;
    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        3,
        "a signal inside the doubled cooldown runs nothing"
    );
    assert_eq!(
        cooldown(&readiness),
        Duration::from_millis(40),
        "an ignored signal leaves the backoff where it was"
    );

    // Past the cooldown the next signal is honoured again, and the backoff
    // carries on from 40 ms rather than starting over.
    advance(2 * Duration::from_millis(40)).await;
    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 4);
    assert_eq!(cooldown(&readiness), Duration::from_millis(80));
}

// 11. One effective interval without a signal resets the cooldown to its base
//     before the next signal is evaluated (§6.4), so the backoff starts over
//     after a quiet spell.
#[tokio::test(start_paused = true)]
async fn one_quiet_interval_resets_the_cooldown_to_its_base() {
    let runs = Arc::new(AtomicUsize::new(0));
    let readiness = single_hop(&runs, ProbeOutcome::ok);

    readiness.run_reporting_once().await;
    // Two triggered runs double the cooldown from 10 ms to 40 ms.
    for _ in 0..2 {
        advance(cooldown(&readiness) * 2).await;
        readiness.hop_failed("key_server");
        readiness.run_reporting_once().await;
    }
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    assert_eq!(cooldown(&readiness), Duration::from_millis(40));

    // One interval with no signal at all. The signal that follows is past
    // 40 ms anyway, so what this checks is the reset: without it the run
    // below would double the cooldown to 80 ms instead of 20 ms.
    advance(2 * INTERVAL).await;
    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        4,
        "the quiet reset let the signal through"
    );
    assert_eq!(
        cooldown(&readiness),
        2 * BASE,
        "the backoff restarted from the base value"
    );
}

// 12. A triggered check that confirms the failure resets the cooldown to its
//     base value (§6.4): the failure is now cached, so the healthy-looking
//     rule stops further signals anyway.
#[tokio::test(start_paused = true)]
async fn a_triggered_check_that_confirms_the_failure_resets_the_cooldown() {
    let runs = Arc::new(AtomicUsize::new(0));
    let down = Arc::new(AtomicBool::new(false));
    let readiness = single_hop_with(switchable(&runs, Arc::clone(&down)));

    readiness.run_reporting_once().await;
    advance(2 * BASE).await;
    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert_eq!(
        cooldown(&readiness),
        2 * BASE,
        "the probe stayed ok, so the cooldown doubled"
    );

    // The next request failure is real: the probe sees it too. The wait is
    // past the doubled cooldown of 20 ms but far short of the 200 ms
    // interval, so it is the failure that resets the cooldown, not a quiet
    // interval.
    down.store(true, Ordering::SeqCst);
    advance(2 * 2 * BASE).await;
    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    assert_eq!(
        cooldown(&readiness),
        BASE,
        "a triggered check that fails resets the cooldown"
    );

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["checks"]["key_server"]["status"], "unreachable");
    assert_eq!(body["checks"]["key_server"]["reason"], "connect");
    assert_schema(&body);

    // And with the failure cached, the next signal is ignored outright.
    advance(2 * BASE).await;
    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 3);
}

// 13. A stale cached result still counts as healthy-looking (§6.4): the
//     checker may simply be stuck, so a signal is worth an early probe.
#[tokio::test(start_paused = true)]
async fn a_stale_hop_still_accepts_a_signal() {
    let runs = Arc::new(AtomicUsize::new(0));
    // A 50 ms interval, so the three intervals of the staleness rule
    // (§6.1) pass without slowing the test down.
    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&runs, ProbeOutcome::ok, None))
        .interval_unclamped(Duration::from_millis(50))
        .trigger_cooldown_unclamped(BASE)
        .build();

    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["checks"]["key_server"]["status"], "ok");
    assert_schema(&body);

    // Three intervals without a run make the cached ok stale. Nothing runs
    // by itself: only run_reporting_once drives the runner here.
    advance(6 * Duration::from_millis(50)).await;
    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "degraded", "stale degrades (§3.3)");
    assert_eq!(body["checks"]["key_server"]["status"], "stale");
    assert_schema(&body);

    readiness.hop_failed("key_server");
    readiness.run_reporting_once().await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "a stale hop is still healthy-looking enough to re-check"
    );
    assert_eq!(
        cooldown(&readiness),
        2 * BASE,
        "the run was a triggered one, so the cooldown doubled"
    );

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["checks"]["key_server"]["status"], "ok");
    assert!(body["checks"]["key_server"]["checkedAt"].is_string());
    assert_schema(&body);
}

// 14. The backoff follows the dedupe group (§6.4): a signal for one member
//     marks them all due, and the run that follows backs off every member
//     that ran.
#[tokio::test(start_paused = true)]
async fn the_backoff_covers_every_hop_of_the_dedupe_group() {
    let runs = Arc::new(AtomicUsize::new(0));
    let readiness = Readiness::builder("launcher", AppEnv::Development)
        .report(
            "key_server",
            counted(&runs, ProbeOutcome::ok, Some("key-server:8092")),
        )
        .report(
            "key_server_internal",
            counted(&runs, ProbeOutcome::ok, Some("key-server:8092")),
        )
        .interval_unclamped(INTERVAL)
        .trigger_cooldown_unclamped(BASE)
        .build();

    readiness.run_reporting_once().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1, "one probe run per group");
    for hop in ["key_server", "key_server_internal"] {
        assert_eq!(
            readiness.current_trigger_cooldown(hop),
            Some(BASE),
            "{hop} starts at the base cooldown"
        );
    }

    advance(2 * BASE).await;
    readiness.hop_failed("key_server_internal"); // the other member
    readiness.run_reporting_once().await;

    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the triggered run is still one probe run for the whole group"
    );
    for hop in ["key_server", "key_server_internal"] {
        assert_eq!(
            readiness.current_trigger_cooldown(hop),
            Some(2 * BASE),
            "{hop} ran, so {hop}'s cooldown doubled"
        );
    }

    let (code, body) = readiness.body().await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["checks"]["key_server"]["status"], "ok");
    assert_eq!(body["checks"]["key_server_internal"]["status"], "ok");
    assert_schema(&body);
}
