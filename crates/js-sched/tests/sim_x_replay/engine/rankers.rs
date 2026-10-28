//! The rule that picks a host among those the predicate passed.
//!
//! A [`Ranker`] is handed the lease and the hosts that passed, in name order, with the
//! whole state of the pool, so a scorer written elsewhere (an adapter over the placement
//! scorer) plugs in by implementing the trait and edits nothing here. Two rankers live
//! here:
//!
//! * [`LeastLoaded`] - the previous rule, through the shipped [`js_sched::Load`] and
//!   [`js_sched::least_loaded`] (the larger of the CPU, memory and lease shares of what the
//!   predicate charges the host, then fewer leases; the first of equals in name order).
//! * [`Random`] - a uniformly random feasible host, drawn from a stream of its own for
//!   each lease, so adding a lease, a ranker or a retry shifts no other lease's draws.

use std::collections::BTreeMap;

use js_sched::{Load, least_loaded};
use js_sim::SimRng;
use js_types::sched::{Resources, WorkerId};

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

/// The load of a host of `capacity` (`cpu_millis`, `mem_mib`, lease cap) that is charged
/// `cpu_millis` and `mem_mib` and holds `leases` leases, as the shipped ranking
/// ([`js_sched::Load`]) orders hosts: the largest of its CPU, memory and lease shares,
/// then how many leases it holds. Memory goes in as bytes; the scale is the same on both
/// sides of a share, so it changes no order.
pub fn load_of(cpu_millis: u64, mem_mib: u64, capacity: (u64, u64, u64), leases: usize) -> Load {
    let mib = |m: u64| m.saturating_mul(1 << 20);
    let resources = |cpu_millis: u64, mem_mib: u64, leases: Option<u64>| Resources {
        cpu_millis,
        memory_bytes: mib(mem_mib),
        leases,
        ..Resources::default()
    };
    Load::new(
        resources(cpu_millis, mem_mib, None),
        resources(capacity.0, capacity.1, Some(capacity.2)),
        leases,
    )
}

/// The host of the least load among `loads` (host index, load); the first of equals. It is
/// [`js_sched::least_loaded`] over names that stand for the host indexes.
pub fn least_loaded_standin(loads: &[(usize, Load)]) -> Option<usize> {
    let names: Vec<WorkerId> = loads
        .iter()
        .map(|(host, _)| WorkerId::new(host.to_string()))
        .collect();
    let admitted: Vec<(&WorkerId, Load)> = names
        .iter()
        .zip(loads)
        .map(|(name, (_, load))| (name, *load))
        .collect();
    let chosen = least_loaded(&admitted)?;
    let at = names.iter().position(|n| std::ptr::eq(n, chosen))?;
    Some(loads[at].0)
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
                    load_of(
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
/// [`stream_seed`]. Each time the ranker is asked about the lease the stream gives one
/// draw per host of the pool, in host order, and the candidate with the smallest draw is
/// chosen, so the choice depends on which hosts passed and not on the order they were
/// handed in. The engine places a lease as soon as it has a candidate, so it asks once
/// per lease; the stream is dropped when the lease is placed.
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
