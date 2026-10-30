//! The memory headroom of a pool: the part of a host's memory the pool books.

use std::fmt;

/// The name of the configuration key that sets a [`MemoryPercent`], wherever it is written.
pub const MEMORY_PERCENT_KEY: &str = "memory_schedulable_percent";

/// The percent of a host's memory that may be booked: a whole number from 1 to 100.
///
/// It applies to the memory an operator configures for a pool or a host, never to a
/// figure an agent reports about itself. The default is 100, which books all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemoryPercent(u8);

/// A percent outside 1 to 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("memory_schedulable_percent must be from 1 to 100")]
pub struct MemoryPercentError;

impl MemoryPercent {
    /// All of the memory: the default.
    pub const FULL: Self = Self(100);

    /// `percent`, when it is from 1 to 100.
    pub fn new(percent: u64) -> Result<Self, MemoryPercentError> {
        match u8::try_from(percent) {
            Ok(percent @ 1..=100) => Ok(Self(percent)),
            _ => Err(MemoryPercentError),
        }
    }

    /// The percent, from 1 to 100.
    pub fn get(self) -> u8 {
        self.0
    }

    /// The part of `bytes` this percent books, rounded down.
    pub fn of(self, bytes: u64) -> u64 {
        let booked = u128::from(bytes) * u128::from(self.0) / 100;
        u64::try_from(booked).unwrap_or(u64::MAX)
    }
}

impl Default for MemoryPercent {
    fn default() -> Self {
        Self::FULL
    }
}

impl fmt::Display for MemoryPercent {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self.0)
    }
}

/// What a host's memory is, what share of it is booked, and what that comes to: the three
/// figures an operator reads off a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryHeadroom {
    configured_bytes: u64,
    percent: MemoryPercent,
}

impl MemoryHeadroom {
    /// The memory the operator configured and the percent of it that is booked.
    pub fn new(configured_bytes: u64, percent: MemoryPercent) -> Self {
        Self {
            configured_bytes,
            percent,
        }
    }

    /// All of `bytes` booked.
    pub fn full(bytes: u64) -> Self {
        Self::new(bytes, MemoryPercent::FULL)
    }

    /// The memory the operator configured.
    pub fn configured_bytes(&self) -> u64 {
        self.configured_bytes
    }

    /// The percent of it that is booked.
    pub fn percent(&self) -> MemoryPercent {
        self.percent
    }

    /// The memory that is booked: the configured memory at the percent, rounded down.
    pub fn bookable_bytes(&self) -> u64 {
        self.percent.of(self.configured_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// Asserts that 1 and 100 are the ends of what is accepted and that 0, 101 and a
    /// number too large for a byte are refused with the key named. It catches a bound off
    /// by one and a cast that wraps 256 to 0.
    #[test]
    fn only_one_to_a_hundred_is_a_percent() {
        for good in [1, 50, 100] {
            assert_eq!(
                MemoryPercent::new(good).unwrap().get(),
                u8::try_from(good).unwrap()
            );
        }
        for bad in [0, 101, 256, 357, u64::MAX] {
            let refused = MemoryPercent::new(bad).unwrap_err();
            assert_eq!(
                refused.to_string(),
                "memory_schedulable_percent must be from 1 to 100"
            );
        }
        assert_eq!(MemoryPercent::default(), MemoryPercent::FULL);
        assert_eq!(MemoryPercent::FULL.to_string(), "100");
    }

    /// Asserts that the booked part is rounded down, is exact when it divides, is the
    /// whole at 100, and does not overflow for the largest memory. It catches a
    /// rounding up (one byte too many booked) and a multiply that wraps.
    #[test]
    fn the_booked_part_rounds_down() {
        let ninety = MemoryPercent::new(90).unwrap();
        assert_eq!(ninety.of(251 * GIB), 251 * GIB * 90 / 100);
        assert_eq!(ninety.of(251 * GIB), 242_558_278_041);
        assert_eq!(MemoryPercent::new(33).unwrap().of(10), 3);
        assert_eq!(MemoryPercent::new(1).unwrap().of(99), 0);
        assert_eq!(MemoryPercent::FULL.of(u64::MAX), u64::MAX);
        assert_eq!(ninety.of(u64::MAX), u64::MAX / 100 * 90 + 13);
    }

    /// Asserts that the headroom reports what was configured, the percent and the booked
    /// part, and that `full` books everything.
    #[test]
    fn the_headroom_reports_three_figures() {
        let headroom = MemoryHeadroom::new(10 * GIB, MemoryPercent::new(80).unwrap());
        assert_eq!(headroom.configured_bytes(), 10 * GIB);
        assert_eq!(headroom.percent().get(), 80);
        assert_eq!(headroom.bookable_bytes(), 8 * GIB);
        let full = MemoryHeadroom::full(7);
        assert_eq!(
            (full.percent(), full.bookable_bytes()),
            (MemoryPercent::FULL, 7)
        );
    }
}
