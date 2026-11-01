//! The failure-brake keys of an elastic pool: how long the cooldown after a failed boot may
//! grow, how many failures in a row trip the pool to plan, and how long a host must be heard
//! from before a boot counts as a success.

use std::time::Duration;

use super::Section;
use crate::error::StartupError;
use js_scaler::{BOOT_FAIL_COOLDOWN_MAX, BOOT_FAIL_RESET_AFTER, BOOT_FAIL_TRIP_AFTER};

/// The most failures in a row the file may name: more is no brake at all.
const MOST_TRIPS: u64 = 100;

/// The shortest ceiling: the first cooldown, which the ceiling cannot be below.
const SHORTEST: Duration = Duration::from_secs(10 * 60);

/// The shortest time a host must be heard from before the count of failures ends.
const SHORTEST_RESET: Duration = Duration::from_secs(60);

/// The longest ceiling of the cooldown, and the longest time a host must be heard from: a day.
const LONGEST: Duration = Duration::from_secs(24 * 60 * 60);

/// The failure brake of a pool, as the plan carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrakePlan {
    /// The longest the cooldown after failed boots grows to (it doubles from 10 minutes).
    pub cooldown_max: Duration,
    /// How many failures in a row trip the pool to plan until an operator resets it.
    pub trip_after: u32,
    /// How long a host must be heard from, unbroken, before its boot ends the count.
    pub reset_after: Duration,
}

/// The brake keys as the pool's table gave them.
pub(super) struct Given {
    cooldown_max: Option<Duration>,
    trip_after: Option<u64>,
    reset_after: Option<Duration>,
}

impl Given {
    /// Reads the keys out of the pool's table.
    pub(super) fn read(s: &mut Section) -> Result<Self, StartupError> {
        Ok(Self {
            cooldown_max: s.duration("boot_fail_cooldown_max")?,
            trip_after: s.integer("boot_fail_trip_after")?,
            reset_after: s.duration("boot_fail_reset_after")?,
        })
    }

    /// The first key given, for a pool that cannot use any.
    pub(super) fn first_given(&self) -> Option<&'static str> {
        let given = [
            ("boot_fail_cooldown_max", self.cooldown_max.is_some()),
            ("boot_fail_trip_after", self.trip_after.is_some()),
            ("boot_fail_reset_after", self.reset_after.is_some()),
        ];
        given.into_iter().find(|(_, set)| *set).map(|(key, _)| key)
    }

    /// The plan: a ceiling of 2 hours unless the file says (from 10 minutes to 24 hours), a trip after
    /// 3 failures in a row unless it says (from 1 to 100), and 10 minutes of a host being
    /// heard unless it says (from 1 minute to 24 hours).
    pub(super) fn check(&self, s: &Section) -> Result<BrakePlan, StartupError> {
        let cooldown_max = self.cooldown_max.unwrap_or(BOOT_FAIL_COOLDOWN_MAX);
        if cooldown_max < SHORTEST {
            let reason = "must be at least 10 minutes, the first cooldown";
            return Err(s.fail("boot_fail_cooldown_max", reason));
        }
        if cooldown_max > LONGEST {
            return Err(s.fail("boot_fail_cooldown_max", "must be at most 24 hours"));
        }
        let trips = self.trip_after.unwrap_or(u64::from(BOOT_FAIL_TRIP_AFTER));
        if !(1..=MOST_TRIPS).contains(&trips) {
            return Err(s.fail("boot_fail_trip_after", "must be from 1 to 100"));
        }
        let reset_after = self.reset_after.unwrap_or(BOOT_FAIL_RESET_AFTER);
        if reset_after < SHORTEST_RESET {
            return Err(s.fail("boot_fail_reset_after", "must be at least 1 minute"));
        }
        if reset_after > LONGEST {
            return Err(s.fail("boot_fail_reset_after", "must be at most 24 hours"));
        }
        Ok(BrakePlan {
            cooldown_max,
            trip_after: u32::try_from(trips).unwrap_or(BOOT_FAIL_TRIP_AFTER),
            reset_after,
        })
    }
}
