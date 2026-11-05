//! Wall time, as the signer and a credential's expiry read it. A clock is injected so that no
//! test waits and every signature a test makes is reproducible.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

/// A wall clock.
pub trait Clock: Send + Sync {
    /// The time now.
    fn now(&self) -> SystemTime;
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now(&self) -> SystemTime {
        (**self).now()
    }
}

/// The operating system's wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that moves only when told to, for tests.
#[derive(Debug)]
pub struct ManualClock {
    at: Mutex<SystemTime>,
}

impl ManualClock {
    /// A clock that reads `at`.
    pub fn new(at: SystemTime) -> Self {
        Self { at: Mutex::new(at) }
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        let mut at = self.at.lock().unwrap_or_else(PoisonError::into_inner);
        *at += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.at.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
