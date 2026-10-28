//! The rig of the tests of profile admission: a worker `a` that has measured, the curves it
//! is given, and helpers that place a lease or read what the worker is charged ahead.

#![allow(dead_code, reason = "each test binary uses some of the rig")]

#[path = "../load_rig/mod.rs"]
pub mod load_rig;

use std::sync::Arc;

use js_sched::LoadConfig;
use js_types::sched::{Resources, StartLease};

use load_rig::{Curves, GIB, Rig, w, warm};

/// One hour on the agent's clock: leases can be given an age before the baseline.
pub const HOUR: u64 = 3_600_000_000;

/// A 100-core worker with 64 GiB: the CPU limit is 110 cores.
pub fn node() -> Resources {
    Resources::new(100_000, 64 * GIB)
}

/// A worker `a` that has measured (a baseline and a window), and the curves it is given.
pub fn cluster(config: LoadConfig) -> (Rig, Arc<Curves>) {
    cluster_on(config, node())
}

/// As [`cluster`], with a worker of `capacity`.
pub fn cluster_on(config: LoadConfig, capacity: Resources) -> (Rig, Arc<Curves>) {
    let curves = Curves::new("load");
    let mut r = Rig::with_curves(config, &curves);
    r.up("a", capacity);
    r.set_clock("a", HOUR);
    r.baseline("a");
    r.window("a", 0, 0, &[]);
    (r, curves)
}

/// The cores and memory `a` is charged `minutes` minutes ahead.
pub fn ahead(r: &Rig, minutes: u32) -> (u64, u64) {
    r.s.charged_ahead(&w("a"), minutes).expect("registered")
}

/// A curve of `cores` millicores per minute with the working set `ws` at each.
pub fn flat_ws(cores: &[u32], ws: u64) -> Vec<(u32, u64)> {
    cores.iter().map(|&c| (c, ws)).collect()
}

/// Places one action of `key` predicted at `hint` on the worker; its start.
pub fn place(r: &mut Rig, key: &str, hint: js_sched::Prediction) -> StartLease {
    r.keyed(key, 1_000, 8 * GIB, hint);
    let started = r.tick();
    assert_eq!(started.len(), 1, "{key} is placed");
    started.into_iter().next().expect("one start")
}

/// A worker holding one lease `first`, covered at age 0, whose curve is quiet (10 cores)
/// except for 90 cores at `busy`; it is predicted at 20 cores (a floor of 15).
pub fn holding_busy_at(busy: usize, minutes: usize) -> (Rig, Arc<Curves>) {
    let (mut r, curves) = cluster(LoadConfig::default());
    curves.set("first", &busy_at(busy, minutes), 5);
    let first = place(&mut r, "first", warm(20_000, GIB));
    r.window_clocked("a", 5_000, GIB, &[first.lease], &[(first.lease, 0)], &[]);
    (r, curves)
}

/// Quiet (10 cores) for `minutes` minutes except 90 cores at minute `busy`.
pub fn busy_at(busy: usize, minutes: usize) -> Vec<(u32, u64)> {
    let mut cores = vec![10_000; minutes];
    cores[busy] = 90_000;
    flat_ws(&cores, GIB)
}

/// The cores and memory of the first lease are charged by the curve below; the second
/// is predicted at `p_bar`.
pub fn busy_pair(config: LoadConfig, p_bar: u64) -> (Rig, usize) {
    let (mut r, curves) = cluster(config);
    let busy = |at: usize| {
        let mut cores = vec![10_000; 20];
        cores[at] = 60_000;
        flat_ws(&cores, GIB)
    };
    curves.set("early", &busy(5), 5);
    curves.set("late", &busy(12), 5);
    // Not covered yet: the first lease carries its charge whole.
    r.keyed("early", 1_000, 8 * GIB, warm(p_bar, GIB));
    r.keyed("late", 1_000, 8 * GIB, warm(p_bar, GIB));
    let placed = r.tick().len();
    (r, placed)
}
