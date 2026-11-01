//! The expected life of a lease, the feed of [`js_whole::cost::HostLoad::est_ends`].
//!
//! The feed reads the raw P90 of the wall minutes (`Fit.wall_minutes_p90` of the usage
//! store), not the life admission charges a lease to (`Profile.life`, which is that P90
//! plus `expected_life_delta_minutes`).

use js_types::LeaseId;
use js_usage::{ProfileCurves, ProfileKey};

use crate::runs::Runs;

/// Where the registry finds how long the runs of a profile usually take.
pub trait ProfileLives: Send + Sync {
    /// The P90 of the wall minutes the runs of `key` lasted, or `None` when it is not
    /// known.
    fn wall_minutes_p90(&self, key: &ProfileKey) -> Option<u32>;
}

impl ProfileLives for ProfileCurves {
    fn wall_minutes_p90(&self, key: &ProfileKey) -> Option<u32> {
        self.wall_minutes_p90_of(key)
    }
}

/// The estimated end (ms on the registry's clock) of a lease started at `started_at`
/// whose keyed runs carry `profiles`: the start plus the largest P90 among the keys that
/// have one. `None` while no key has one. The value is raw: the lease's max-wall bound is
/// applied by [`js_whole::cost::tail`].
pub(crate) fn estimate_end(
    started_at: u64,
    profiles: &[ProfileKey],
    lives: &dyn ProfileLives,
) -> Option<u64> {
    let longest = profiles
        .iter()
        .filter_map(|key| lives.wall_minutes_p90(key));
    let minutes = longest.max()?;
    let life_ms = u64::from(minutes).saturating_mul(60_000);
    Some(started_at.saturating_add(life_ms))
}

/// The `est_ends` of a host, one entry for each lease in `started` (a booked lease and
/// when it started, in the order of the host's end bounds): the estimate of the lease, or
/// `None` when no source of lives is set or no keyed run of the lease has a P90.
pub(crate) fn est_ends(
    lives: Option<&dyn ProfileLives>,
    runs: &Runs,
    started: &[(LeaseId, u64)],
) -> Vec<Option<u64>> {
    let estimate = |(lease, started_at): &(LeaseId, u64)| {
        estimate_end(*started_at, &runs.profiles_of(lease), lives?)
    };
    started.iter().map(estimate).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct Fixed(BTreeMap<&'static str, u32>);

    impl ProfileLives for Fixed {
        fn wall_minutes_p90(&self, key: &ProfileKey) -> Option<u32> {
            self.0.get(key.as_str()).copied()
        }
    }

    fn lives() -> Fixed {
        Fixed(BTreeMap::from([
            ("fast", 1),
            ("slow", 8),
            ("huge", 100_000),
        ]))
    }

    fn keys(names: &[&str]) -> Vec<ProfileKey> {
        names
            .iter()
            .map(|n| ProfileKey::parse(n).unwrap())
            .collect()
    }

    /// Catches: the start left out of the sum, or the minutes not converted to
    /// milliseconds.
    #[test]
    fn one_keyed_run_ends_its_p90_after_the_start() {
        assert_eq!(
            estimate_end(1_000, &keys(&["fast"]), &lives()),
            Some(1_000 + 60_000)
        );
    }

    /// Catches: the first, the last, the smallest or the sum taken in place of the
    /// largest P90.
    #[test]
    fn several_runs_take_the_largest_p90() {
        let expected = Some(5 + 8 * 60_000);
        assert_eq!(
            estimate_end(5, &keys(&["fast", "slow", "fast"]), &lives()),
            expected
        );
        assert_eq!(
            estimate_end(5, &keys(&["slow", "fast"]), &lives()),
            expected
        );
    }

    /// Catches: a lease with no keyed run given an estimate (zero minutes read as
    /// known).
    #[test]
    fn no_keyed_run_is_no_estimate() {
        assert_eq!(estimate_end(5, &[], &lives()), None);
    }

    /// Catches: a key with no P90 read as zero minutes, and an unknown key hiding the
    /// estimate that another key of the lease has.
    #[test]
    fn a_key_with_no_p90_gives_none_and_is_skipped_beside_a_known_one() {
        assert_eq!(estimate_end(5, &keys(&["unseen"]), &lives()), None);
        assert_eq!(
            estimate_end(5, &keys(&["unseen", "fast"]), &lives()),
            Some(5 + 60_000)
        );
    }

    /// Catches: the estimate clamped here (the tail already clamps to the bound) or
    /// overflowing on a huge P90.
    #[test]
    fn a_p90_beyond_any_bound_is_fed_raw_and_does_not_overflow() {
        assert_eq!(
            estimate_end(0, &keys(&["huge"]), &lives()),
            Some(100_000 * 60_000)
        );
        assert_eq!(
            estimate_end(u64::MAX, &keys(&["fast"]), &lives()),
            Some(u64::MAX)
        );
    }

    /// Catches: the curves of the usage store not answering through the trait.
    #[test]
    fn profile_curves_answer_through_the_trait() {
        let curves = ProfileCurves::new(Box::new(|_| None));
        let lives: &dyn ProfileLives = &curves;
        assert_eq!(lives.wall_minutes_p90(&keys(&["fast"])[0]), None);
    }
}
