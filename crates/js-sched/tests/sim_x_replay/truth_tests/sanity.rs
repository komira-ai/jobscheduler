//! A check of the model against the data it is built from: the truth model keeps each
//! run's recorded mean cores, so how those means vary within a key is a property of the
//! fixture. The band below is the one a learner's `k` rests on.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};

use crate::engine::fixture::{Fixture, KeyIx, Outcome};
use crate::engine::truth::{TruthConfig, build};

/// Runs a key's learner remembers.
const RING: usize = 32;

/// Nearest-rank P90 of `v` (sorted ascending in place).
fn p90(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v[(v.len() * 9).div_ceil(10) - 1]
}

#[test]
fn warm_leases_run_below_their_keys_p90_by_the_stated_band() {
    // The ground-truth model is only a fair test of a learner if a run's mean cores,
    // against the P90 of the key's earlier runs (the 32 latest that ended before this one
    // started, three or more of them), is distributed within a stated band: median ratio
    // within [0.55, 0.90] and the share of runs above the P90 within [5%, 25%]. A truth
    // that scrambled cpu against wall, or a fixture whose keys do not repeat, leaves it.
    let fx = Fixture::embedded().expect("the committed fixture loads");
    let runs = build(&fx.rows, &TruthConfig::new(1));
    let mut ending = BinaryHeap::new();
    let mut rings: BTreeMap<KeyIx, VecDeque<u64>> = BTreeMap::new();
    let mut ratios = Vec::new();
    let mut above = 0usize;
    for (i, run) in runs.iter().enumerate() {
        while let Some(&Reverse((end, j))) = ending.peek() {
            if end >= run.start_ms {
                break;
            }
            ending.pop();
            let done: &crate::engine::truth::Run = &runs[j];
            if done.outcome == Outcome::Ok {
                let ring = rings.entry(done.key).or_default();
                ring.push_back(done.mean_millicores());
                if ring.len() > RING {
                    ring.pop_front();
                }
            }
        }
        if let Some(ring) = rings.get(&run.key).filter(|r| r.len() >= 3) {
            let mut sorted: Vec<u64> = ring.iter().copied().collect();
            let belief = p90(&mut sorted);
            if let Some(ratio) = (run.mean_millicores() * 1_000_000).checked_div(belief) {
                ratios.push(ratio);
                above += usize::from(run.mean_millicores() > belief);
            }
        }
        ending.push(Reverse((run.start_ms + run.wall_ms, i)));
    }
    assert!(ratios.len() > 1_000, "only {} warm leases", ratios.len());
    ratios.sort_unstable();
    let median = ratios[ratios.len() / 2];
    assert!(
        (550_000..=900_000).contains(&median),
        "median ratio {median} ppm"
    );
    assert!(
        20 * above >= ratios.len() && 4 * above <= ratios.len(),
        "{above} of {} warm leases above their key's P90",
        ratios.len()
    );
}
