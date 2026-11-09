//! A hop whose last result was not `ok` is re-run sooner than the interval:
//! after the base failure retry, doubling per consecutive failure, capped at
//! the interval, and back on the interval once it is `ok` (`CONTRACT.md`
//! §6.1). A hop with its own minimum interval (§6.3 rule 4) keeps it.
//!
//! The clock is paused and the runner is driven by hand, as in the backoff
//! tests of `tests/trigger.rs`. Jitter is ±20%, so every "not yet" waits
//! under 0.8× the expected delay and every "now" waits over 1.2×.

use std::future::{ready, Ready};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use maxgame_readiness::{AppEnv, CheckStatus, FnProbe, Probe, ProbeOutcome, Readiness};

const RETRY: Duration = Duration::from_millis(10);
const INTERVAL: Duration = Duration::from_millis(200);

/// A probe whose `nth` run (0-based) answers `outcome(nth)`.
fn nth_probe(
    runs: &Arc<AtomicUsize>,
    outcome: fn(usize) -> ProbeOutcome,
) -> FnProbe<impl Fn() -> Ready<ProbeOutcome> + Send + Sync + 'static> {
    let runs = Arc::clone(runs);
    FnProbe::new(move || ready(outcome(runs.fetch_add(1, Ordering::SeqCst))))
}

fn down(_: usize) -> ProbeOutcome {
    ProbeOutcome::fail(CheckStatus::Unreachable, "connect")
}

fn reporter(probe: impl Probe + 'static) -> Readiness {
    Readiness::builder("utility", AppEnv::Development)
        .report("admin_introspect", probe)
        .interval_unclamped(INTERVAL)
        .failure_retry_unclamped(RETRY)
        .build()
}

/// Move virtual time by `ms`, then give the runner one chance to run.
async fn tick(readiness: &Readiness, ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    readiness.run_reporting_once().await;
}

fn runs_of(runs: &AtomicUsize) -> usize {
    runs.load(Ordering::SeqCst)
}

#[tokio::test(start_paused = true)]
async fn a_failure_is_retried_long_before_the_interval_and_backs_off() {
    let runs = Arc::new(AtomicUsize::new(0));
    let readiness = reporter(nth_probe(&runs, down));

    readiness.run_reporting_once().await;
    assert_eq!(runs_of(&runs), 1);

    tick(&readiness, 13).await; // > 1.2 × 10 ms, far under 0.8 × 200 ms
    assert_eq!(runs_of(&runs), 2, "first retry after the base delay");

    tick(&readiness, 13).await; // < 0.8 × 20 ms
    assert_eq!(runs_of(&runs), 2, "second retry waits the doubled delay");
    tick(&readiness, 12).await; // 25 ms in total > 1.2 × 20 ms
    assert_eq!(runs_of(&runs), 3);

    // 40, 80, 160, then the cap: 200 ms.
    for (wait, total) in [(49, 4), (97, 5), (193, 6), (241, 7)] {
        tick(&readiness, wait).await;
        assert_eq!(runs_of(&runs), total);
    }
    tick(&readiness, 159).await; // < 0.8 × 200 ms: the delay stays capped, not 400 ms
    assert_eq!(runs_of(&runs), 7);
    tick(&readiness, 82).await; // 241 ms in total > 1.2 × 200 ms
    assert_eq!(runs_of(&runs), 8, "capped at the interval");

    let (_, body) = readiness.body().await;
    assert_eq!(body["checks"]["admin_introspect"]["status"], "unreachable");
}

#[tokio::test(start_paused = true)]
async fn an_ok_returns_the_hop_to_the_interval_and_resets_the_retry() {
    // fail, fail, ok, fail, ...
    fn outcome(nth: usize) -> ProbeOutcome {
        if nth == 2 {
            ProbeOutcome::ok()
        } else {
            down(nth)
        }
    }
    let runs = Arc::new(AtomicUsize::new(0));
    let readiness = reporter(nth_probe(&runs, outcome));

    readiness.run_reporting_once().await; // fail, retry in 10 ms
    tick(&readiness, 13).await; // fail, retry in 20 ms
    tick(&readiness, 25).await; // ok
    assert_eq!(runs_of(&runs), 3);
    let (_, body) = readiness.body().await;
    assert_eq!(body["checks"]["admin_introspect"]["status"], "ok");

    tick(&readiness, 150).await; // < 0.8 × 200 ms
    assert_eq!(runs_of(&runs), 3, "an ok hop waits for the interval");
    tick(&readiness, 91).await; // 241 ms in total
    assert_eq!(runs_of(&runs), 4, "fails again");

    tick(&readiness, 13).await;
    assert_eq!(
        runs_of(&runs),
        5,
        "the retry is back at its base, not at the 40 ms it had reached"
    );
}

#[tokio::test(start_paused = true)]
async fn a_hop_with_its_own_minimum_interval_keeps_its_schedule() {
    let runs = Arc::new(AtomicUsize::new(0));
    let readiness = reporter(nth_probe(&runs, down).min_interval(INTERVAL));

    readiness.run_reporting_once().await;
    tick(&readiness, 150).await; // many base retries' worth, < 0.8 × 200 ms
    assert_eq!(
        runs_of(&runs),
        1,
        "a quota-guarded hop is not retried early (§6.3 rule 4)"
    );
    tick(&readiness, 91).await;
    assert_eq!(runs_of(&runs), 2);
}
