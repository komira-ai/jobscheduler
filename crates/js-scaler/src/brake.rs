//! The failure brake of a pool: a cooldown that doubles with each failure in a row, and a
//! trip to `Plan` after too many, which only an operator's reset ends.
//!
//! The brake only removes launches. It never drains, retires or releases a machine, and it
//! reads no clock but the step's. Its state is [`Records::brake`], written with the record of
//! the failure that changed it, so a restart finds the count and the trip as they were.

use std::time::Duration;

use crate::pass::Pass;
use crate::record::{Cooldown, CooldownReason, Records};
use crate::settings::Settings;

/// How long the pool waits after its `failures`-th failure in a row: `boot_fail_cooldown`
/// doubled for each failure after the first, and never more than `boot_fail_cooldown_max`
/// (or `boot_fail_cooldown`, if that is longer).
pub(crate) fn backoff(settings: &Settings, failures: u32) -> Duration {
    let base = settings.boot_fail_cooldown;
    let doubled = failures.saturating_sub(1).min(30);
    let grown = base.saturating_mul(1_u32 << doubled);
    grown.min(settings.boot_fail_cooldown_max.max(base))
}

impl Records {
    /// Whether the brake has tripped `pool` to `Plan` and nobody has reset it.
    #[must_use]
    pub fn tripped(&self, pool: &str) -> bool {
        self.brake
            .get(pool)
            .is_some_and(|brake| brake.tripped_at.is_some())
    }
}

impl Pass<'_> {
    /// Counts one failure of the pool: toward the trip, and toward the length of the next
    /// cooldown. Returns the count.
    pub(crate) fn strike(&mut self, settings: &Settings) -> u32 {
        let now = self.now;
        let brake = self.records.brake.entry(settings.pool.clone()).or_default();
        brake.failures = brake.failures.saturating_add(1);
        if brake.failures >= settings.boot_fail_trip_after.max(1) {
            brake.tripped_at.get_or_insert(now);
        }
        brake.failures
    }

    /// The pool launches nothing until its cooldown has run. This is a failure: it counts
    /// toward the trip, and the cooldown is [`backoff`] of the count. A cooldown already
    /// running is never shortened.
    ///
    /// Each caller is the one step that moves a record into the state that is the failure
    /// (a refused acquire, a boot given up on), so a failure is counted once however many
    /// steps see its record afterward.
    pub(crate) fn cool(&mut self, settings: &Settings, reason: CooldownReason) {
        let failures = self.strike(settings);
        let until = self.now.plus(backoff(settings, failures));
        let running = self.records.cooldowns.get(&settings.pool);
        let until = running.map_or(until, |each| each.until.max(until));
        self.records
            .cooldowns
            .insert(settings.pool.clone(), Cooldown { until, reason });
    }

    /// A machine of `pool` booted and was heard from: the count of failures in a row starts
    /// again. A trip stays: only a reset ends it.
    pub(crate) fn succeeded(&mut self, pool: &str) {
        if let Some(brake) = self.records.brake.get_mut(pool) {
            brake.failures = 0;
            if brake.tripped_at.is_none() {
                self.records.brake.remove(pool);
            }
        }
    }

    /// Clears the brake of each pool an operator asked to reset: the count, the trip and the
    /// cooldown, so the pool launches at this step if it is live. A pool with none of them
    /// changes nothing, so a request that arrives twice is the same as one.
    pub(crate) fn reset_brakes(&mut self) {
        for pool in &self.input.brake_resets {
            self.records.brake.remove(pool);
            self.records.cooldowns.remove(pool);
        }
    }
}
