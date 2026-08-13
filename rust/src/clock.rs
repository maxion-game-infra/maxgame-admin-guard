//! An injectable clock, so the circuit breaker's timing can be tested
//! without sleeping.
//!
//! `Instant` cannot be constructed from an arbitrary point in time, only
//! read from `Instant::now()` or shifted by a `Duration` — both safe
//! operations. [`FakeClock`] exploits that: it fixes a base instant at
//! creation and adds an operator-controlled offset, so `advanceMs` in a
//! contract sequence case becomes one call to [`FakeClock::advance`] instead
//! of an actual wait.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The real wall clock. What every production guard uses.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that only moves when told to. Used by the contract test suite to
/// drive the breaker's 30-second reset window in a single-digit-millisecond
/// test run.
#[derive(Debug)]
pub struct FakeClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        }
    }

    pub fn advance(&self, by: Duration) {
        let mut offset = self.offset.lock().expect("fake clock lock poisoned");
        *offset += by;
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        let offset = *self.offset.lock().expect("fake clock lock poisoned");
        self.base + offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fake_clock_only_moves_when_advanced() {
        let clock = FakeClock::new();
        let t0 = clock.now();
        assert_eq!(clock.now(), t0);
        clock.advance(Duration::from_secs(30));
        assert_eq!(clock.now(), t0 + Duration::from_secs(30));
    }
}
