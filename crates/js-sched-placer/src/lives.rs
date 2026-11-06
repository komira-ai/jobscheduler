//! The expected lives of the registry's leases, as the learned curves the engine's
//! scheduler reads ([`Lives`]).
//!
//! The registry knows how long a lease is expected to last: the largest P90 of the wall
//! minutes among the profiles of its keyed runs, from its source of lives
//! ([`js_registry::Registry::set_lives`]). It hands them over at each pass of the
//! whole-node queue ([`js_registry::WholeRound::lives`]), and the engine keeps them here
//! under the work key of the operation that stands for each lease. The scheduler reads
//! them through [`CurveSource::wall_minutes_p90`] when it costs a host to drain (the
//! lease's start plus that many minutes, cut to its bound), so the engine's drain order
//! reads the same tails as the registry's own pass. They give no curve: every lease is
//! still charged what it books.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use js_types::sched::WorkKey;
use js_types::sched::usage::{Curve, CurveSource};

/// The P90 of the wall minutes of each lease the registry knows one for, by the work key of
/// the engine's operation for the lease. Shared between the engine, which sets them, and
/// its scheduler, which reads them; its lock is a leaf.
#[derive(Debug, Default)]
pub struct Lives {
    minutes: Mutex<BTreeMap<WorkKey, u32>>,
}

impl Lives {
    /// Replaces the lives with `minutes`: a key left out has none from now on.
    pub fn set(&self, minutes: BTreeMap<WorkKey, u32>) {
        *self.lock() = minutes;
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<WorkKey, u32>> {
        self.minutes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CurveSource for Lives {
    /// No curve: a lease is charged what it books.
    fn curve(&self, _key: &WorkKey) -> Option<Curve> {
        None
    }

    fn wall_minutes_p90(&self, key: &WorkKey) -> Option<u32> {
        self.lock().get(key).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a life kept after the registry stopped giving it (the drain cost would read
    /// a lease that has no keyed run any more as ending early), or a curve made up from a
    /// life (the lease would be charged by it).
    #[test]
    fn the_lives_are_replaced_whole_and_give_no_curve() {
        let lives = Lives::default();
        let a = WorkKey::new("held", "1-1");
        let b = WorkKey::new("held", "1-2");
        lives.set(BTreeMap::from([(a.clone(), 3), (b.clone(), 9)]));
        assert_eq!(lives.wall_minutes_p90(&a), Some(3));
        assert_eq!(lives.curve(&a), None);
        lives.set(BTreeMap::from([(b.clone(), 4)]));
        assert_eq!(lives.wall_minutes_p90(&a), None);
        assert_eq!(lives.wall_minutes_p90(&b), Some(4));
    }
}
