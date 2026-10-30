//! On-demand reporting: with `ReadinessBuilder::on_demand`, the background
//! runner probes nothing until a caller that carries `x-readiness-probe` (the
//! aggregator) asks for `/readyz`, keeps probing while such requests keep
//! coming, and goes idle again once they stop. A kubelet probe, which sends
//! no such header, never wakes it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue};
use maxgame_readiness::{AppEnv, FnProbe, ProbeOutcome, Readiness, PROBE_HEADER};

const INTERVAL: Duration = Duration::from_millis(150);
const IDLE: Duration = Duration::from_millis(500);

fn counted(counter: &Arc<AtomicUsize>) -> impl maxgame_readiness::Probe {
    let counter = Arc::clone(counter);
    FnProbe::new(move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            ProbeOutcome::ok()
        }
    })
}

fn aggregator() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        PROBE_HEADER,
        HeaderValue::from_static("readinessAggregator"),
    );
    h
}

fn build(counter: &Arc<AtomicUsize>) -> Readiness {
    Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(counter))
        .interval_unclamped(INTERVAL)
        .on_demand(IDLE)
        .build()
}

#[tokio::test]
async fn nothing_is_probed_until_the_aggregator_asks() {
    let n = Arc::new(AtomicUsize::new(0));
    let r = build(&n);
    let _runner = r.spawn();
    tokio::time::sleep(INTERVAL * 4).await;
    assert_eq!(n.load(Ordering::SeqCst), 0, "an idle runner probed");

    // Before the first run the hop is pending, never stale, and the
    // service is not degraded by it.
    let (_, body) = r.body_for(&aggregator()).await;
    assert_eq!(body["checks"]["key_server"]["status"], "pending", "{body}");
    assert_eq!(body["status"], "ready", "{body}");

    tokio::time::sleep(INTERVAL).await;
    assert!(
        n.load(Ordering::SeqCst) >= 1,
        "the aggregator's request did not wake the runner"
    );
    let (_, body) = r.body_for(&aggregator()).await;
    assert_eq!(body["checks"]["key_server"]["status"], "ok", "{body}");
}

#[tokio::test]
async fn a_request_without_the_probe_header_never_wakes_the_runner() {
    let n = Arc::new(AtomicUsize::new(0));
    let r = build(&n);
    let _runner = r.spawn();
    for _ in 0..4 {
        // kubelet: no x-readiness-probe header.
        let _ = r.body_for(&HeaderMap::new()).await;
        tokio::time::sleep(INTERVAL).await;
    }
    assert_eq!(
        n.load(Ordering::SeqCst),
        0,
        "a kubelet probe woke the runner"
    );
}

#[tokio::test]
async fn the_runner_keeps_probing_while_asked_and_stops_after_the_idle_window() {
    let n = Arc::new(AtomicUsize::new(0));
    let r = build(&n);
    let _runner = r.spawn();
    for _ in 0..6 {
        let _ = r.body_for(&aggregator()).await;
        tokio::time::sleep(INTERVAL).await;
    }
    let while_asked = n.load(Ordering::SeqCst);
    assert!(
        while_asked >= 3,
        "probed only {while_asked} times while being asked"
    );

    tokio::time::sleep(IDLE + INTERVAL * 2).await;
    let at_idle = n.load(Ordering::SeqCst);
    tokio::time::sleep(INTERVAL * 5).await;
    assert_eq!(
        n.load(Ordering::SeqCst),
        at_idle,
        "the runner kept probing after the idle window"
    );
}

#[tokio::test]
async fn a_result_from_before_the_runner_went_idle_reads_pending_on_wake_not_stale() {
    let n = Arc::new(AtomicUsize::new(0));
    let r = build(&n);
    let _runner = r.spawn();
    let _ = r.body_for(&aggregator()).await;
    tokio::time::sleep(INTERVAL).await;
    assert!(n.load(Ordering::SeqCst) >= 1);

    // Idle long enough that the old result is past three intervals.
    tokio::time::sleep(IDLE + INTERVAL * 4).await;
    let (_, body) = r.body_for(&aggregator()).await;
    assert_eq!(body["checks"]["key_server"]["status"], "pending", "{body}");
    assert_eq!(
        body["status"], "ready",
        "a waking runner degraded the service: {body}"
    );
}

#[tokio::test]
async fn without_on_demand_the_runner_probes_on_its_own_as_before() {
    let n = Arc::new(AtomicUsize::new(0));
    let r = Readiness::builder("launcher", AppEnv::Development)
        .report("key_server", counted(&n))
        .interval_unclamped(INTERVAL)
        .build();
    let _runner = r.spawn();
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(
        n.load(Ordering::SeqCst) >= 2,
        "the default runner stopped probing"
    );
}
