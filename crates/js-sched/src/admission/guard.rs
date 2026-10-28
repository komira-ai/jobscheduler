//! The shape guard: a key whose curve keeps being wrong is charged flat for a while.
//!
//! Per key and per resource (CPU, memory) the guard keeps whether each of the last ten
//! runs charged by curve misfit. A resource's curve is demoted to the flat charge for the
//! next [`DEMOTED_RUNS`] runs when 2 of the last 3 runs, or 3 of the last 10, misfit. The
//! second trigger catches a misfit every third run, which 2 of 3 never does; misfits five
//! or more runs apart trip neither and are left to the memory ladder.
//!
//! This is pure state in the scheduler, fed by what the scheduler already sees: the
//! per-minute records of the heartbeats (a lease-minute that misfit marks its run) and
//! the way a run ends. It is not in `js-sizing`: the curve is fitted elsewhere, and the
//! guard decides only whether the scheduler believes it.

use std::collections::BTreeMap;

/// How many runs a demoted resource is charged flat.
pub(crate) const DEMOTED_RUNS: u32 = 8;

/// The most keys the guard remembers; past it the half touched longest ago are
/// forgotten (a forgotten key starts with a clean history).
pub(crate) const MAX_KEYS: usize = 1 << 16;

/// One resource of one key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Track {
    /// Bit `i` is set when the run `i` runs ago misfit; ten bits.
    recent: u16,
    /// Runs left that are charged flat.
    demoted_left: u32,
}

impl Track {
    /// Records a run charged by curve; whether it demoted the resource. A run that ends
    /// while the resource is demoted (it was charged by curve before) says nothing.
    fn record(&mut self, misfit: bool) -> bool {
        if self.demoted_left > 0 {
            return false;
        }
        self.recent = ((self.recent << 1) | u16::from(misfit)) & 0x03ff;
        let last_three = (self.recent & 0b111).count_ones();
        if last_three >= 2 || self.recent.count_ones() >= 3 {
            self.recent = 0;
            self.demoted_left = DEMOTED_RUNS;
            return true;
        }
        false
    }

    fn flat_run(&mut self) {
        self.demoted_left = self.demoted_left.saturating_sub(1);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Keyed {
    cpu: Track,
    mem: Track,
    touched: u64,
}

/// Which resource of a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resource {
    Cpu,
    Memory,
}

/// The shape guard of a scheduler, by key hash.
#[derive(Clone, Debug, Default)]
pub(crate) struct ShapeGuard {
    keys: BTreeMap<u64, Keyed>,
    clock: u64,
}

impl ShapeGuard {
    /// The runs left that `key`'s CPU and memory are charged flat.
    pub(crate) fn runs_left(&self, key: u64) -> (u32, u32) {
        self.keys
            .get(&key)
            .map_or((0, 0), |k| (k.cpu.demoted_left, k.mem.demoted_left))
    }

    fn entry(&mut self, key: u64) -> &mut Keyed {
        self.clock += 1;
        if self.keys.len() >= MAX_KEYS && !self.keys.contains_key(&key) {
            let mut seen: Vec<u64> = self.keys.values().map(|k| k.touched).collect();
            seen.sort_unstable();
            let cutoff = seen[seen.len() / 2];
            self.keys.retain(|_, k| k.touched > cutoff);
        }
        let entry = self.keys.entry(key).or_default();
        entry.touched = self.clock;
        entry
    }

    /// A run of `key` charged by curve in `what` ended: it misfit or not. Returns whether
    /// that demoted the resource.
    pub(crate) fn record(&mut self, key: u64, what: Resource, misfit: bool) -> bool {
        let k = self.entry(key);
        match what {
            Resource::Cpu => k.cpu.record(misfit),
            Resource::Memory => k.mem.record(misfit),
        }
    }

    /// A run of `key` charged flat because `what` was demoted ended: one run less to go.
    pub(crate) fn flat_run(&mut self, key: u64, what: Resource) {
        let k = self.entry(key);
        match what {
            Resource::Cpu => k.cpu.flat_run(),
            Resource::Memory => k.mem.flat_run(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(g: &mut ShapeGuard, what: Resource, runs: &[bool]) -> Vec<bool> {
        runs.iter().map(|&m| g.record(1, what, m)).collect()
    }

    /// Catches: the trigger of 2 of the last 3 wrong at either edge: two misfits in a row
    /// and two with one run between demote; two with two runs between do not.
    #[test]
    fn two_of_the_last_three_demote() {
        let mut g = ShapeGuard::default();
        assert_eq!(
            feed(&mut g, Resource::Cpu, &[true, true]),
            [false, true],
            "runs n and n+1"
        );
        assert_eq!(g.runs_left(1), (DEMOTED_RUNS, 0));
        let mut g = ShapeGuard::default();
        assert_eq!(
            feed(&mut g, Resource::Cpu, &[true, false, true]),
            [false, false, true]
        );
        let mut g = ShapeGuard::default();
        assert_eq!(
            feed(&mut g, Resource::Cpu, &[true, false, false, true]),
            [false; 4],
            "two runs between: not 2 of 3"
        );
    }

    /// Catches: the second trigger missing (a misfit every third run never demotes), or
    /// firing on two misfits in ten, or counting more than the last ten.
    #[test]
    fn three_of_the_last_ten_demote_and_five_apart_never_do() {
        let mut g = ShapeGuard::default();
        let every_third = [true, false, false, true, false, false, true];
        assert_eq!(
            feed(&mut g, Resource::Memory, &every_third),
            [false, false, false, false, false, false, true],
            "the third misfit within ten runs trips it"
        );
        assert_eq!(g.runs_left(1), (0, DEMOTED_RUNS));
        let mut g = ShapeGuard::default();
        let fifth: Vec<bool> = (0..30).map(|i| i % 5 == 0).collect();
        assert!(
            feed(&mut g, Resource::Memory, &fifth).iter().all(|d| !d),
            "misfits five apart are never demoted"
        );
        // The window is ten runs: misfits at runs 1, 5 and 11 are two in the last ten at
        // the third, at 1, 5 and 10 three.
        let hits = |at: &[usize], runs: usize| -> Vec<bool> {
            (1..=runs).map(|i| at.contains(&i)).collect()
        };
        let mut g = ShapeGuard::default();
        let late = feed(&mut g, Resource::Cpu, &hits(&[1, 5, 11], 11));
        assert!(late.iter().all(|d| !d), "the first misfit left the window");
        let mut g = ShapeGuard::default();
        let inside = feed(&mut g, Resource::Cpu, &hits(&[1, 5, 10], 10));
        assert_eq!(inside.last(), Some(&true), "still inside the window");
    }

    /// Catches: a demotion that does not last eight flat runs, does not clear the
    /// history (a second demotion right after), counts runs charged by curve while
    /// demoted, or one resource demoting the other.
    #[test]
    fn a_demotion_lasts_eight_flat_runs_then_starts_clean() {
        let mut g = ShapeGuard::default();
        feed(&mut g, Resource::Cpu, &[true, true]);
        assert_eq!(g.runs_left(1), (8, 0), "memory is its own track");
        assert!(
            !g.record(1, Resource::Cpu, true),
            "a late run charged by curve says nothing"
        );
        for left in (0..8).rev() {
            g.flat_run(1, Resource::Cpu);
            assert_eq!(g.runs_left(1).0, left);
        }
        g.flat_run(1, Resource::Cpu);
        assert_eq!(g.runs_left(1).0, 0, "saturates");
        assert!(
            !g.record(1, Resource::Cpu, true),
            "clean history: one misfit is not enough"
        );
        g.flat_run(99, Resource::Memory);
        assert_eq!(
            g.runs_left(99),
            (0, 0),
            "a key never seen has nothing to count off"
        );
    }

    /// Catches: the key table growing without bound, and forgetting the keys touched
    /// most recently instead of the ones touched longest ago.
    #[test]
    fn the_table_forgets_the_keys_touched_longest_ago() {
        let mut g = ShapeGuard::default();
        for key in 0..MAX_KEYS as u64 {
            g.record(key, Resource::Cpu, true);
        }
        assert_eq!(g.keys.len(), MAX_KEYS);
        g.record(5, Resource::Cpu, false);
        assert_eq!(g.keys.len(), MAX_KEYS, "a key already known makes no room");
        g.record(u64::MAX, Resource::Cpu, true);
        assert!(g.keys.len() <= MAX_KEYS / 2 + 1, "{}", g.keys.len());
        assert!(g.keys.contains_key(&u64::MAX));
        assert!(
            g.keys.contains_key(&(MAX_KEYS as u64 - 1)),
            "the newest stays"
        );
        assert!(!g.keys.contains_key(&0), "the oldest goes");
    }
}
