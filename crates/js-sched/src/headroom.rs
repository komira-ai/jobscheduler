//! The share of a worker's memory capacity that admission may book.

use js_caps::NodeCaps;
use js_types::sched::Resources;

use crate::Request;

/// The name of the key, as errors and logs spell it.
pub const MEMORY_SCHEDULABLE_PERCENT: &str = "memory_schedulable_percent";

/// A `memory_schedulable_percent` outside 1 to 100.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PercentError(pub u32);

impl std::fmt::Display for PercentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{MEMORY_SCHEDULABLE_PERCENT} must be an integer from 1 to 100, got {}",
            self.0
        )
    }
}

impl std::error::Error for PercentError {}

/// The share of a worker's reported memory capacity that admission books against, in
/// whole percent from 1 to 100; the rest is headroom. It applies to the capacity a worker
/// reports and never to a figure an agent measures or a job's learned memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryPercent(u8);

impl MemoryPercent {
    /// All of the capacity: admission is unchanged.
    pub const FULL: Self = Self(100);

    /// `percent`, refusing 0 and anything above 100.
    pub fn new(percent: u32) -> Result<Self, PercentError> {
        match u8::try_from(percent) {
            Ok(p @ 1..=100) => Ok(Self(p)),
            _ => Err(PercentError(percent)),
        }
    }

    /// `percent` held to 1 to 100: 0 becomes 1 and anything above becomes 100. For a
    /// caller that cannot fail; [`MemoryPercent::new`] refuses instead.
    #[must_use]
    pub fn clamped(percent: u32) -> Self {
        Self(u8::try_from(percent.min(100)).unwrap_or(100).max(1))
    }

    /// The percent.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// `floor(bytes * percent / 100)`.
    #[must_use]
    pub fn of_bytes(self, bytes: u64) -> u64 {
        // bytes = 100 q + r, so the floor is q p + floor(r p / 100); no step overflows.
        let p = u64::from(self.0);
        bytes / 100 * p + bytes % 100 * p / 100
    }

    /// `capacity` with its memory cut to the share; cores, GPUs, VM slots and the lease
    /// cap are as they were.
    #[must_use]
    pub fn of_capacity(self, capacity: Resources) -> Resources {
        Resources {
            memory_bytes: self.of_bytes(capacity.memory_bytes),
            ..capacity
        }
    }
}

impl MemoryPercent {
    /// Whether a worker with `caps` and the reported `capacity` could run `request` under
    /// this share: [`Request::runnable_on`] against the cut capacity. The one test that
    /// every look at another frontend's daemons uses, so none judges the raw figure.
    #[must_use]
    pub fn runnable_on(self, request: &Request, caps: &NodeCaps, capacity: &Resources) -> bool {
        request.runnable_on(caps, &self.of_capacity(*capacity))
    }
}

impl Default for MemoryPercent {
    fn default() -> Self {
        Self::FULL
    }
}
