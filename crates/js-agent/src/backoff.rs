//! How long to wait after a failed exchange: doubling, capped, jittered, and never
//! shorter than the wait the server asked for (within a bound).

use std::time::Duration;

/// The ceiling of the first wait.
pub const BASE: Duration = Duration::from_secs(1);

/// The longest ceiling a run of failures reaches.
pub const CAP: Duration = Duration::from_secs(60);

/// The longest `Retry-After` the agent obeys: a server that asks for more is not
/// allowed to silence the host for longer.
pub const RETRY_AFTER_CAP: Duration = Duration::from_secs(120);

/// Doublings after which the ceiling is above [`CAP`] for any [`BASE`] of a second.
const MAX_DOUBLINGS: u32 = 6;

/// A source of randomness for the wait, so a test picks the number.
pub trait Jitter {
    /// A number from 0 to `max`, both included.
    fn up_to(&mut self, max: u64) -> u64;
}

/// Randomness from the operating system. When it cannot be read the jitter is zero, which
/// keeps the lower end of every wait.
#[derive(Debug, Clone, Copy)]
pub struct SystemJitter;

impl Jitter for SystemJitter {
    fn up_to(&mut self, max: u64) -> u64 {
        let mut bytes = [0u8; 8];
        let _ = getrandom::getrandom(&mut bytes);
        u64::from_le_bytes(bytes) % max.saturating_add(1)
    }
}

/// The run of failures since the last success.
#[derive(Debug, Default)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// A success: the next failure waits the shortest time again.
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// The wait after one more failure. The ceiling is [`BASE`] doubled once for each
    /// failure before this one, up to [`CAP`]; the wait is somewhere from half the
    /// ceiling to the ceiling, so a fleet that failed together does not return together.
    /// A `retry_after` the server asked for raises the wait to that, at most
    /// [`RETRY_AFTER_CAP`].
    pub fn next(&mut self, jitter: &mut dyn Jitter, retry_after: Option<Duration>) -> Duration {
        let doublings = self.failures.min(MAX_DOUBLINGS);
        self.failures = self.failures.saturating_add(1);
        let ceiling = BASE.saturating_mul(1 << doublings).min(CAP);
        let half = ceiling / 2;
        let room = u64::try_from(half.as_millis()).unwrap_or(u64::MAX);
        let spread = Duration::from_millis(jitter.up_to(room).min(room));
        let asked = retry_after.unwrap_or_default().min(RETRY_AFTER_CAP);
        (half + spread).max(asked)
    }
}
