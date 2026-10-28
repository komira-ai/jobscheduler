//! Placement by load scoring hosts with the per-minute figures of profile admission, through
//! the scheduler's public inputs: a lease charged by a curve whose peak comes late is kept
//! off the host that would be over its capacity at that minute, though it fits now, and
//! what the score reads is what `load_at` and `charge_at` give.
//!
//! Two 100-core hosts that have measured, each holding one lease charged by a curve that no
//! reading covers yet (so it is charged whole): `a` holds 70 cores and 90 at minute 10, `b`
//! holds 60 cores throughout. The CPU limit is 110 cores; memory is the same on both.

#[path = "profile_rig/mod.rs"]
mod profile_rig;

use js_sched::{
    Charge, Event, H_PLUS, HORIZON, HostFacts, LeaseFacts, LoadConfig, PredictedLoad, SLOTS,
    Scheduler, ScoreConfig, Scorer,
};
use js_types::sched::{LeaseId, OperationId, Resources, WorkerId};

use profile_rig::load_curves::Curves;
use profile_rig::load_rig::{GIB, Rig, w, warm};
use profile_rig::{HOUR, flat_ws};

const CORES: u64 = 100_000;
/// Minutes of every curve in the rig.
const MINUTES: usize = 14;
/// The minute the held lease of `a` and the new lease peak at.
const PEAK: usize = 10;

fn node() -> Resources {
    Resources::new(CORES, 64 * GIB)
}

/// The working set of a held lease: half of a host's memory is left beside the two.
const HELD_WS: u64 = 28 * GIB;
/// The working set of the new lease.
const NEW_WS: u64 = 4 * GIB;

/// A curve of `quiet` millicores for `MINUTES` minutes except `peak` at minute `PEAK`, with
/// the working set `ws` throughout.
fn peaking(quiet: u32, peak: u32, ws: u64) -> Vec<(u32, u64)> {
    let mut cores = vec![quiet; MINUTES];
    cores[PEAK] = peak;
    flat_ws(&cores, ws)
}

/// Places one action of `key` on `worker` alone, by cordoning the other for the round.
fn hold(r: &mut Rig, worker: &str, other: &str, key: &str, p_bar: u64) {
    r.feed(Event::Cordon { worker: w(other) });
    r.keyed(key, 1_000, 8 * GIB, warm(p_bar, HELD_WS));
    let started = r.tick();
    assert_eq!(started.len(), 1, "{key} is placed");
    assert_eq!(started[0].worker, w(worker));
    r.feed(Event::Uncordon { worker: w(other) });
}

/// The two hosts, each holding its lease, with fresh readings. The key `k` has a curve of
/// 10 cores and 18 at the peak; the key `t` has none, so it is charged flat at 10 cores.
fn farm() -> Rig {
    let curves = Curves::new("load");
    curves.set("ha", &peaking(70_000, 90_000, HELD_WS), 5);
    curves.set("hb", &peaking(60_000, 60_000, HELD_WS), 5);
    curves.set("k", &peaking(10_000, 18_000, NEW_WS), 5);
    let mut r = Rig::with_curves(LoadConfig::default(), curves);
    for name in ["a", "b"] {
        r.up(name, node());
        r.set_clock(name, HOUR);
        r.baseline(name);
        r.window(name, 0, 0, &[]);
    }
    hold(&mut r, "a", "b", "ha", 70_000);
    hold(&mut r, "b", "a", "hb", 60_000);
    for name in ["a", "b"] {
        r.window(name, 0, 0, &[]);
    }
    r
}

/// What `worker` is charged at `slot` with the lease of `op` on it, as `(cpu, memory)`:
/// the left side and the new lease's charge from the scheduler's view.
fn with_lease(s: &Scheduler, op: OperationId, worker: &str, slot: usize) -> (u64, u64) {
    let (lc, lm) = s.load_at(&w(worker), slot).expect("registered");
    let (nc, nm) = s.charge_at(op, &w(worker), slot).expect("held");
    (lc + nc, lm + nm)
}

/// Submits an action of `key`, predicted at 10 cores, and returns it.
fn submit(r: &mut Rig, key: &str) -> OperationId {
    r.keyed(key, 1_000, GIB, warm(10_000, NEW_WS))
}

/// Catches: the score reading the new lease and the left side at minute 0 only: the curved
/// lease peaks at 18 cores at minute 10, where `a` is at 108% of its cores (under the 110%
/// limit, so the predicate admits it) though it is at 80% now and the better fit; `b` is at
/// 70%, 78% at the peak. The lease goes to `b`, and the score chose a host with no slack.
#[test]
fn a_late_peak_keeps_a_curved_lease_off_the_host_that_is_over_then() {
    let mut r = farm();
    let op = submit(&mut r, "k");
    for slot in [0, 5, 9, 11, HORIZON, H_PLUS] {
        assert_eq!(with_lease(&r.s, op, "a", slot).0, 80_000, "a, slot {slot}");
    }
    assert_eq!(with_lease(&r.s, op, "a", PEAK).0, 108_000, "a at the peak");
    for slot in [0, 5, H_PLUS] {
        assert_eq!(with_lease(&r.s, op, "b", slot).0, 70_000, "b, slot {slot}");
    }
    assert_eq!(with_lease(&r.s, op, "b", PEAK).0, 78_000, "b at the peak");
    let before = r.s.load_counters();
    let started = r.tick();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].worker, w("b"));
    let after = r.s.load_counters();
    assert_eq!(after.scored - before.scored, 1);
    assert_eq!(
        after.chose_slack, before.chose_slack,
        "the host chosen fits at every minute"
    );
}

/// Minutes of the curves whose peak comes after the horizon.
const LONG_MINUTES: usize = 45;
/// The minute those curves peak at: past `HORIZON`, so only the `H+` slot sees it.
const LATE: usize = 40;

/// A curve of `quiet` millicores for `LONG_MINUTES` minutes except `peak` at minute
/// `LATE`, with the working set `ws` throughout.
fn peaking_late(quiet: u32, peak: u32, ws: u64) -> Vec<(u32, u64)> {
    let mut cores = vec![quiet; LONG_MINUTES];
    cores[LATE] = peak;
    flat_ws(&cores, ws)
}

/// Catches: the `H+` slot left out of what the score reads: `a` holds a lease of 70 cores
/// that asks for 80 at minute 40, and the new lease asks for 10 cores and 25 at minute 40.
/// In the horizon `a` is the tighter fit (80% against `b`'s 70%), but from minute 30 on it
/// is at 105% (under the 110% limit, so admitted) where `b` is at 85%: the lease goes to
/// `b`, and the host chosen has no slack.
#[test]
fn a_peak_past_the_horizon_keeps_a_lease_off_the_host_that_is_over_then() {
    let curves = Curves::new("load");
    curves.set("la", &peaking_late(70_000, 80_000, HELD_WS), 5);
    curves.set("lb", &peaking_late(60_000, 60_000, HELD_WS), 5);
    curves.set("n", &peaking_late(10_000, 25_000, NEW_WS), 5);
    let mut r = Rig::with_curves(LoadConfig::default(), curves);
    for name in ["a", "b"] {
        r.up(name, node());
        r.set_clock(name, HOUR);
        r.baseline(name);
        r.window(name, 0, 0, &[]);
    }
    hold(&mut r, "a", "b", "la", 70_000);
    hold(&mut r, "b", "a", "lb", 60_000);
    for name in ["a", "b"] {
        r.window(name, 0, 0, &[]);
    }
    let op = submit(&mut r, "n");
    for slot in [0, PEAK, HORIZON] {
        assert_eq!(with_lease(&r.s, op, "a", slot).0, 80_000, "a, slot {slot}");
        assert_eq!(with_lease(&r.s, op, "b", slot).0, 70_000, "b, slot {slot}");
    }
    assert_eq!(
        with_lease(&r.s, op, "a", H_PLUS).0,
        105_000,
        "a from minute 30 on"
    );
    assert_eq!(
        with_lease(&r.s, op, "b", H_PLUS).0,
        85_000,
        "b from minute 30 on"
    );
    let started = r.tick();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].worker, w("b"));
    assert_eq!(
        r.s.load_counters().chose_slack,
        0,
        "the host chosen fits at H+"
    );
}

/// Catches: the same placement going wrong the other way, and the control for the test
/// above: the same two hosts and a lease of the same size now (10 cores) charged flat, which
/// brings `a` to exactly 100% at minute 10, not over. Nothing is over anywhere, the fit
/// decides, and `a`, the tighter fit now, takes it: the host the lease with the late peak
/// was kept off is the one a scorer that looked at minute 0 alone would choose.
#[test]
fn a_flat_twin_of_the_same_size_goes_to_the_tighter_host() {
    let mut r = farm();
    let op = submit(&mut r, "t");
    for slot in [0, PEAK, H_PLUS] {
        assert_eq!(
            with_lease(&r.s, op, "a", slot).0,
            if slot == PEAK { 100_000 } else { 80_000 }
        );
        assert_eq!(with_lease(&r.s, op, "b", slot).0, 70_000);
    }
    let started = r.tick();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].worker, w("a"));
    assert_eq!(r.s.load_counters().chose_slack, 0);
}

/// The scorer's model over the scheduler's public view: the figures `load_at` and
/// `charge_at` give for the operation, and the scalars of the rig's hosts.
struct Viewed<'a> {
    s: &'a Scheduler,
    op: OperationId,
}

impl PredictedLoad for Viewed<'_> {
    fn lease(&self) -> LeaseFacts {
        LeaseFacts {
            lease: LeaseId::new(1, 99),
            cold: false,
            retry: false,
        }
    }

    fn load(&self, worker: &WorkerId, slot: usize) -> Charge {
        let (cpu_millis, memory_bytes) = self.s.load_at(worker, slot).expect("registered");
        Charge {
            cpu_millis,
            memory_bytes,
        }
    }

    fn charge(&self, worker: &WorkerId, slot: usize) -> Charge {
        let (cpu_millis, memory_bytes) = self.s.charge_at(self.op, worker, slot).expect("held");
        Charge {
            cpu_millis,
            memory_bytes,
        }
    }

    fn host(&self, worker: &WorkerId) -> HostFacts {
        let capacity = self.s.capacity(worker).expect("registered");
        HostFacts {
            cpu_capacity_millis: capacity.cpu_millis,
            memory_capacity_bytes: capacity.memory_bytes,
            fit_minutes: 1,
            fallback: false,
            measured_above_predicted: false,
            same_key_millis: 0,
            new_key_millis: 10_000,
            uncovered_leases: 1,
        }
    }
}

/// Catches: the figures the score reads drifting from `load_at` and `charge_at` for a curved
/// lease: a scorer fed only those two functions, at all 32 slots including `H+`, ranks the
/// hosts as the scheduler's placement did, with the same slack (positive for `a`, whose
/// minute 10 is over, zero for `b`) and the same number of evaluations.
#[test]
fn the_score_reads_the_figures_the_view_gives() {
    let mut r = farm();
    let op = submit(&mut r, "k");
    let model = Viewed { s: &r.s, op };
    let (a, b) = (w("a"), w("b"));
    // The curved lease's own charge by slot: 10 cores, 18 at the peak, the last point from
    // the end of the curve on and at `H+`.
    for (slot, cores) in [
        (0, 10_000),
        (9, 10_000),
        (PEAK, 18_000),
        (11, 10_000),
        (H_PLUS, 10_000),
    ] {
        assert_eq!(model.charge(&a, slot).cpu_millis, cores, "slot {slot}");
        assert_eq!(model.charge(&b, slot).cpu_millis, cores, "slot {slot}");
    }
    assert_eq!(model.load(&a, PEAK).cpu_millis, 90_000);
    assert_eq!(model.load(&a, H_PLUS).cpu_millis, 70_000);
    assert_eq!(model.load(&b, PEAK).cpu_millis, 60_000);
    let mut scorer = Scorer::new(ScoreConfig::default());
    let (key_a, key_b) = (scorer.key(&model, &a), scorer.key(&model, &b));
    assert!(key_a.slack() > 0, "a is over at minute 10");
    assert_eq!(key_b.slack(), 0);
    let candidates = [a, b.clone()];
    let mut ranked = Scorer::new(ScoreConfig::default());
    assert_eq!(ranked.choose(&model, &candidates), Some(&b));
    assert_eq!(ranked.evaluations(), 2 * 2 * SLOTS as u64);

    let before = r.s.load_counters();
    let started = r.tick();
    assert_eq!(
        started[0].worker, b,
        "the scheduler chose as the view's figures do"
    );
    let spent = r.s.load_counters().score_evaluations - before.score_evaluations;
    assert_eq!(spent, ranked.evaluations());
}
