//! The `/readyz` reporter: hops, the background runner and the body
//! (`CONTRACT.md` §3, §6).

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use chrono::{SecondsFormat, Utc};
use futures_util::FutureExt;
use rand::Rng;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::env::AppEnv;
use crate::model::{CheckStatus, Identity, ProbeOutcome};
use crate::probe::{sanitize_target, Probe};

/// Default background interval, `READYZ_CHECK_INTERVAL_SECONDS`
/// (`CONTRACT.md` §6.1).
pub const DEFAULT_INTERVAL_SECONDS: u64 = 30;
/// Smallest interval the knob allows (`CONTRACT.md` §6.1).
pub const MIN_INTERVAL_SECONDS: u64 = 10;
/// Largest interval the knob allows (`CONTRACT.md` §6.1).
pub const MAX_INTERVAL_SECONDS: u64 = 300;
/// Environment variable that overrides the interval (`CONTRACT.md` §6.3
/// rule 6).
pub const INTERVAL_ENV_VAR: &str = "READYZ_CHECK_INTERVAL_SECONDS";
/// Default trigger cooldown, `READYZ_TRIGGER_COOLDOWN_SECONDS`
/// (`CONTRACT.md` §6.4).
pub const DEFAULT_TRIGGER_COOLDOWN_SECONDS: u64 = 5;
/// Smallest trigger cooldown the knob allows (`CONTRACT.md` §6.4).
pub const MIN_TRIGGER_COOLDOWN_SECONDS: u64 = 1;
/// Largest trigger cooldown the knob allows (`CONTRACT.md` §6.4).
pub const MAX_TRIGGER_COOLDOWN_SECONDS: u64 = 60;
/// Environment variable that overrides the trigger cooldown (`CONTRACT.md`
/// §6.4).
pub const TRIGGER_COOLDOWN_ENV_VAR: &str = "READYZ_TRIGGER_COOLDOWN_SECONDS";

const DEFAULT_INTERVAL: Duration = Duration::from_secs(DEFAULT_INTERVAL_SECONDS);
const DEFAULT_TRIGGER_COOLDOWN: Duration = Duration::from_secs(DEFAULT_TRIGGER_COOLDOWN_SECONDS);
const MIN_TRIGGER_COOLDOWN: Duration = Duration::from_secs(MIN_TRIGGER_COOLDOWN_SECONDS);
const MAX_TRIGGER_COOLDOWN: Duration = Duration::from_secs(MAX_TRIGGER_COOLDOWN_SECONDS);
const MIN_INTERVAL: Duration = Duration::from_secs(MIN_INTERVAL_SECONDS);
const MAX_INTERVAL: Duration = Duration::from_secs(MAX_INTERVAL_SECONDS);
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Jitter spread applied to every scheduled run (`CONTRACT.md` §6.1).
const JITTER: f64 = 0.2;
/// Lower bound on the runner sleep so a rounding error cannot spin the loop.
const MIN_SLEEP: Duration = Duration::from_millis(1);

/// One registered hop.
struct Hop {
    name: String,
    gate: bool,
    required: bool,
    probe: Option<Arc<dyn Probe>>,
    /// Index into `Inner::reporting`, set for reporting hops that have a
    /// probe. `None` for gating hops and for a `not_configured` optional hop.
    reporting_index: Option<usize>,
    /// Last status seen by an inline run, for transition logging only.
    last_status: Mutex<Option<CheckStatus>>,
}

/// The cached result of one background run (`CONTRACT.md` §6.1).
#[derive(Clone)]
struct CachedResult {
    outcome: ProbeOutcome,
    latency_ms: u64,
    checked_at: String,
    /// Clock the staleness rule uses, so tests can move it.
    produced_at: Instant,
}

#[derive(Clone)]
struct ReportingState {
    cached: Option<CachedResult>,
    next_due: Instant,
    /// This hop's current §6.4 trigger cooldown: the builder's base value,
    /// or whatever the exponential backoff has raised it to, capped at the
    /// hop's effective interval.
    cooldown_current: Duration,
    /// When this hop last saw a signal reach the cooldown step of §6.4,
    /// accepted or ignored. One effective interval of silence resets
    /// `cooldown_current` to the base.
    last_signal_at: Option<Instant>,
    /// A signal was accepted and this hop's next run is therefore a
    /// triggered one, which is what the backoff keys off. Consumed by that
    /// run (`CONTRACT.md` §6.4).
    triggered: bool,
}

struct Inner {
    service: String,
    env: AppEnv,
    version: Option<String>,
    interval: Duration,
    probe_timeout: Duration,
    /// The base value of the §6.4 trigger cooldown: how soon after a result a
    /// [`Readiness::hop_failed`] signal is first honoured, and what the
    /// cooldown returns to after a quiet interval or a confirmed failure.
    trigger_cooldown: Duration,
    hops: Vec<Hop>,
    reporting: Mutex<Vec<ReportingState>>,
    /// Whether a check of this reporting hop is running right now, indexed
    /// like `reporting`. A signal that arrives while a flag is set is merged
    /// into that run (`CONTRACT.md` §6.4).
    in_flight: Vec<AtomicBool>,
    /// Wakes the background runner when the request path signals a failed
    /// hop, or when the process starts draining (`CONTRACT.md` §6.4).
    notify: Notify,
    /// When the background runner (or the process) started, for the
    /// `pending` → `stale` deadline (`CONTRACT.md` §6.1).
    started_at: Mutex<Instant>,
    draining: AtomicBool,
}

impl Inner {
    /// The interval this hop is actually checked on: the global interval,
    /// raised to the hop's own minimum (`CONTRACT.md` §6.3 rule 4).
    fn effective_interval(&self, hop: &Hop) -> Duration {
        let min = hop
            .probe
            .as_ref()
            .and_then(|p| p.min_interval())
            .unwrap_or_default();
        self.interval.max(min)
    }

    /// The hop that owns cache slot `idx`. Reporting slots and hops are in
    /// one-to-one correspondence, so this always finds one.
    fn hop_for(&self, idx: usize) -> &Hop {
        self.hops
            .iter()
            .find(|hop| hop.reporting_index == Some(idx))
            .expect("every reporting slot belongs to a hop")
    }

    /// The cache slots of `hop` and of every reporting hop sharing its
    /// dedupe key (`CONTRACT.md` §6.3 rule 3, §6.4). A keyless hop is alone.
    fn trigger_group(&self, index: usize) -> Vec<usize> {
        let key = self.hops[index].probe.as_ref().and_then(|p| p.dedupe_key());
        match key.as_deref() {
            Some(key) => self
                .hops
                .iter()
                .filter(|hop| !hop.gate)
                .filter(|hop| {
                    hop.probe.as_ref().and_then(|p| p.dedupe_key()).as_deref() == Some(key)
                })
                .filter_map(|hop| hop.reporting_index)
                .collect(),
            None => self.hops[index].reporting_index.into_iter().collect(),
        }
    }
}

/// A built readiness reporter. Cheap to clone; the clone shares the same
/// cache, runner state and draining flag.
#[derive(Clone)]
pub struct Readiness(Arc<Inner>);

/// Builder for [`Readiness`].
///
/// Gating checks (the service's own datastores) run inline on every
/// `/readyz` request; reporting checks (every upstream hop) run in the
/// background and are cached (`CONTRACT.md` §2). An upstream HTTP hop MUST
/// NOT be gating.
pub struct ReadinessBuilder {
    service: String,
    env: AppEnv,
    version: Option<String>,
    interval: Duration,
    probe_timeout: Duration,
    trigger_cooldown: Duration,
    hops: Vec<HopSpec>,
}

struct HopSpec {
    name: String,
    gate: bool,
    required: bool,
    probe: Option<Arc<dyn Probe>>,
}

impl Readiness {
    /// Start building a reporter for `service` (the service key from
    /// `PLATFORM.md` §5.1) in `env`.
    pub fn builder(service: impl Into<String>, env: AppEnv) -> ReadinessBuilder {
        ReadinessBuilder {
            service: service.into(),
            env,
            version: None,
            interval: DEFAULT_INTERVAL,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            trigger_cooldown: DEFAULT_TRIGGER_COOLDOWN,
            hops: Vec::new(),
        }
    }

    /// Start the background runner: one cycle straight away, then a cycle
    /// whenever a hop is due (`CONTRACT.md` §6.1). The first cycle does not
    /// delay the caller. Call once, after the listener is up.
    ///
    /// A [`Readiness::hop_failed`] signal wakes the runner ahead of the
    /// schedule (`CONTRACT.md` §6.4).
    pub fn spawn(&self) -> tokio::task::JoinHandle<()> {
        *self.0.started_at.lock().unwrap() = Instant::now();
        let inner = Arc::clone(&self.0);
        tokio::spawn(async move {
            loop {
                let wait = cycle(&inner).await;
                if inner.draining.load(Ordering::Relaxed) {
                    break;
                }
                // Sleep until the next hop is due, or until the request path
                // signals a failure (CONTRACT.md §6.4). `Notify` keeps a
                // permit for a signal that arrives between cycles, so a
                // wakeup is never lost.
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = inner.notify.notified() => {}
                }
            }
        })
    }

    /// Run one background cycle now, honouring each hop's own effective
    /// interval and the dedupe keys. Probes are not run while draining.
    ///
    /// Cycles are sequential: the runner started by [`Readiness::spawn`] is
    /// the only caller this crate supports, so a hop is never checked twice
    /// concurrently. Calling this while such a runner is alive is not
    /// supported.
    pub async fn run_reporting_once(&self) {
        cycle(&self.0).await;
    }

    /// Mark the process as draining (SIGTERM). From here on `/readyz`
    /// answers 503 `draining` and no probe runs (`CONTRACT.md` §3.3).
    pub fn set_draining(&self) {
        self.0.draining.store(true, Ordering::Release);
        // Wake the runner so it notices instead of finishing its sleep.
        self.0.notify.notify_one();
    }

    /// Whether [`Readiness::set_draining`] has been called.
    pub fn is_draining(&self) -> bool {
        self.0.draining.load(Ordering::Acquire)
    }

    /// Signal that a real request to `hop` failed with a 5xx, a timeout, or
    /// a connection, DNS or TLS error (`CONTRACT.md` §6.4), so the hop is
    /// checked right away instead of at its next scheduled run. Every
    /// reporting hop sharing its dedupe key is checked in the same run.
    ///
    /// Classify the failure with [`should_trigger`] first: a 4xx is an answer
    /// about that request, not about the hop, and MUST NOT trigger.
    ///
    /// Never blocks, never awaits, never panics: the signal is dropped
    /// rather than paid for on the request path. Ignored, at debug level,
    /// when the hop name is unknown, the hop is gating or `not_configured`,
    /// a check of the hop is already in flight, the process is draining, or
    /// the hop does not look healthy: **only a hop whose cached status is
    /// `ok`, `pending` or `stale` is re-checked**, because with a failure
    /// already cached the outage is known and a second probe adds nothing.
    ///
    /// A signal that passes that gate is still ignored while the hop's last
    /// result is younger than its current cooldown. That cooldown starts at
    /// [`ReadinessBuilder::trigger_cooldown`], doubles after every triggered
    /// check that comes back `ok` (capped at the hop's effective interval),
    /// and returns to the base value after a triggered check that fails or
    /// after one effective interval without a signal. This bounds a hop to
    /// one extra probe per cooldown however many requests fail, and backs
    /// off while the probe cannot see what the request path sees.
    ///
    /// With [`Readiness::spawn`] this wakes the background runner. Without
    /// it, the hop is only marked due; the next
    /// [`Readiness::run_reporting_once`] picks it up.
    pub fn hop_failed(&self, hop: &str) {
        let inner = &self.0;
        if inner.draining.load(Ordering::Acquire) {
            tracing::debug!(hop, "readiness trigger ignored: draining");
            return;
        }

        let index = match inner.hops.iter().position(|h| h.name == hop) {
            Some(index) => index,
            None => {
                tracing::debug!(hop, "readiness trigger ignored: unknown hop");
                return;
            }
        };

        if inner.hops[index].gate {
            tracing::debug!(hop, "readiness trigger ignored: gating hop runs inline");
            return;
        }
        let Some(idx) = inner.hops[index].reporting_index else {
            tracing::debug!(hop, "readiness trigger ignored: hop is not_configured");
            return;
        };

        // The group is computed before the lock, from state that is frozen
        // at build time (CONTRACT.md §6.3 rule 3).
        let group = inner.trigger_group(index);
        let now = Instant::now();
        {
            let mut states = inner
                .reporting
                // A signal never panics, so it never waits on a poisoned
                // lock either.
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if inner.in_flight[idx].load(Ordering::Acquire) {
                tracing::debug!(hop, "readiness trigger merged into an in-flight check");
                return;
            }

            // Only a hop that still looks healthy is re-checked
            // (CONTRACT.md §6.4). Staleness is judged with the same rule the
            // body uses: a result older than three effective intervals is
            // `stale`, not healthy, but still worth an early probe.
            let looks_healthy = match states[idx].cached.as_ref() {
                None => true, // pending: nothing has been checked yet
                Some(cached) => matches!(
                    cached_status(cached, inner.effective_interval(&inner.hops[index]), now),
                    CheckStatus::Ok | CheckStatus::Stale
                ),
            };
            if !looks_healthy {
                tracing::debug!(hop, "readiness trigger ignored: hop already failing");
                return;
            }

            // One effective interval without a signal puts the cooldown back
            // at its base value, before the cooldown is evaluated
            // (CONTRACT.md §6.4). A signal for one member of a dedupe group
            // is a signal for the group, so the bookkeeping covers every
            // member. Every signal that reaches this point is remembered,
            // honoured or not, so a quiet interval is measured from the last
            // signal rather than from the last run.
            for &member in &group {
                let effective = inner.effective_interval(inner.hop_for(member));
                let state = &mut states[member];
                if state
                    .last_signal_at
                    .is_none_or(|at| now.saturating_duration_since(at) >= effective)
                {
                    state.cooldown_current = inner.trigger_cooldown;
                }
                state.last_signal_at = Some(now);
            }

            let cooldown = states[idx].cooldown_current;
            if states[idx]
                .cached
                .as_ref()
                .is_some_and(|cached| now.saturating_duration_since(cached.produced_at) < cooldown)
            {
                tracing::debug!(hop, "readiness trigger ignored: inside the cooldown");
                return;
            }
            // A hop with no cached result yet is already due; mark the whole
            // group due anyway so the next cycle cannot skip it, and remember
            // that the run this signal asked for is a triggered one.
            for member in group {
                states[member].next_due = now;
                states[member].triggered = true;
            }
        }
        inner.notify.notify_one();
    }

    /// The hop's current §6.4 trigger cooldown: the base value, or whatever
    /// the exponential backoff has raised it to, capped at the hop's
    /// effective interval. `None` when `hop` is not a reporting hop.
    ///
    /// Hidden: tests watch the backoff move. Production code has no use for
    /// it, because the cooldown is applied inside [`Readiness::hop_failed`].
    #[doc(hidden)]
    pub fn current_trigger_cooldown(&self, hop: &str) -> Option<Duration> {
        let inner = &self.0;
        let index = inner.hops.iter().position(|h| h.name == hop)?;
        let idx = inner.hops[index].reporting_index?;
        let states = inner.reporting.lock().unwrap();
        Some(states[idx].cooldown_current)
    }

    /// Build the `/readyz` body and its HTTP status (`CONTRACT.md` §3).
    ///
    /// Gating checks run inline here, concurrently, each bounded by the
    /// probe timeout. Reporting checks are read from the cache and never
    /// probed.
    pub async fn body(&self) -> (StatusCode, Value) {
        let inner = &self.0;
        if inner.draining.load(Ordering::Acquire) {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({ "status": "draining" }),
            );
        }

        let gate_indices: Vec<usize> = inner
            .hops
            .iter()
            .enumerate()
            .filter(|(_, hop)| hop.gate)
            .map(|(i, _)| i)
            .collect();

        let gate_results = if gate_indices.is_empty() {
            Vec::new()
        } else {
            let futs = gate_indices.iter().map(|&i| {
                let hop = &inner.hops[i];
                let probe = Arc::clone(hop.probe.as_ref().expect("gating hop has a probe"));
                let timeout = inner.probe_timeout;
                async move {
                    let (outcome, latency) = run_bounded(probe, timeout).await;
                    (i, outcome, latency)
                }
            });
            futures_util::future::join_all(futs).await
        };

        let mut gate_outcomes: Vec<Option<(ProbeOutcome, Duration)>> = vec![None; inner.hops.len()];
        for (i, outcome, latency) in gate_results {
            record_gate_status(&inner.hops[i], outcome.status, outcome.effective_reason());
            gate_outcomes[i] = Some((outcome, latency));
        }

        let now = Instant::now();
        let started_at = *inner.started_at.lock().unwrap();
        let reporting = inner.reporting.lock().unwrap();

        let mut checks = Map::new();
        let mut statuses = vec![CheckStatus::Ok; inner.hops.len()];

        for (i, hop) in inner.hops.iter().enumerate() {
            let check = if let Some((outcome, latency)) = gate_outcomes[i].as_ref() {
                CheckBody::from_outcome(outcome, hop, Some(as_ms(*latency)), None)
            } else if let Some(idx) = hop.reporting_index {
                reporting_check(inner, hop, &reporting[idx], now, started_at)
            } else {
                // An optional hop that is switched off (CONTRACT.md §3.5).
                CheckBody::not_configured(hop)
            };
            statuses[i] = check.status;
            checks.insert(
                hop.name.clone(),
                serde_json::to_value(&check).expect("check body serializes"),
            );
        }
        drop(reporting);

        // First failing gating check in registration order names the
        // dependency (CONTRACT.md §3.3).
        let dependency = inner
            .hops
            .iter()
            .zip(statuses.iter())
            .find(|(hop, status)| hop.gate && **status != CheckStatus::Ok)
            .map(|(hop, _)| hop.name.clone());

        let (status, code) = match dependency {
            Some(_) => ("unavailable", StatusCode::SERVICE_UNAVAILABLE),
            None => {
                // A required reporting check that is neither ok nor pending
                // degrades; a not_configured optional hop never does
                // (CONTRACT.md §3.3).
                let degraded = inner.hops.iter().zip(statuses.iter()).any(|(hop, status)| {
                    hop.required
                        && !hop.gate
                        && hop.probe.is_some()
                        && !matches!(status, CheckStatus::Ok | CheckStatus::Pending)
                });
                if degraded {
                    ("degraded", StatusCode::OK)
                } else {
                    ("ready", StatusCode::OK)
                }
            }
        };

        let body = ReadyBody {
            status,
            dependency,
            service: &inner.service,
            env: inner.env,
            version: inner.version.as_deref(),
            checks,
        };
        (
            code,
            serde_json::to_value(&body).expect("readyz body serializes"),
        )
    }

    /// A router serving `GET /readyz` at the pod root. It is never nested
    /// under `BASE_PATH` (`PLATFORM.md` §3.1).
    pub fn router<S>(&self) -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        let readiness = self.clone();
        axum::Router::new().route(
            "/readyz",
            axum::routing::get(move || {
                let readiness = readiness.clone();
                async move {
                    let (status, body) = readiness.body().await;
                    (
                        status,
                        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
                        axum::Json(body),
                    )
                }
            }),
        )
    }
}

/// One entry of the `checks` map (`CONTRACT.md` §3.4).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckBody {
    status: CheckStatus,
    gate: bool,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected: Option<Identity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed: Option<Identity>,
}

impl CheckBody {
    fn from_outcome(
        outcome: &ProbeOutcome,
        hop: &Hop,
        latency_ms: Option<u64>,
        checked_at: Option<String>,
    ) -> Self {
        Self {
            status: outcome.status,
            gate: hop.gate,
            required: hop.required,
            latency_ms,
            checked_at,
            http_status: outcome.http_status,
            reason: if outcome.status.is_failure() {
                Some(outcome.effective_reason().expect("failure has a reason"))
            } else {
                outcome.reason.clone()
            },
            target: hop
                .probe
                .as_ref()
                .and_then(|p| p.target())
                .and_then(|t| sanitize_target(&t)),
            expected: outcome.expected.clone(),
            observed: outcome.observed.clone(),
        }
    }

    fn not_configured(hop: &Hop) -> Self {
        Self {
            status: CheckStatus::NotConfigured,
            gate: hop.gate,
            required: hop.required,
            latency_ms: None,
            checked_at: None,
            http_status: None,
            reason: None,
            target: None,
            expected: None,
            observed: None,
        }
    }
}

/// Build the body entry for a cached reporting hop, applying the staleness
/// rule of `CONTRACT.md` §6.1.
fn reporting_check(
    inner: &Inner,
    hop: &Hop,
    state: &ReportingState,
    now: Instant,
    started_at: Instant,
) -> CheckBody {
    let effective = inner.effective_interval(hop);
    match &state.cached {
        None => {
            // Still waiting for the first run: pending, unless that wait has
            // outlasted two intervals, which makes it stale.
            let too_long = now.saturating_duration_since(started_at) >= effective * 2;
            let mut check = CheckBody::not_configured(hop);
            check.status = if too_long {
                CheckStatus::Stale
            } else {
                CheckStatus::Pending
            };
            check
        }
        Some(cached) => {
            let mut check = CheckBody::from_outcome(
                &cached.outcome,
                hop,
                Some(cached.latency_ms),
                Some(cached.checked_at.clone()),
            );
            if cached_status(cached, effective, now) == CheckStatus::Stale {
                check.status = CheckStatus::Stale;
            }
            check
        }
    }
}

/// The status a cached result reports at `now`, applying the staleness rule
/// of `CONTRACT.md` §6.1: a result older than three effective intervals is
/// `stale`.
///
/// The body and the §6.4 trigger both go through here, so the two cannot
/// disagree about what a healthy-looking hop is.
fn cached_status(cached: &CachedResult, effective: Duration, now: Instant) -> CheckStatus {
    if now.saturating_duration_since(cached.produced_at) >= effective * 3 {
        CheckStatus::Stale
    } else {
        cached.outcome.status
    }
}

#[derive(Serialize)]
struct ReadyBody<'a> {
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dependency: Option<String>,
    service: &'a str,
    env: AppEnv,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    checks: Map<String, Value>,
}

/// Run one probe, bounded by the timeout and by panic safety, and time it.
///
/// A timeout is `unreachable` with reason `timeout` (`CONTRACT.md` §3.5);
/// a panicking probe is `unexpected` with reason
/// `probe_panicked` and must not take the runner or the request down.
async fn run_bounded(probe: Arc<dyn Probe>, timeout: Duration) -> (ProbeOutcome, Duration) {
    let started = Instant::now();
    let fut = AssertUnwindSafe(probe.run()).catch_unwind();
    let outcome = match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_panic)) => ProbeOutcome::fail(CheckStatus::Unexpected, "probe_panicked"),
        Err(_elapsed) => ProbeOutcome::fail(CheckStatus::Unreachable, "timeout"),
    };
    (outcome, started.elapsed())
}

fn as_ms(latency: Duration) -> u64 {
    u64::try_from(latency.as_millis()).unwrap_or(u64::MAX)
}

/// One background cycle: run every due reporting hop, deduplicating hops
/// that share a probe (`CONTRACT.md` §6.1, §6.3 rule 3). Returns how long
/// the runner should sleep.
async fn cycle(inner: &Arc<Inner>) -> Duration {
    if inner.draining.load(Ordering::Acquire) {
        return inner.interval;
    }

    let now = Instant::now();
    let due: Vec<usize> = {
        let states = inner.reporting.lock().unwrap();
        inner
            .hops
            .iter()
            .enumerate()
            .filter_map(|(i, hop)| hop.reporting_index.map(|idx| (i, idx)))
            .filter(|(_, idx)| now >= states[*idx].next_due)
            .map(|(i, _)| i)
            .collect()
    };

    if !due.is_empty() {
        let groups = group_by_dedupe_key(inner, due);
        let futs = groups.into_iter().map(|members| {
            let inner = Arc::clone(inner);
            async move {
                run_group(&inner, members).await;
            }
        });
        futures_util::future::join_all(futs).await;
    }

    next_wake(inner, Instant::now())
}

/// Turn the due hop indices into dedupe groups. Hops without a dedupe key
/// always run on their own; hops sharing a key share one run
/// (`CONTRACT.md` §6.3 rule 3).
fn group_by_dedupe_key(inner: &Inner, due: Vec<usize>) -> Vec<Vec<usize>> {
    let mut groups: Vec<(Option<String>, Vec<usize>)> = Vec::new();
    for i in due {
        let key = inner.hops[i].probe.as_ref().and_then(|p| p.dedupe_key());
        // A hop without a key never joins a group, not even another keyless one.
        let existing = match key.as_deref() {
            Some(key) => groups.iter_mut().find(|(k, _)| k.as_deref() == Some(key)),
            None => None,
        };
        match existing {
            Some((_, members)) => members.push(i),
            None => groups.push((key, vec![i])),
        }
    }
    groups.into_iter().map(|(_, members)| members).collect()
}

/// Run the representative probe of a dedupe group and copy the outcome to
/// every member. The group's in-flight flags are set for the duration, so a
/// [`Readiness::hop_failed`] signal that arrives meanwhile is merged into
/// this run rather than queueing another (`CONTRACT.md` §6.4).
async fn run_group(inner: &Arc<Inner>, members: Vec<usize>) {
    let representative = members[0];
    let probe = Arc::clone(
        inner.hops[representative]
            .probe
            .as_ref()
            .expect("reporting hop has a probe"),
    );

    for &i in &members {
        set_in_flight(inner, i, true);
    }
    let (outcome, latency) = run_bounded(probe, inner.probe_timeout).await;
    let produced_at = Instant::now();
    let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let latency_ms = as_ms(latency);

    {
        let mut states = inner.reporting.lock().unwrap();
        for i in &members {
            let hop = &inner.hops[*i];
            let idx = hop.reporting_index.expect("reporting hop has a cache slot");
            let state = &mut states[idx];
            let previous = state.cached.as_ref().map(|c| c.outcome.status);
            log_transition(
                &hop.name,
                previous,
                outcome.status,
                outcome.effective_reason(),
            );
            state.cached = Some(CachedResult {
                outcome: outcome.clone(),
                latency_ms,
                checked_at: checked_at.clone(),
                produced_at,
            });
            state.next_due = produced_at + jittered(inner.effective_interval(hop));
            // The §6.4 backoff keys off a run a signal asked for. A probe
            // that stays ok while requests fail doubles the cooldown, capped
            // at the hop's effective interval; a probe that confirms the
            // failure puts it back at the base, where the cached failure
            // stops further signals anyway. A scheduled run leaves the
            // cooldown where it is.
            if state.triggered {
                state.triggered = false;
                state.cooldown_current = if outcome.status == CheckStatus::Ok {
                    state
                        .cooldown_current
                        .saturating_mul(2)
                        .min(inner.effective_interval(hop))
                } else {
                    inner.trigger_cooldown
                };
            }
        }
    }
    // Cleared only after the cache holds the fresh result, so a signal that
    // lands between the two is either merged or answered by the cooldown.
    for i in members {
        set_in_flight(inner, i, false);
    }
}

/// Raise or lower the in-flight flag of one reporting hop.
fn set_in_flight(inner: &Inner, hop_index: usize, value: bool) {
    let idx = inner.hops[hop_index]
        .reporting_index
        .expect("reporting hop has a cache slot");
    inner.in_flight[idx].store(value, Ordering::Release);
}

/// How long the runner sleeps: until the next hop is due, jittered per hop
/// already, capped by the global interval.
fn next_wake(inner: &Inner, now: Instant) -> Duration {
    let states = inner.reporting.lock().unwrap();
    states
        .iter()
        .map(|s| s.next_due.saturating_duration_since(now))
        .min()
        .unwrap_or(inner.interval)
        .min(inner.interval)
        .max(MIN_SLEEP)
}

/// ±20% around the interval, so replicas do not probe in lockstep
/// (`CONTRACT.md` §6.1).
fn jittered(interval: Duration) -> Duration {
    let factor = rand::thread_rng().gen_range(1.0 - JITTER..=1.0 + JITTER);
    // Intervals are bounded (10–300 s, or a hop's own minimum), so this
    // cannot overflow.
    interval.mul_f64(factor)
}

/// Log a result: debug, except a transition to or from `ok`
/// (`CONTRACT.md` §6.3 rule 1 keeps probe log lines quiet).
fn log_transition(
    hop: &str,
    previous: Option<CheckStatus>,
    current: CheckStatus,
    reason: Option<String>,
) {
    match previous {
        None => tracing::debug!(
            hop,
            status = current.as_str(),
            "readiness check produced its first result"
        ),
        Some(prev) if prev == current => {
            tracing::debug!(hop, status = current.as_str(), "readiness check unchanged")
        }
        Some(prev) if prev == CheckStatus::Ok => tracing::warn!(
            hop,
            from = prev.as_str(),
            to = current.as_str(),
            reason = reason.as_deref().unwrap_or(""),
            "readiness check stopped being ok"
        ),
        Some(prev) if current == CheckStatus::Ok => tracing::info!(
            hop,
            from = prev.as_str(),
            to = current.as_str(),
            "readiness check is ok again"
        ),
        Some(prev) => tracing::debug!(
            hop,
            from = prev.as_str(),
            to = current.as_str(),
            reason = reason.as_deref().unwrap_or(""),
            "readiness check changed"
        ),
    }
}

fn record_gate_status(hop: &Hop, status: CheckStatus, reason: Option<String>) {
    let mut last = hop.last_status.lock().unwrap();
    log_transition(&hop.name, *last, status, reason);
    *last = Some(status);
}

impl ReadinessBuilder {
    /// A gating check: the service's own datastore, without which it can
    /// serve no request. It runs inline on every `/readyz` request, bounded
    /// by the probe timeout, and its failure turns `/readyz` into 503
    /// `unavailable` (`CONTRACT.md` §2).
    ///
    /// An upstream HTTP hop MUST NOT be gating.
    pub fn gate(self, name: &str, probe: impl Probe) -> Self {
        self.push(name, true, true, Some(Arc::new(probe)))
    }

    /// A reporting check: an upstream hop, object storage, mail provider or
    /// chain RPC. It runs in the background, is cached, and never changes
    /// the HTTP status of `/readyz` (`CONTRACT.md` §2).
    pub fn report(self, name: &str, probe: impl Probe) -> Self {
        self.push(name, false, true, Some(Arc::new(probe)))
    }

    /// An optional hop. `None` (the feature is switched off) reports
    /// `not_configured` with `required: false` and never degrades the
    /// service. `Some(probe)` is a reporting hop with `required: true`
    /// (`CONTRACT.md` §3.4).
    pub fn optional<P: Probe>(self, name: &str, probe: Option<P>) -> Self {
        match probe {
            None => self.push(name, false, false, None),
            Some(probe) => self.push(name, false, true, Some(Arc::new(probe))),
        }
    }

    fn push(
        mut self,
        name: &str,
        gate: bool,
        required: bool,
        probe: Option<Arc<dyn Probe>>,
    ) -> Self {
        self.hops.push(HopSpec {
            name: name.to_owned(),
            gate,
            required,
            probe,
        });
        self
    }

    /// The build version or image tag, reported as `version`
    /// (`CONTRACT.md` §3.2).
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// The background interval for reporting checks. Clamped to the allowed
    /// range of 10–300 s (`CONTRACT.md` §6.1). Defaults to 30 s.
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
        self
    }

    /// [`ReadinessBuilder::interval`] without the lower clamp. Exists for
    /// tests that need to watch the schedule and the staleness rule move;
    /// production code must use [`ReadinessBuilder::interval`] or
    /// [`ReadinessBuilder::interval_from_env`].
    #[doc(hidden)]
    pub fn interval_unclamped(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Read the interval from `READYZ_CHECK_INTERVAL_SECONDS`, the knob that
    /// keeps probe load flat as replicas scale out (`CONTRACT.md` §6.3
    /// rule 6). A value that does not parse keeps the default; a value
    /// outside 10–300 s is clamped. Both cases warn.
    pub fn interval_from_env(mut self) -> Self {
        let raw = match std::env::var(INTERVAL_ENV_VAR) {
            Ok(raw) => raw,
            Err(std::env::VarError::NotPresent) => return self,
            Err(err) => {
                tracing::warn!(
                    var = INTERVAL_ENV_VAR,
                    error = %err,
                    "ignoring unreadable readiness interval, using default"
                );
                return self;
            }
        };
        let trimmed = raw.trim();
        match trimmed.parse::<u64>() {
            Ok(secs) => {
                let clamped = Duration::from_secs(secs).clamp(MIN_INTERVAL, MAX_INTERVAL);
                if clamped != Duration::from_secs(secs) {
                    tracing::warn!(
                        var = INTERVAL_ENV_VAR,
                        value = secs,
                        min = MIN_INTERVAL_SECONDS,
                        max = MAX_INTERVAL_SECONDS,
                        "readiness interval out of range, clamped"
                    );
                }
                self.interval = clamped;
            }
            Err(err) => {
                tracing::warn!(
                    var = INTERVAL_ENV_VAR,
                    value = trimmed,
                    error = %err,
                    "invalid readiness interval, using default"
                );
            }
        }
        self
    }

    /// The base value of the §6.4 trigger cooldown: how soon after a result a
    /// [`Readiness::hop_failed`] signal is first honoured. Clamped to the
    /// allowed range of 1–60 s. Defaults to 5 s, which bounds a hop to one
    /// probe per 5 s per replica however many requests fail.
    ///
    /// The cooldown doubles after every triggered check that comes back `ok`
    /// (capped at the hop's effective interval) and returns to this value
    /// after a triggered check that fails or after one interval without a
    /// signal (`CONTRACT.md` §6.4).
    pub fn trigger_cooldown(mut self, cooldown: Duration) -> Self {
        self.trigger_cooldown = cooldown.clamp(MIN_TRIGGER_COOLDOWN, MAX_TRIGGER_COOLDOWN);
        self
    }

    /// [`ReadinessBuilder::trigger_cooldown`] without the clamping. Exists
    /// for tests that need a cooldown shorter than a second; production code
    /// must use [`ReadinessBuilder::trigger_cooldown`] or
    /// [`ReadinessBuilder::trigger_cooldown_from_env`].
    #[doc(hidden)]
    pub fn trigger_cooldown_unclamped(mut self, cooldown: Duration) -> Self {
        self.trigger_cooldown = cooldown;
        self
    }

    /// Read the cooldown from `READYZ_TRIGGER_COOLDOWN_SECONDS`
    /// (`CONTRACT.md` §6.4). A value that does not parse keeps the default; a
    /// value outside 1–60 s is clamped. Both cases warn.
    pub fn trigger_cooldown_from_env(mut self) -> Self {
        let raw = match std::env::var(TRIGGER_COOLDOWN_ENV_VAR) {
            Ok(raw) => raw,
            Err(std::env::VarError::NotPresent) => return self,
            Err(err) => {
                tracing::warn!(
                    var = TRIGGER_COOLDOWN_ENV_VAR,
                    error = %err,
                    "ignoring unreadable readiness trigger cooldown, using default"
                );
                return self;
            }
        };
        let trimmed = raw.trim();
        match trimmed.parse::<u64>() {
            Ok(secs) => {
                let clamped =
                    Duration::from_secs(secs).clamp(MIN_TRIGGER_COOLDOWN, MAX_TRIGGER_COOLDOWN);
                if clamped != Duration::from_secs(secs) {
                    tracing::warn!(
                        var = TRIGGER_COOLDOWN_ENV_VAR,
                        value = secs,
                        min = MIN_TRIGGER_COOLDOWN_SECONDS,
                        max = MAX_TRIGGER_COOLDOWN_SECONDS,
                        "readiness trigger cooldown out of range, clamped"
                    );
                }
                self.trigger_cooldown = clamped;
            }
            Err(err) => {
                tracing::warn!(
                    var = TRIGGER_COOLDOWN_ENV_VAR,
                    value = trimmed,
                    error = %err,
                    "invalid readiness trigger cooldown, using default"
                );
            }
        }
        self
    }

    /// The per-probe timeout. Defaults to 2 s (`CONTRACT.md` §6.1), which is
    /// also the bound a gating check must meet.
    pub fn probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Finish. Panics on a hop name that is not `^[a-z][a-z0-9_]*$`
    /// (`CONTRACT.md` §4) or on a duplicate hop name.
    pub fn build(self) -> Readiness {
        let mut seen: Vec<&str> = Vec::with_capacity(self.hops.len());
        for spec in &self.hops {
            if let Err(problem) = validate_hop_name(&spec.name) {
                panic!("invalid readiness hop name {:?}: {}", spec.name, problem);
            }
            if seen.contains(&spec.name.as_str()) {
                panic!("duplicate readiness hop name {:?}", spec.name);
            }
            seen.push(spec.name.as_str());
        }

        let mut hops = Vec::with_capacity(self.hops.len());
        let mut reporting = Vec::new();
        for spec in self.hops {
            let reporting_index = match (&spec.probe, spec.gate) {
                (Some(_), false) => {
                    // Due immediately: the first cycle runs right after the
                    // listener is up (CONTRACT.md §6.1).
                    reporting.push(ReportingState {
                        cached: None,
                        next_due: Instant::now(),
                        cooldown_current: self.trigger_cooldown,
                        last_signal_at: None,
                        triggered: false,
                    });
                    Some(reporting.len() - 1)
                }
                _ => None,
            };
            hops.push(Hop {
                name: spec.name,
                gate: spec.gate,
                required: spec.required,
                probe: spec.probe,
                reporting_index,
                last_status: Mutex::new(None),
            });
        }
        let in_flight = reporting.iter().map(|_| AtomicBool::new(false)).collect();

        Readiness(Arc::new(Inner {
            service: self.service,
            env: self.env,
            version: self.version,
            interval: self.interval,
            probe_timeout: self.probe_timeout,
            trigger_cooldown: self.trigger_cooldown,
            hops,
            reporting: Mutex::new(reporting),
            in_flight,
            notify: Notify::new(),
            started_at: Mutex::new(Instant::now()),
            draining: AtomicBool::new(false),
        }))
    }
}

/// Hop names are `snake_case` and name the callee (`CONTRACT.md` §4), which
/// the `checks` map of the schema enforces too.
fn validate_hop_name(name: &str) -> Result<(), &'static str> {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        Some(_) => return Err("must start with a lowercase letter"),
        None => return Err("must not be empty"),
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Err("only lowercase letters, digits and '_' are allowed");
    }
    Ok(())
}

/// `CONTRACT.md` §6.3 rule 1: every probe request carries this header, so
/// the upstream can keep it out of traffic metrics and per-key budgets.
pub const PROBE_HEADER: &str = "x-readiness-probe";

/// Whether the request carries [`PROBE_HEADER`] with a caller service key.
pub fn is_probe_request(headers: &HeaderMap) -> bool {
    headers
        .get(PROBE_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty())
}

/// Whether the failure of a real request to a hop should be signalled with
/// [`Readiness::hop_failed`] (`CONTRACT.md` §6.4).
///
/// `None` means no answer arrived at all — a transport error, a DNS or TLS
/// failure, or a timeout — and triggers. `Some(5xx)` triggers. Anything else
/// does not: a 4xx is an answer about that request (bad input, no
/// permission), not about the hop, which is the undecidable-versus-rejected
/// rule of `CONTRACT.md` §3.5.
pub fn should_trigger(status: Option<u16>) -> bool {
    match status {
        None => true,
        Some(status) => (500..=599).contains(&status),
    }
}
