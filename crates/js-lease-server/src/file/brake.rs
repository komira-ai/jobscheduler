//! The failure-brake keys of an elastic pool: how long the cooldown after a failed boot may
//! grow, and how many failures in a row trip the pool to plan.

use std::time::Duration;

use super::Section;
use crate::error::StartupError;
use js_scaler::{BOOT_FAIL_COOLDOWN_MAX, BOOT_FAIL_TRIP_AFTER};

/// The most failures in a row the file may name: more is no brake at all.
const MOST_TRIPS: u64 = 100;

/// The shortest ceiling: the first cooldown, which the ceiling cannot be below.
const SHORTEST: Duration = Duration::from_secs(10 * 60);

/// The brake keys as the pool's table gave them.
pub(super) struct Given {
    pub(super) cooldown_max: Option<Duration>,
    pub(super) trip_after: Option<u64>,
}

impl Given {
    /// Reads the keys out of the pool's table.
    pub(super) fn read(s: &mut Section) -> Result<Self, StartupError> {
        Ok(Self {
            cooldown_max: s.duration("boot_fail_cooldown_max")?,
            trip_after: s.integer("boot_fail_trip_after")?,
        })
    }

    /// The first key given, for a pool that cannot use any.
    pub(super) fn first_given(&self) -> Option<&'static str> {
        let given = [
            ("boot_fail_cooldown_max", self.cooldown_max.is_some()),
            ("boot_fail_trip_after", self.trip_after.is_some()),
        ];
        given.into_iter().find(|(_, set)| *set).map(|(key, _)| key)
    }

    /// The ceiling on the cooldown (2 hours unless the file says, at least 10 minutes) and
    /// the failures in a row that trip the pool (3 unless the file says, from 1 to 100).
    pub(super) fn check(&self, s: &Section) -> Result<(Duration, u32), StartupError> {
        let ceiling = self.cooldown_max.unwrap_or(BOOT_FAIL_COOLDOWN_MAX);
        if ceiling < SHORTEST {
            let reason = "must be at least 10 minutes, the first cooldown";
            return Err(s.fail("boot_fail_cooldown_max", reason));
        }
        let trips = self.trip_after.unwrap_or(u64::from(BOOT_FAIL_TRIP_AFTER));
        if !(1..=MOST_TRIPS).contains(&trips) {
            return Err(s.fail("boot_fail_trip_after", "must be from 1 to 100"));
        }
        Ok((
            ceiling,
            u32::try_from(trips).unwrap_or(BOOT_FAIL_TRIP_AFTER),
        ))
    }
}
