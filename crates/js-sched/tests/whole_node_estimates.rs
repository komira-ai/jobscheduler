//! The estimate feed of the drain cost: a lease whose key has a P90 of wall minutes is
//! costed to end that long after it started, cut to its max-wall bound, so the worker held
//! for a whole-machine request is the one that empties first by estimate. Every time is
//! farm time fed to the scheduler.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use std::sync::Arc;

use crate::whole_rig::{Farm, request, secs, w};
use js_sched::Scheduler;
use js_types::sched::usage::{Curve, CurveSource};
use js_types::sched::{FarmTime, LeaseKind, OperationId, WorkKey};
use js_whole::WholeNodeConfig;

/// P90 wall minutes for the keys of the leases the rig starts: `(n, minutes)` is the key
/// of the `n`th action the rig submits.
struct Lives(Vec<(WorkKey, u32)>);

impl Lives {
    fn of(minutes: &[(u8, u32)]) -> Arc<Self> {
        let keys = minutes
            .iter()
            .map(|&(n, m)| (request(n, LeaseKind::Action).key, m));
        Arc::new(Self(keys.collect()))
    }
}

impl CurveSource for Lives {
    fn curve(&self, _key: &WorkKey) -> Option<Curve> {
        None
    }

    fn wall_minutes_p90(&self, key: &WorkKey) -> Option<u32> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, m)| *m)
    }
}

fn farm(lives: Option<Arc<Lives>>) -> Farm {
    let whole = WholeNodeConfig {
        max_draining: 4,
        ..WholeNodeConfig::dedicated(4)
    };
    let mut s = Scheduler::new(1)
        .with_load_config(booked::booked())
        .with_whole_node(whole);
    if let Some(lives) = lives {
        s = s.with_curves(lives);
    }
    Farm::new(s)
}

/// Catches: no estimate fed: `a`'s lease may run 600 s but its key takes a minute at the
/// P90, so `a` empties before `b` (300 s) and is held; by bounds alone `b` is held.
#[test]
fn a_short_estimate_makes_the_worker_cheaper_than_a_shorter_bound() {
    let mut f = farm(Some(Lives::of(&[(1, 1)])));
    f.worker("a");
    f.lease_on("a", secs(600));
    f.worker("b");
    f.lease_on("b", secs(300));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: the order changed for scheduler with no estimate for the key: the worker
/// with the shorter bound (`b`) is held.
#[test]
fn a_key_with_no_p90_is_costed_at_its_bound() {
    let mut f = farm(Some(Lives::of(&[(3, 1)])));
    f.worker("a");
    f.lease_on("a", secs(600));
    f.worker("b");
    f.lease_on("b", secs(300));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: the estimate counted from the time the order is given instead of from the
/// start of the lease: both leases start at 0, and at 120 s `a` (3 minutes) ends at 180 s,
/// before `b` (250 s); counted from 120 s it would end at 300 s.
#[test]
fn the_estimate_counts_from_the_start_of_the_lease() {
    let mut f = farm(Some(Lives::of(&[(1, 3)])));
    f.worker("a");
    f.lease_on("a", secs(900));
    f.worker("b");
    f.lease_on("b", secs(250));
    f.at(120_000);
    f.beat();
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: an estimate past the bound taking the lease past its max wall: `a`'s key takes
/// 600 minutes but the lease may run 300 s, so `a` ends at 300 s, before `b` (400 s).
#[test]
fn an_estimate_beyond_the_bound_is_cut_to_the_bound() {
    let mut f = farm(Some(Lives::of(&[(1, 600)])));
    f.worker("a");
    f.lease_on("a", secs(300));
    f.worker("b");
    f.lease_on("b", secs(400));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: the scheduler built with no curves reading estimates from somewhere: the
/// bounds alone decide and `b` is held.
#[test]
fn with_no_curves_the_bounds_decide() {
    let mut f = farm(None);
    f.worker("a");
    f.lease_on("a", secs(600));
    f.worker("b");
    f.lease_on("b", secs(300));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// `a` holds a lease of 60 s, past its bound at 100 s; `b` then takes a lease of
/// `b_wall` seconds, and a whole-machine request arrives: the farm and the request.
fn late_a_against_b(b_wall: u64) -> (Farm, OperationId) {
    let mut f = farm(None);
    f.worker("a");
    f.lease_on("a", secs(60));
    f.worker("b");
    assert!(f.advance(100_000).is_empty());
    f.lease_on("b", secs(b_wall));
    let whole = f.whole();
    (f, whole)
}

/// Catches: a lease past its bound read as ending now, so its worker looks empty. At 100 s
/// `a`'s lease of 60 s still runs; `b`'s ends at 130 s. Read as a minute from now (160 s),
/// `a` is the dearer and `b` is held; read as now, `a` would be held.
#[test]
fn a_lease_past_its_bound_does_not_make_its_worker_the_cheapest() {
    let (f, whole) = late_a_against_b(30);
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: the late lease read as ending other than exactly a minute from now. Against a
/// lease of `b` that ends 59 s from now `b` is held; against one that ends 60 s from now the
/// two cost the same and the lower name `a` is held, with the tail it was chosen on at
/// 160 s (a minute past 100 s), not the passed bound of 60 s.
#[test]
fn a_lease_past_its_bound_reads_as_ending_a_minute_from_now() {
    let (f, whole) = late_a_against_b(59);
    assert_eq!(f.held_for(whole), Some(&w("b")));
    let (f, whole) = late_a_against_b(60);
    assert_eq!(f.held_for(whole), Some(&w("a")));
    let mark = f.s.whole_mark(whole).expect("the request marks a");
    assert_eq!(mark.tail_end, Some(FarmTime::from_millis(160_000)));
}

/// Catches: only the bound checked for lateness. `a`'s lease may run 900 s but its key
/// takes a minute at the P90, so by 120 s it has run past its estimate: it reads as ending
/// at 180 s, after `b`'s lease (150 s), and `b` is held; read as its passed estimate, `a`
/// would cost nothing and be held.
#[test]
fn a_lease_past_its_estimate_reads_as_ending_a_minute_from_now() {
    let mut f = farm(Some(Lives::of(&[(1, 1)])));
    f.worker("a");
    f.lease_on("a", secs(900));
    f.worker("b");
    assert!(f.advance(120_000).is_empty());
    f.lease_on("b", secs(30));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: a lease with no bound left at its passed estimate. `a`'s lease has no max
/// wall and its key takes a minute at the P90; at 120 s it reads as ending at 180 s, not at
/// 60 s, and `b` (150 s) is held.
#[test]
fn a_lease_with_no_bound_past_its_estimate_reads_as_ending_a_minute_from_now() {
    let mut f = farm(Some(Lives::of(&[(1, 1)])));
    f.worker("a");
    f.place_on("a", None);
    f.worker("b");
    assert!(f.advance(120_000).is_empty());
    f.lease_on("b", secs(30));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}
