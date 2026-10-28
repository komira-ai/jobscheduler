//! The score that chooses among the workers the load predicate admits, through the
//! scheduler's public inputs. Each test names the behaviour it shows.

#[path = "load_rig/mod.rs"]
mod load_rig;

use js_sched::{EVALUATIONS_PER_WORKER, Event, Load, LoadConfig, least_loaded};
use js_types::sched::{LeaseKind, Resources, StartLease, WorkKey};

use load_rig::{GIB, Rig, w, warm};

fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

fn on(starts: &[StartLease]) -> Vec<&str> {
    starts.iter().map(|s| s.worker.as_str()).collect()
}

/// An action of `cpu` millicores predicted the same, of the key `class`. It is not
/// joinable, so two of one key run side by side as a fan-out does.
fn keyed(r: &mut Rig, class: &str, cpu: u64) {
    let mut request = r.request(LeaseKind::Action, cpu, 4 * GIB);
    request.key = WorkKey::new(class, "x");
    request.do_not_cache = true;
    let op = r.submit(request);
    r.s.set_cost(op, warm(cpu, 0));
}

/// Loads `worker` with `count` one-core actions by cordoning the others for the round.
fn load(r: &mut Rig, worker: &str, others: &[&str], count: u64) {
    for other in others {
        r.feed(Event::Cordon { worker: w(other) });
    }
    for _ in 0..count {
        r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    }
    assert_eq!(on(&r.tick()), vec![worker; count as usize]);
    for other in others {
        r.feed(Event::Uncordon { worker: w(other) });
    }
}

/// Shows: a lease goes to the worker it fits most tightly, not the one least loaded now
/// (fill before open): `a` holds four cores, `b` none, and the next lease goes to `a`.
#[test]
fn a_lease_goes_to_the_fuller_worker_that_fits() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    load(&mut r, "a", &["b"], 4);
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    assert_eq!(on(&r.tick()), ["a"]);
}

/// Shows: the score reads only the workers the predicate passed. `a` is full (refused by
/// the predicate) so the lease goes to `b` however well it would fit `a`, and the score
/// was evaluated for `b` alone, which is one worker left: nothing is scored.
#[test]
fn a_worker_the_predicate_refuses_is_not_scored() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    load(&mut r, "a", &["b"], 8);
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    assert_eq!(on(&r.tick()), ["b"]);
    assert_eq!(r.s.load_counters().score_evaluations, 0);
}

/// Shows: evaluations per round are exactly the feasible workers times 64 per request,
/// and zero for a cordoned worker (excluded before the predicate), for a worker the
/// predicate refused, and for a request with one worker left.
#[test]
fn evaluations_are_the_feasible_workers_times_64_per_request() {
    assert_eq!(EVALUATIONS_PER_WORKER, 64);
    let mut r = Rig::on();
    for name in ["a", "b", "c", "d"] {
        r.up(name, node());
    }
    // `d` is full for a one-core lease; `c` is cordoned.
    load(&mut r, "d", &["a", "b", "c"], 8);
    r.feed(Event::Cordon { worker: w("c") });
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    let first = r.tick();
    assert_eq!(first.len(), 1);
    let c = r.s.load_counters();
    assert_eq!(c.score_evaluations, 2 * 64, "a and b only");
    assert_eq!(c.scored, 1);
    // Three requests in one round: the placed lease changes the figures, but the
    // count is still two scored workers per request.
    for _ in 0..3 {
        r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    }
    assert_eq!(r.tick().len(), 3);
    assert_eq!(r.s.load_counters().score_evaluations, (2 + 3 * 2) * 64);
}

/// Shows: with one worker left nothing is scored (no evaluations), and the lease is
/// still placed there.
#[test]
fn one_worker_left_is_taken_without_scoring() {
    let mut r = Rig::on();
    r.up("a", node());
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    assert_eq!(on(&r.tick()), ["a"]);
    assert_eq!(r.s.load_counters().score_evaluations, 0);
    assert_eq!(r.s.load_counters().scored, 0);
}

/// Shows: a lease that books the whole host's memory (the last rung of the ladder) and a
/// whole-machine lease bypass scoring: no evaluations, placed by the previous rule (the
/// less loaded worker).
#[test]
fn whole_node_leases_bypass_scoring() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    load(&mut r, "a", &["b"], 2);
    let evaluations = r.s.load_counters().score_evaluations;
    let op = r.action(1_000, 32 * GIB, Some(warm(1_000, 0)));
    let starts = r.tick();
    assert_eq!(
        on(&starts),
        ["b"],
        "the less loaded worker, operation {op:?}"
    );
    let mut q = r.request(LeaseKind::WholeMachine, 8_000, 32 * GIB);
    q.resources = node();
    r.submit(q);
    let _ = r.tick();
    assert_eq!(r.s.load_counters().score_evaluations, evaluations);
}

/// Shows: key 4. `a` holds a lease of the key `fan` and the new lease has the same key:
/// together they are over a quarter of `a`'s cores, so the lease goes to the empty `b`
/// although it fits `a` better; with `--placement-key-share=100` it goes to `a`.
#[test]
fn a_key_is_kept_to_its_share_of_a_worker() {
    for (share, want) in [("25", "b"), ("100", "a")] {
        let mut cfg = LoadConfig::default();
        cfg.apply_flag("--placement-key-share", share).unwrap();
        let mut r = Rig::new(cfg);
        r.up("a", node());
        r.up("b", node());
        r.feed(Event::Cordon { worker: w("b") });
        keyed(&mut r, "fan", 1_000);
        assert_eq!(on(&r.tick()), ["a"]);
        r.feed(Event::Uncordon { worker: w("b") });
        keyed(&mut r, "fan", 1_500);
        assert_eq!(on(&r.tick()), [want], "share {share}");
        // Neither choice was a concentrated worker.
        assert_eq!(r.s.load_counters().chose_concentrated, 0);
    }
}

/// Shows: key 3. A worker with no usable reading (fallback) and a fresh worker with the
/// same predicted load: the lease goes to the fresh one, which best fit alone would not
/// choose (the fallback worker looks fuller).
#[test]
fn a_fallback_worker_loses_to_a_fresh_one() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    load(&mut r, "a", &["b"], 2);
    load(&mut r, "b", &["a"], 2);
    // Two windows of readings for `b` only, covering its leases at a quarter of a core
    // each: `b` is charged by measurement (its cpu is lower), `a` by prediction.
    let covered = r.held("b");
    r.window("b", 500, GIB, &covered);
    r.window("b", 500, GIB, &covered);
    let (a, b) = (
        r.s.load_view(&w("a")).unwrap(),
        r.s.load_view(&w("b")).unwrap(),
    );
    assert!(a.fallback && !b.fallback);
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    let starts = r.tick();
    assert_eq!(on(&starts), ["b"]);
}

/// Shows: key 3, `M > P_c`. Two fresh workers with equal predicted load; on `a` the
/// measured cores are above the predictions of its covered leases, on `b` they are not:
/// the lease goes to `b`.
#[test]
fn a_worker_measured_above_its_predictions_loses() {
    let mut r = Rig::on();
    r.up("a", node());
    r.up("b", node());
    load(&mut r, "a", &["b"], 2);
    load(&mut r, "b", &["a"], 2);
    let (held_a, held_b) = (r.held("a"), r.held("b"));
    for _ in 0..2 {
        r.window("a", 2_500, GIB, &held_a);
        r.window("b", 2_000, GIB, &held_b);
    }
    assert!(!r.s.load_view(&w("a")).unwrap().fallback);
    assert!(!r.s.load_view(&w("b")).unwrap().fallback);
    r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
    assert_eq!(on(&r.tick()), ["b"]);
    assert_eq!(r.s.load_counters().chose_distrusted, 0);
}

/// Shows: the public `least_loaded` ranks workers of different sizes by the larger of
/// their CPU and memory shares of what is held, then by lease count, takes the first of
/// equals, and gives nothing for no candidate.
#[test]
fn the_public_least_loaded_rule_ranks_by_share_then_leases() {
    let small = Resources::new(4_000, 16 * GIB);
    let large = Resources::new(16_000, 64 * GIB);
    let (a, b, c) = (w("a"), w("b"), w("c"));
    // `a` holds 2 of 4 cores, `b` 4 of 16, `c` 4 of 16 in more leases.
    let held = Resources::new(2_000, 0);
    let held_large = Resources::new(4_000, 0);
    let ranked = [
        (&a, Load::new(held, small, 2)),
        (&b, Load::new(held_large, large, 4)),
        (&c, Load::new(held_large, large, 5)),
    ];
    assert_eq!(least_loaded(&ranked), Some(&b), "the smaller share");
    assert_eq!(least_loaded(&ranked[2..]), Some(&c));
    let tied = [
        (&a, Load::new(held_large, large, 4)),
        (&b, Load::new(held_large, large, 4)),
    ];
    assert_eq!(least_loaded(&tied), Some(&a), "the first of equals");
    let memory = [
        (&a, Load::new(Resources::new(0, 8 * GIB), small, 1)),
        (&b, Load::new(Resources::new(3_000, 0), small, 1)),
    ];
    assert_eq!(least_loaded(&memory), Some(&a), "memory 0.5 below CPU 0.75");
    assert_eq!(least_loaded(&[]), None);
}
