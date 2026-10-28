//! The rule that picks a host among those the predicate passed.
//!
//! A [`Ranker`] is handed the lease and the hosts that passed, in name order, with the
//! whole state of the pool, so a scorer written elsewhere (an adapter over the placement
//! scorer) plugs in by implementing the trait and edits nothing here. Two rankers live
//! here:
//!
//! * [`LeastLoaded`] - the previous rule, a stand-in for the pure function the scorer's
//!   branch will export. It is pinned to the flat rule's `Load` (the larger of the CPU,
//!   memory and lease shares of what the predicate charges the host, then fewer leases)
//!   and takes the first of equals in name order, as shipped.
//! * [`Random`] - a uniformly random feasible host, drawn from a stream of its own for
//!   each lease, so adding a lease, a ranker or a retry shifts no other lease's draws.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use js_sim::SimRng;

use super::metrics::Decided;
use super::model::{Belief, HostState};
use super::predicate::{Params, left};

/// What a ranker is shown.
#[derive(Clone, Copy)]
pub struct Choose<'a, 'w> {
    /// The lease index.
    pub lease: usize,
    /// The tick of the round.
    pub tick: u64,
    /// What the scheduler believes of the lease.
    pub belief: &'a Belief,
    /// Every host, by index.
    pub hosts: &'a [HostState<'w>],
    /// The hosts the predicate passed, by index, ascending. Never empty.
    pub candidates: &'a [usize],
    /// The predicate's settings.
    pub params: &'a Params,
}

/// A ranker's choice and what it cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pick {
    /// The host chosen: one of the candidates.
    pub host: usize,
    /// How the choice was decided.
    pub decided: Decided,
    /// Candidates scored (0 unless a scorer ran on two or more).
    pub scored_candidates: u64,
    /// Evaluations the scorer made.
    pub evaluations: u64,
}

impl Pick {
    /// A choice no scoring went into.
    pub fn unscored(host: usize) -> Self {
        Self {
            host,
            decided: Decided::Unscored,
            scored_candidates: 0,
            evaluations: 0,
        }
    }
}

/// A rule that picks one host among the candidates.
pub trait Ranker {
    /// The host for the lease: one of `c.candidates`.
    fn choose(&mut self, c: &Choose<'_, '_>) -> Pick;

    /// The lease was placed (on the host `choose` returned); a ranker that keeps state
    /// per lease drops it.
    fn placed(&mut self, _lease: usize) {}
}

/// `booked / capacity`, compared exactly: a share of a host that has none of a resource is
/// 0 whatever is booked.
#[derive(Clone, Copy, Debug)]
struct Share {
    booked: u64,
    capacity: u64,
}

impl Share {
    fn new(booked: u64, capacity: u64) -> Self {
        if capacity == 0 {
            Self {
                booked: 0,
                capacity: 1,
            }
        } else {
            Self { booked, capacity }
        }
    }
}

impl Ord for Share {
    fn cmp(&self, other: &Self) -> Ordering {
        (u128::from(self.booked) * u128::from(other.capacity))
            .cmp(&(u128::from(other.booked) * u128::from(self.capacity)))
    }
}

impl PartialOrd for Share {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Share {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Share {}

/// A host's load as the previous rule orders hosts: the largest of its CPU, memory and
/// lease shares, then how many leases it holds. The smaller is the less loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Load {
    share: Share,
    leases: usize,
}

impl Load {
    /// The load of a host of `capacity` (`cpu_millis`, `mem_mib`, lease cap) that is
    /// charged `cpu_millis` and `mem_mib` and holds `leases` leases.
    pub fn new(cpu_millis: u64, mem_mib: u64, capacity: (u64, u64, u64), leases: usize) -> Self {
        let cpu = Share::new(cpu_millis, capacity.0);
        let mem = Share::new(mem_mib, capacity.1);
        let held = u64::try_from(leases).unwrap_or(u64::MAX);
        let lease_share = Share::new(held, capacity.2);
        Self {
            share: cpu.max(mem).max(lease_share),
            leases,
        }
    }
}

/// The host of the least load among `loads` (host index, load); the first of equals.
/// A stand-in for the pure function the scorer's branch will export; the tests pin it to
/// the flat rule's load cases.
pub fn least_loaded_standin(loads: &[(usize, Load)]) -> Option<usize> {
    loads
        .iter()
        .min_by_key(|(_, load)| *load)
        .map(|(host, _)| *host)
}

/// The previous rule: the least loaded candidate.
#[derive(Clone, Copy, Debug, Default)]
pub struct LeastLoaded;

impl Ranker for LeastLoaded {
    fn choose(&mut self, c: &Choose<'_, '_>) -> Pick {
        let loads: Vec<(usize, Load)> = c
            .candidates
            .iter()
            .map(|&h| {
                let host = &c.hosts[h];
                let l = left(c.params, host);
                let cap = &host.cap;
                (
                    h,
                    Load::new(
                        l.cpu_millis,
                        l.mem_mib,
                        (cap.cpu_millis, cap.mem_mib, u64::from(cap.leases)),
                        host.residents().len(),
                    ),
                )
            })
            .collect();
        let host = least_loaded_standin(&loads).expect("a candidate");
        Pick::unscored(host)
    }
}

/// splitmix64 of `x`: a bijection of `u64`, the only mixing the random ranker needs.
pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The seed of the stream of `lease` under `seed`.
pub fn stream_seed(seed: u64, lease: usize) -> u64 {
    splitmix64(seed ^ splitmix64(lease as u64 ^ 0x5241_4e44_4f4d_0001))
}

/// A random feasible host. Each lease has a stream of its own, `SimRng::from_seed` of
/// [`stream_seed`]. Every time the lease is looked at the stream gives one draw per
/// host of the pool, in host order, and the candidate with the smallest draw is
/// chosen, so the choice depends on which hosts passed and not on the order they were
/// handed in.
#[derive(Debug)]
pub struct Random {
    seed: u64,
    streams: BTreeMap<usize, SimRng>,
}

impl Random {
    /// A random ranker under `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            streams: BTreeMap::new(),
        }
    }
}

impl Ranker for Random {
    fn choose(&mut self, c: &Choose<'_, '_>) -> Pick {
        let seed = self.seed;
        let stream = self
            .streams
            .entry(c.lease)
            .or_insert_with(|| SimRng::from_seed(stream_seed(seed, c.lease)));
        let draws: Vec<u64> = (0..c.hosts.len()).map(|_| stream.next_u64()).collect();
        let host = c
            .candidates
            .iter()
            .copied()
            .min_by_key(|&h| (draws[h], h))
            .expect("a candidate");
        Pick::unscored(host)
    }

    fn placed(&mut self, lease: usize) {
        self.streams.remove(&lease);
    }
}
