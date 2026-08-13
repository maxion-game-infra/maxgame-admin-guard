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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clock::{Clock, SystemClock};

pub struct CircuitBreaker {
    failure_threshold: u32,
    reset_after: Duration,
    consecutive_failures: AtomicU32,
    opened_at: Mutex<Option<Instant>>,
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
            clock,
        }
    }

    /// Whether a call may go out. A breaker that has been open longer than
    /// the reset window lets one call through to probe.
    pub fn allow_request(&self) -> bool {
        let mut opened_at = self
            .opened_at
            .lock()
            .expect("circuit breaker lock poisoned");
        match *opened_at {
            None => true,
            Some(at) if self.clock.now().saturating_duration_since(at) >= self.reset_after => {
                // Half-open: clear the state so the probe call decides.
                *opened_at = None;
                self.consecutive_failures.store(0, Ordering::Relaxed);
                true
            }
            Some(_) => false,
        }
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        *self
            .opened_at
            .lock()
            .expect("circuit breaker lock poisoned") = None;
    }

    pub fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.failure_threshold {
            let mut opened_at = self
                .opened_at
                .lock()
                .expect("circuit breaker lock poisoned");
            if opened_at.is_none() {
                tracing::warn!(failures, "admin IdP circuit opened");
                *opened_at = Some(self.clock.now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    #[test]
    fn the_circuit_opens_only_after_the_threshold_is_reached() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(30));
        for _ in 0..2 {
            breaker.record_failure();
            assert!(breaker.allow_request());
        }
        breaker.record_failure();
        assert!(!breaker.allow_request());
    }

    #[test]
    fn a_success_resets_the_run_of_failures() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(30));
        breaker.record_failure();
        breaker.record_failure();
        breaker.record_success();
        breaker.record_failure();
        breaker.record_failure();
        assert!(
            breaker.allow_request(),
            "the run restarted after the success"
        );
    }

    #[test]
    fn an_open_circuit_half_opens_once_the_reset_window_passes() {
        let breaker = CircuitBreaker::new(1, Duration::from_millis(0));
        breaker.record_failure();
        assert!(
            breaker.allow_request(),
            "a zero reset window lets the next probe through"
        );

        let breaker = CircuitBreaker::new(1, Duration::from_secs(300));
        breaker.record_failure();
        assert!(!breaker.allow_request());
    }

    #[test]
    fn a_fake_clock_can_cross_the_reset_window_without_sleeping() {
        let clock = Arc::new(FakeClock::new());
        let breaker = CircuitBreaker::with_clock(1, Duration::from_secs(30), clock.clone());
        breaker.record_failure();
        assert!(!breaker.allow_request());
        clock.advance(Duration::from_millis(30_001));
        assert!(breaker.allow_request(), "the window has passed");
    }
}
