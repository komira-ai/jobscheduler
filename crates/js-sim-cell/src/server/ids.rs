//! What the cell's server mints its job and allocation ids from, and the clock its job
//! store stamps its rows with: both from the cell, so a seed replays.
//!
//! The job store of a service mints each allocation id (and each job id a client does not
//! give) from the system's random source, and stamps its rows with the system's clock; the
//! server orders by both. The cell gives it instead, through the in-process seam
//! (`Config::with_job_sources`), js-jobs's scripted source over [`SCRIPT_LEN`] ids drawn
//! from the seed ([`seeded`]), and the cell's own clock ([`StoreClock`]). The script
//! starts over after its last id, so ids repeat only after that many placements, more than
//! any scenario makes. The node keeps one source across a restart of the server, so a
//! restarted server goes on with the script rather than starting it again.

use std::time::Duration;

use js_jobs::ScriptedIds;
use js_state::StoreTime;
use js_types::{Clock as _, ManualClock};

/// How many ids the script holds before it starts over.
pub const SCRIPT_LEN: usize = 4_096;

/// The ids the server of a cell of `seed` mints, in the order it mints them: a function of
/// the seed alone.
#[must_use]
pub fn seeded(seed: u64) -> ScriptedIds {
    let mut state = seed;
    let mut next = || {
        let high = split_mix(&mut state);
        let low = split_mix(&mut state);
        let mut bits = [0; 16];
        bits[..8].copy_from_slice(&high.to_be_bytes());
        bits[8..].copy_from_slice(&low.to_be_bytes());
        bits
    };
    let first = next();
    let rest: Vec<[u8; 16]> = (1..SCRIPT_LEN).map(|_| next()).collect();
    ScriptedIds::new(first, rest)
}

/// One step of SplitMix64: a well-mixed 64 bits from a counter, with no random source.
fn split_mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The job store's clock: the cell's, as the server reads it, in milliseconds since the
/// Unix epoch, which is how the server compares the store's stamps with its own time.
#[derive(Clone, Debug)]
pub struct StoreClock(pub ManualClock);

impl js_state::Clock for StoreClock {
    fn now(&self) -> StoreTime {
        StoreTime::from_duration(Duration::from_millis(self.0.now().millis()))
    }
}

#[cfg(test)]
mod tests {
    use js_jobs::IdSource;
    use js_state::Clock as _;
    use js_types::Timestamp;

    use super::*;

    /// Catches: a script that does not follow its seed (two seeds alike, or one seed
    /// unlike itself), and one that repeats inside its length.
    #[test]
    fn a_script_is_a_function_of_its_seed_and_does_not_repeat_early() {
        let draw = |seed| {
            let source = seeded(seed);
            let ids = (0..=SCRIPT_LEN).map(|_| source.mint().unwrap());
            ids.collect::<Vec<[u8; 16]>>()
        };
        let (one, again, two) = (draw(1), draw(1), draw(2));
        assert_eq!(one, again);
        assert_ne!(one[..4], two[..4]);
        let distinct: std::collections::BTreeSet<[u8; 16]> =
            one[..SCRIPT_LEN].iter().copied().collect();
        assert_eq!(distinct.len(), SCRIPT_LEN);
        assert_eq!(one[SCRIPT_LEN], one[0], "the script starts over");
    }

    /// Catches: a store clock that reads another clock, or another unit, than the cell's
    /// milliseconds.
    #[test]
    fn the_store_clock_reads_the_cells_clock_in_milliseconds() {
        let clock = ManualClock::new(Timestamp::from_millis(1_500));
        let store = StoreClock(clock.clone());
        assert_eq!(
            store.now(),
            StoreTime::from_duration(Duration::from_millis(1_500))
        );
        clock.set(Timestamp::from_millis(2_750));
        assert_eq!(
            store.now(),
            StoreTime::from_duration(Duration::from_millis(2_750))
        );
    }
}
