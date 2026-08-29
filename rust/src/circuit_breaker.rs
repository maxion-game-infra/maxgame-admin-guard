//! A small circuit breaker for outbound calls to the admin IdP (contract
//! rule 7).
//!
//! Its job is not resilience — an open breaker still refuses the write —
//! but blast radius: when the IdP is down, every mutation would otherwise
//! wait out the full introspection timeout before failing, and those waits
//! stack up into a thread-starved service answering nothing at all.
//!
//! Only *transport* failures trip it. An `active: false` verdict is the IdP
//! working correctly and must never count as a failure, or a single revoked
//! token retried enough times would take the whole admin write surface down.
//!
//! "Half-open" is meant literally: once the reset window passes, exactly one
//! caller is lent a probe call and everyone else still fails fast. Reopening
//! the gate for everybody after the window would undo the whole point during
//! a long outage — the pile-up the breaker exists to prevent would simply
//! recur every reset window.
//!
//! Lending something out raises the question of what happens when it is not
//! given back, and the answer here has to be structural rather than a
//! convention both call sites are asked to remember.
//! [`CircuitBreaker::allow_request`] hands out a [`RequestPermit`] rather
//! than a `bool`, and the permit's `Drop` returns the probe — on the happy
//! path, on an early return, through a panic, and, the case that actually
//! happens in production, when the whole future is cancelled. Both call
//! sites `.await` between taking the permit and reporting an outcome, so a
//! client that disconnects or an outer `tokio::time::timeout` that fires
//! drops the future without ever reaching the reporting code.
//!
//! A probe that is never handed back would be a latch, not a breaker: with
//! it outstanding every later call fails fast, and the only thing that can
//! clear it is an answer from a probe that no longer exists. The IdP coming
//! back healthy would not help, because nothing would be allowed to go and
//! find out. Since offline verification runs on *every* admin request, that
//! is a whole service answering 503 until it is restarted. The `Drop` is the
//! only thing standing between "half-open" and that, which is why it is
//! documented at length on [`RequestPermit`] and covered by tests that drop
//! a permit rather than report through it.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clock::{Clock, SystemClock};

pub struct CircuitBreaker {
    failure_threshold: u32,
    reset_after: Duration,
    consecutive_failures: AtomicU32,
    opened_at: Mutex<Option<Instant>>,
    /// Set while the one probe call allowed after the reset window is still
    /// outstanding. Without it "half-open" would not be half of anything:
    /// clearing `opened_at` on the first caller past the window reopens the
    /// gate for *everyone*, so a down IdP gets a fresh herd every reset
    /// window instead of a single trial call.
    ///
    /// Owned for its whole life by the [`RequestPermit`] it was granted to,
    /// and cleared in exactly one place — that permit's `Drop`. Reporting an
    /// outcome deliberately does *not* clear it. Having a single release
    /// point is what makes "the probe always comes back" a property you can
    /// check by reading one function, instead of one you have to re-audit on
    /// every exit path of two HTTP clients each time either of them grows a
    /// new `return`.
    probe_in_flight: AtomicBool,
    clock: Arc<dyn Clock>,
}

impl CircuitBreaker {
    /// The real-clock breaker every production guard uses.
    pub fn new(failure_threshold: u32, reset_after: Duration) -> Self {
        Self::with_clock(failure_threshold, reset_after, Arc::new(SystemClock))
    }

    /// A breaker driven by `clock` instead of the wall clock, for tests that
    /// need to cross the reset window without sleeping.
    pub fn with_clock(
        failure_threshold: u32,
        reset_after: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            failure_threshold,
            reset_after,
            consecutive_failures: AtomicU32::new(0),
            opened_at: Mutex::new(None),
            probe_in_flight: AtomicBool::new(false),
            clock,
        }
    }

    /// Whether a call may go out, answered with a permit rather than a
    /// `bool`.
    ///
    /// `Some` is permission to make the call, and the caller reports what
    /// came of it through [`RequestPermit::record_success`] or
    /// [`RequestPermit::record_failure`]. `None` is the breaker refusing:
    /// it is open, and either the reset window has not passed yet or its one
    /// probe is already out with somebody else.
    ///
    /// The permit exists so that the third possibility — the caller never
    /// reports anything because it was cancelled — cannot cost the breaker
    /// its probe. See [`RequestPermit`].
    ///
    /// `opened_at` deliberately survives the probe being handed out. The
    /// breaker is still open — it has merely lent one call to find out
    /// whether it can close — and only an answer, not the passage of time,
    /// decides which way that goes.
    pub fn allow_request(&self) -> Option<RequestPermit<'_>> {
        let opened_at = self
            .opened_at
            .lock()
            .expect("circuit breaker lock poisoned");
        match *opened_at {
            None => Some(RequestPermit::new(self, false)),
            Some(at) if self.clock.now().saturating_duration_since(at) >= self.reset_after => self
                .probe_in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
                .then(|| RequestPermit::new(self, true)),
            Some(_) => None,
        }
    }

    /// Reached only through [`RequestPermit::record_success`], which is what
    /// keeps the probe's release paired with the permit's lifetime rather
    /// than with the outcome.
    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        *self
            .opened_at
            .lock()
            .expect("circuit breaker lock poisoned") = None;
    }

    /// Reached only through [`RequestPermit::record_failure`]. `was_probe`
    /// comes from the permit rather than from `probe_in_flight`, because the
    /// permit still holds the probe at this point and goes on holding it
    /// until it is dropped.
    fn record_failure(&self, was_probe: bool) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        // A failure reported by the half-open probe reopens the breaker for
        // another full window regardless of the counter: the point of the
        // probe was to answer "is it back yet", and the answer was no.
        if failures < self.failure_threshold && !was_probe {
            return;
        }
        let mut opened_at = self
            .opened_at
            .lock()
            .expect("circuit breaker lock poisoned");
        match *opened_at {
            None => {
                tracing::warn!(failures, "admin IdP circuit opened");
                *opened_at = Some(self.clock.now());
            }
            // Already open, and the probe we lent out just came back
            // failing: restart the window from now rather than let a
            // stale `opened_at` hand out a new probe on every call.
            Some(_) if was_probe => *opened_at = Some(self.clock.now()),
            Some(_) => {}
        }
    }
}

/// Permission to make one outbound call, and — when the breaker is
/// half-open — custody of the single probe it lends out.
///
/// The whole reason this is a value with a `Drop` rather than a `bool` is
/// the gap between taking it and reporting an outcome. Both call sites
/// `.await` across that gap, and an `.await` is a point at which the future
/// can simply cease to exist: axum drops the handler future when a client
/// disconnects, and an outer `tokio::time::timeout` drops the future it is
/// wrapping when the deadline passes. Neither unwinds through the code that
/// would have reported back. Anything the breaker needs done in that case
/// has to be done by a destructor, the same reasoning that produced
/// `FetchLease` on the JWKS fetch next door.
#[must_use = "an unused permit returns the half-open probe immediately, so the call it was \
              granted for is never counted"]
pub struct RequestPermit<'a> {
    breaker: &'a CircuitBreaker,
    /// Whether this permit carries the breaker's one half-open probe. A
    /// permit taken while the breaker is closed carries nothing, so its
    /// `Drop` has nothing to give back.
    holds_probe: bool,
    /// Whether an outcome has already been reported through this permit.
    /// Only guards against a call site reporting twice; the probe is
    /// returned by `Drop` either way.
    settled: AtomicBool,
}

impl<'a> RequestPermit<'a> {
    fn new(breaker: &'a CircuitBreaker, holds_probe: bool) -> Self {
        Self {
            breaker,
            holds_probe,
            settled: AtomicBool::new(false),
        }
    }

    /// The IdP answered. Any answer counts, including one that denies the
    /// request — see the introspect client for why a revoked token is the
    /// dependency working rather than failing.
    pub fn record_success(&self) {
        if self.settle() {
            self.breaker.record_success();
        }
    }

    /// The call could not be made, or could not be turned into an answer: a
    /// transport error, a non-2xx, an unparseable body.
    pub fn record_failure(&self) {
        if self.settle() {
            self.breaker.record_failure(self.holds_probe);
        }
    }

    /// True the first time an outcome is reported. A permit answers once;
    /// a second report is a call-site mistake, and dropping it beats
    /// double-counting it into the failure run.
    fn settle(&self) -> bool {
        !self.settled.swap(true, Ordering::AcqRel)
    }
}

impl Drop for RequestPermit<'_> {
    fn drop(&mut self) {
        if !self.holds_probe {
            return;
        }
        // A probe handed back without an outcome counts as *no information*,
        // not as a failure, and the difference is worth being deliberate
        // about because it is the whole recovery story.
        //
        // The only way to arrive here unsettled is cancellation: the client
        // hung up, or an outer request timeout fired. Both are facts about
        // our own caller, not about the IdP — the call may well have been
        // answered a millisecond later — so folding them into
        // `consecutive_failures`, or restarting the open window from here,
        // would let caller impatience open and hold open a breaker whose
        // entire job is to describe the upstream. A service under load sheds
        // requests, which would then trip the breaker, which sheds more.
        //
        // The objection to "no information" is that it might let a herd
        // through, and it cannot: `probe_in_flight` is what caps half-open
        // concurrency at one call, and handing it back gives it to at most
        // one next caller. The worst case is a chain of single probes, which
        // is exactly what half-open promises. What it buys is the property
        // the breaker had before it lent probes out at all — no answer
        // leaves it no worse off than before the loan, so it still closes
        // itself once the IdP recovers, instead of needing a pod restart.
        self.breaker.probe_in_flight.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    /// One failed call, reported the way both call sites report it: take a
    /// permit, use it, let it go. Panics if the breaker refuses, because a
    /// test that silently records nothing would pass for the wrong reason.
    fn fail(breaker: &CircuitBreaker) {
        breaker
            .allow_request()
            .expect("the breaker refused a call this test needed to go out")
            .record_failure();
    }

    /// One successful call, same shape as [`fail`].
    fn succeed(breaker: &CircuitBreaker) {
        breaker
            .allow_request()
            .expect("the breaker refused a call this test needed to go out")
            .record_success();
    }

    #[test]
    fn the_circuit_opens_only_after_the_threshold_is_reached() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(30));
        for _ in 0..2 {
            fail(&breaker);
            assert!(breaker.allow_request().is_some());
        }
        fail(&breaker);
        assert!(breaker.allow_request().is_none());
    }

    #[test]
    fn a_success_resets_the_run_of_failures() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(30));
        fail(&breaker);
        fail(&breaker);
        succeed(&breaker);
        fail(&breaker);
        fail(&breaker);
        assert!(
            breaker.allow_request().is_some(),
            "the run restarted after the success"
        );
    }

    #[test]
    fn an_open_circuit_half_opens_once_the_reset_window_passes() {
        let breaker = CircuitBreaker::new(1, Duration::from_millis(0));
        fail(&breaker);
        assert!(
            breaker.allow_request().is_some(),
            "a zero reset window lets the next probe through"
        );

        let breaker = CircuitBreaker::new(1, Duration::from_secs(300));
        fail(&breaker);
        assert!(breaker.allow_request().is_none());
    }

    /// The half-open window lends out exactly one call. Everything behind
    /// it keeps failing fast — otherwise a down IdP collects a fresh herd
    /// every reset window, each member waiting out the full timeout.
    ///
    /// The probe permit is *held* across the refusals rather than dropped,
    /// because holding it is what an in-flight call does. Letting it go at
    /// the end of the statement that took it would hand the probe straight
    /// back and the rest of the test would measure nothing.
    #[test]
    fn only_one_probe_is_let_through_per_reset_window() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        fail(&breaker);
        assert!(breaker.allow_request().is_none(), "the breaker is open");

        clock.advance(Duration::from_millis(30_001));
        let probe = breaker
            .allow_request()
            .expect("the first caller past the window probes");
        for _ in 0..5 {
            assert!(
                breaker.allow_request().is_none(),
                "the probe is still outstanding; nobody else gets through"
            );
        }
        drop(probe);
    }

    /// A failing probe restarts the window instead of leaving `opened_at`
    /// stale, which would hand out a new probe to every subsequent caller.
    #[test]
    fn a_failing_probe_reopens_the_breaker_for_another_full_window() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        fail(&breaker);
        clock.advance(Duration::from_millis(30_001));
        let probe = breaker.allow_request().expect("probe granted");

        probe.record_failure();
        drop(probe);
        assert!(
            breaker.allow_request().is_none(),
            "the probe failed; still open"
        );
        clock.advance(Duration::from_millis(30_001));
        assert!(
            breaker.allow_request().is_some(),
            "a new window, a new probe"
        );
    }

    /// A succeeding probe closes the breaker outright.
    #[test]
    fn a_succeeding_probe_closes_the_breaker() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        fail(&breaker);
        clock.advance(Duration::from_millis(30_001));
        let probe = breaker.allow_request().expect("probe granted");

        probe.record_success();
        drop(probe);
        for _ in 0..5 {
            assert!(
                breaker.allow_request().is_some(),
                "the breaker is closed again"
            );
        }
    }

    #[test]
    fn a_fake_clock_can_cross_the_reset_window_without_sleeping() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        fail(&breaker);
        assert!(breaker.allow_request().is_none());
        clock.advance(Duration::from_millis(30_001));
        assert!(breaker.allow_request().is_some(), "the window has passed");
    }

    /// The regression this permit exists for. A probe that is granted and
    /// then simply lost — the caller went away without reporting anything —
    /// used to leave `probe_in_flight` set for the life of the process. The
    /// compare-exchange in `allow_request` then failed on every subsequent
    /// call, and nothing could ever clear it, because every path to
    /// reporting an outcome starts by being granted a probe. The breaker
    /// stopped being a breaker and became a latch that only a restart
    /// opened, however healthy the IdP got.
    #[test]
    fn a_probe_dropped_without_an_answer_does_not_latch_the_breaker_shut() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        fail(&breaker);
        clock.advance(Duration::from_millis(30_001));

        // Granted, and then the caller vanishes.
        drop(breaker.allow_request().expect("probe granted"));

        // Immediately, with no further passage of time: a probe that told us
        // nothing leaves the breaker exactly as it was before it was lent
        // out, so the window it had already served is still served. That is
        // the "no information" half of the `Drop` comment, asserted — had
        // the drop been treated as a failure, this would be `None` and
        // recovery would cost another full window per cancelled request.
        let probe = breaker
            .allow_request()
            .expect("an unanswered probe must come back, not be lost");

        // And the recovery it makes possible actually completes.
        probe.record_success();
        drop(probe);
        for _ in 0..5 {
            assert!(
                breaker.allow_request().is_some(),
                "the IdP answered the retry; the breaker closes with it"
            );
        }
    }

    /// Long after the fact, which is the shape the production report took:
    /// the pod was still refusing every admin request the next day.
    #[test]
    fn a_dropped_probe_does_not_still_refuse_calls_a_day_later() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        fail(&breaker);
        clock.advance(Duration::from_millis(30_001));
        drop(breaker.allow_request().expect("probe granted"));

        clock.advance(Duration::from_secs(24 * 60 * 60));
        assert!(
            breaker.allow_request().is_some(),
            "a breaker that has not been told anything for a day must still be willing to look"
        );
    }

    /// The same loss again, but caused the way it is actually caused rather
    /// than by an explicit `drop`. Nothing here returns the probe on
    /// purpose: `record_failure` below is unreachable, because
    /// `tokio::time::timeout` drops the future while it is parked on the
    /// sleep. A destructor is the only thing that still runs, which is
    /// precisely why the probe's release lives in one.
    #[tokio::test]
    async fn a_probe_lost_to_a_cancelled_future_is_still_returned() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        fail(&breaker);
        clock.advance(Duration::from_millis(30_001));

        let outcome = tokio::time::timeout(Duration::from_millis(20), async {
            let permit = breaker.allow_request().expect("probe granted");
            // Stands in for the HTTP call: an outer deadline (a request
            // timeout, or a client that hung up) fires first.
            tokio::time::sleep(Duration::from_secs(30)).await;
            permit.record_failure();
        })
        .await;
        assert!(
            outcome.is_err(),
            "the probe has to still be in flight when it is cancelled, or this proves nothing"
        );

        assert!(
            breaker.allow_request().is_some(),
            "a cancelled probe must leave the breaker able to try again"
        );
    }

    /// Cancellation does not weaken the one-at-a-time guarantee either: the
    /// probe is returned to the *next* caller, not to all of them.
    #[test]
    fn a_returned_probe_is_handed_to_one_caller_at_a_time() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        fail(&breaker);
        clock.advance(Duration::from_millis(30_001));
        drop(breaker.allow_request().expect("probe granted"));

        let probe = breaker.allow_request().expect("the next caller gets it");
        for _ in 0..5 {
            assert!(
                breaker.allow_request().is_none(),
                "half-open still means one call in flight, not a herd"
            );
        }
        drop(probe);
    }
}
