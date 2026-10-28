//! Sim aa: whole-node exclusion comes before the predicate and before the score. A worker
//! marked for a drain, or held for a whole-machine request, takes no small action even
//! when it is the best fit by a wide margin, and the score is never evaluated for it. The
//! predicate's demand bounds (sims a and h) hold when the order of the workers and the
//! hash of every lease against every worker change.
//!
//! Exercises: the exclusion of marked workers before the predicate and before scoring.

#[path = "load_rig/mod.rs"]
mod load_rig;
#[path = "load_world/mod.rs"]
mod load_world;

use js_sched::{EVALUATIONS_PER_WORKER, Event, LoadConfig, PlacementLoad, Prediction};
use js_types::sched::{FarmTime, LeaseKind, NodePhase, Resources, StartLease};

use load_rig::{GIB, Rig, caps, w, warm};
use load_world::{Spec, World};

fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

fn on(starts: &[StartLease]) -> Vec<&str> {
    starts.iter().map(|s| s.worker.as_str()).collect()
}

/// How a worker is marked.
#[derive(Clone, Copy, Debug)]
enum Mark {
    /// A drain with a deadline far off.
    Drain,
    /// Held for a whole-machine request queued ahead of the small actions.
    Reserved,
    /// Not serving: it reports memory pressure, so it takes no new lease and keeps its own.
    Pressure,
    /// Not serving: its cleanup failed and it is quarantined.
    Quarantine,
}

fn phase(mark: Mark) -> NodePhase {
    match mark {
        Mark::Pressure => NodePhase::MemoryPressure,
        Mark::Quarantine => NodePhase::Quarantined("cleanup failed".to_owned()),
        Mark::Drain | Mark::Reserved => NodePhase::Serving,
    }
}

/// Three 8-core, 32 GiB workers. `a` holds two one-core leases predicted at 14 GiB each:
/// it has the fewest leases (so a whole-machine request holds it) and its memory is
/// nearly full, so a small action of a core and 2 GiB leaves almost no memory on it and
/// is the tightest fit there (a leftover of 2 GiB), where `b` and `c` leave 30 GiB. Its
/// future load is the lowest, since two leases are all it holds. `b` and `c` hold
/// `others` one-core leases each.
struct Fixture {
    r: Rig,
    a_leases: Vec<StartLease>,
    b_leases: Vec<StartLease>,
    head: Option<js_types::sched::OperationId>,
    submitted_at: u64,
}

/// Three one-core leases: room for more.
const FEW: (u64, u64) = (3, 1_000);
/// Four leases of 2.1 cores: 8.4 cores, and a half core more is over the 8.8 cap.
const FULL: (u64, u64) = (4, 2_100);
const SMALL_CPU: u64 = 500;
const SMALL_MEMORY: u64 = 2 * GIB;

fn fixture(mark: Mark, others: (u64, u64)) -> Fixture {
    let whole = caps("container").with_drivers(["container", "native-whole-machine"]);
    let mut r = Rig::on();
    for name in ["a", "b", "c"] {
        r.up_with(name, node(), whole.clone());
        r.baseline(name);
    }
    let place = |r: &mut Rig, worker: &str, out: &[&str], (count, cpu): (u64, u64), memory: u64| {
        for other in out {
            r.feed(Event::Cordon { worker: w(other) });
        }
        for _ in 0..count {
            r.action(cpu, GIB, Some(warm(cpu, memory)));
        }
        let starts = r.tick();
        assert_eq!(on(&starts), vec![worker; count as usize]);
        for other in out {
            r.feed(Event::Uncordon { worker: w(other) });
        }
        starts
    };
    let a_leases = place(&mut r, "a", &["b", "c"], (2, 1_000), 14 * GIB);
    let b_leases = place(&mut r, "b", &["a", "c"], others, 0);
    place(&mut r, "c", &["a", "b"], others, 0);
    let mut head = None;
    match mark {
        Mark::Drain => r.feed(Event::Drain {
            worker: w("a"),
            deadline: FarmTime::from_millis(3_600_000),
        }),
        Mark::Reserved => {
            let mut q = r.request(LeaseKind::WholeMachine, 8_000, 32 * GIB);
            q.resources = node();
            head = Some(r.submit(q));
            // The first round after it holds the worker.
            assert!(r.round(1_000).is_empty());
            assert_eq!(r.s.reservation(head.unwrap()), Some(&w("a")));
            Vec::new()
        }
        Mark::Pressure | Mark::Quarantine => {
            r.feed(Event::Phase {
                worker: w("a"),
                phase: phase(mark),
            });
            Vec::new()
        }
    };
    let submitted_at = r.ms / 1_000;
    Fixture {
        r,
        a_leases,
        b_leases,
        head,
        submitted_at,
    }
}

impl Fixture {
    fn small(&mut self, count: u64) {
        for _ in 0..count {
            self.r
                .action(SMALL_CPU, GIB, Some(warm(SMALL_CPU, SMALL_MEMORY)));
        }
    }
}

/// Shows (draining worker): worker `a`, with the lowest future load and the tightest fit,
/// is marked for a drain while a stream of small actions arrives, two a round for five
/// rounds. No action goes to it, and the score is evaluated for `b` and `c` alone: 64
/// evaluations for each of them for each action, none for `a`. Without the mark the first
/// action goes to `a` (checked on a copy of the setup), so the exclusion is what keeps it
/// off.
#[test]
fn a_draining_worker_takes_no_small_action_and_is_not_scored() {
    // The stream without the mark goes to `a`.
    let mut open = fixture(Mark::Drain, FEW);
    open.r.feed(Event::Uncordon { worker: w("a") });
    open.small(1);
    assert_eq!(on(&open.r.round(1_000)), ["a"]);

    let mut f = fixture(Mark::Drain, FEW);
    let before = f.r.s.load_counters().score_evaluations;
    let mut placed = 0;
    for _ in 0..5 {
        f.small(2);
        let starts = f.r.round(1_000);
        assert_eq!(starts.len(), 2);
        assert!(
            starts.iter().all(|s| s.worker.as_str() != "a"),
            "{:?}",
            on(&starts)
        );
        placed += starts.len() as u64;
    }
    let evaluations = f.r.s.load_counters().score_evaluations - before;
    assert_eq!(evaluations, placed * 2 * EVALUATIONS_PER_WORKER, "b and c");
    assert_eq!(f.r.held("a").len(), 2, "a holds what it held");
}

/// Shows (worker not serving): `a`, the tightest fit with the lowest future load, reports
/// memory pressure, and in a second run is quarantined. It is no cordon and no reservation:
/// only its phase is not `Serving`. No small action goes to it, the score is evaluated for
/// `b` and `c` alone, and once `a` reports `Serving` again the first action goes to it
/// (so the phase is what keeps it off).
#[test]
fn a_worker_that_is_not_serving_takes_no_small_action_and_is_not_scored() {
    for mark in [Mark::Pressure, Mark::Quarantine] {
        let mut open = fixture(mark, FEW);
        open.r.feed(Event::Phase {
            worker: w("a"),
            phase: NodePhase::Serving,
        });
        open.small(1);
        assert_eq!(on(&open.r.round(1_000)), ["a"], "{mark:?}: serving again");

        let mut f = fixture(mark, FEW);
        let before = f.r.s.load_counters().score_evaluations;
        let mut placed = 0;
        for _ in 0..5 {
            f.small(2);
            let starts = f.r.round(1_000);
            assert_eq!(starts.len(), 2, "{mark:?}");
            assert!(
                starts.iter().all(|s| s.worker.as_str() != "a"),
                "{mark:?}: {:?}",
                on(&starts)
            );
            placed += starts.len() as u64;
        }
        let evaluations = f.r.s.load_counters().score_evaluations - before;
        assert_eq!(
            evaluations,
            placed * 2 * EVALUATIONS_PER_WORKER,
            "{mark:?}: b and c"
        );
        assert_eq!(f.r.held("a").len(), 2, "{mark:?}: a holds what it held");
    }
}

/// Shows (mixed pool): an action that books a small worker's whole memory owns that worker
/// (`s`, 8 GiB) but only a quarter of each large one (`l1`, `l2`, 32 GiB), and all three
/// pass the predicate. The score ranks the two workers it would share: the action goes to
/// one of them, 64 evaluations for each of the two and none for `s`. A request of a large
/// worker's whole memory is exclusive on all three, is not scored, and goes by the least
/// loaded rule.
#[test]
fn an_action_exclusive_on_one_worker_is_still_scored_over_the_others() {
    let mut r = Rig::on();
    r.up("s", Resources::new(8_000, 8 * GIB));
    r.up("l1", Resources::new(8_000, 32 * GIB));
    r.up("l2", Resources::new(8_000, 32 * GIB));
    let before = r.s.load_counters().score_evaluations;
    r.action(500, 8 * GIB, Some(warm(500, GIB)));
    let starts = r.round(1_000);
    assert_eq!(starts.len(), 1);
    assert_ne!(starts[0].worker.as_str(), "s", "{:?}", on(&starts));
    assert_eq!(
        r.s.load_counters().score_evaluations - before,
        2 * EVALUATIONS_PER_WORKER,
        "l1 and l2 alone"
    );

    let before = r.s.load_counters().score_evaluations;
    r.action(500, 32 * GIB, Some(warm(500, GIB)));
    assert_eq!(r.round(1_000).len(), 1);
    assert_eq!(r.s.load_counters().score_evaluations, before, "not scored");
}

/// Shows (worker held for a whole-machine request): `a`, the tightest fit with the lowest
/// future load, is held for a whole-machine request queued ahead of the stream. Two small
/// actions arrive each round; none goes to `a`, the score is evaluated for `b` and `c`
/// alone, and the whole-machine request starts on `a` in the first round after the last
/// lease on `a` ends, which is within the reservation bound (the longest remaining lease
/// at reservation plus one round).
#[test]
fn a_worker_held_for_a_whole_machine_request_takes_no_small_action_and_is_not_scored() {
    let mut f = fixture(Mark::Reserved, FEW);
    let head = f.head.unwrap();
    // The longest lease on `a` ends four ticks after the reservation.
    let ends_at = f.submitted_at + 4;
    let before = f.r.s.load_counters().score_evaluations;
    let mut placed = 0;
    let mut started_at = None;
    let mut small_on_a = 0;
    for _ in 0..8 {
        f.small(2);
        if f.r.ms / 1_000 + 1 == ends_at {
            for lease in f.a_leases.clone() {
                f.r.finish(&lease);
            }
        }
        let starts = f.r.round(1_000);
        for s in &starts {
            if s.operation == head {
                assert_eq!(s.worker.as_str(), "a");
                started_at = Some(f.r.ms / 1_000);
            } else {
                placed += 1;
                small_on_a += u64::from(s.worker.as_str() == "a");
            }
        }
        if started_at.is_some() {
            break;
        }
    }
    assert_eq!(small_on_a, 0, "no small action on the held worker");
    let started_at = started_at.expect("the whole-machine request starts");
    assert!(
        started_at <= ends_at + 1,
        "started at {started_at}, the last lease ended at {ends_at}"
    );
    assert!(started_at >= ends_at, "not before `a` was empty");
    // Every small action was scored over `b` and `c`; the round that started the
    // whole-machine request scored its smalls over `b` and `c` as well.
    let evaluations = f.r.s.load_counters().score_evaluations - before;
    assert_eq!(evaluations, placed * 2 * EVALUATIONS_PER_WORKER);
    assert!(placed >= 8, "the stream went on meanwhile: {placed}");
}

/// Shows: if every other worker is infeasible the small action waits. `b` and `c` each
/// hold 8.4 cores in four leases (full for another half core), `a` is marked and would
/// take it; for a drain and for a held worker the action is not started, nothing is evaluated, and it is
/// placed on `b` as soon as a lease there ends (one worker left: nothing scored).
#[test]
fn when_every_other_worker_is_infeasible_the_action_waits() {
    for mark in [Mark::Drain, Mark::Reserved] {
        let mut f = fixture(mark, FULL);
        let before = f.r.s.load_counters().score_evaluations;
        f.small(1);
        for _ in 0..3 {
            assert!(f.r.round(1_000).is_empty(), "{mark:?}: waits");
        }
        assert_eq!(f.r.s.load_counters().score_evaluations, before, "{mark:?}");
        assert_eq!(f.r.held("a").len(), 2, "{mark:?}: nothing went to a");
        let freed = f.b_leases[0].clone();
        f.r.finish(&freed);
        let starts = f.r.round(1_000);
        assert_eq!(on(&starts), ["b"], "{mark:?}");
        assert_eq!(f.r.s.load_counters().score_evaluations, before, "{mark:?}");
    }
}

// ---- the demand bound with the order and the hashes changed ----

/// A small deterministic generator (xorshift64*), so a seed replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }
}

/// `count` worker names drawn from a pool of twelve by `seed`, in a shuffled order: the
/// order the workers register in, and (the hash taking the name) the scores of equal
/// workers, both differ by seed.
fn names(seed: u64, count: usize) -> Vec<String> {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut pool: Vec<String> = (0..12).map(|i| format!("host-{i:02}")).collect();
    for i in (1..pool.len()).rev() {
        pool.swap(i, rng.below(i + 1));
    }
    pool.truncate(count);
    pool
}

fn light_warm(cpu_millis: u64, memory_bytes: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes,
        cold: false,
        life_minutes: 1,
    }
}

fn drain(w: &mut World, max: u64) {
    for _ in 0..max {
        w.step();
        if w.stats.values().all(|s| s.finished == s.submitted) {
            return;
        }
    }
    panic!("not drained in {max} ticks: {:?}", w.stats);
}

/// The sim a bound for an 88-thread node in millicores: `1.47 C`.
const BOUND_88: u64 = 129_067;

/// The `h` bound for a 16-thread node: `(r k / f) (1 + s) C + n_c A` in millicores.
fn cpu_bound(rk_over_f: (u64, u64), cores: u64, n_c: u64, a_cores: u64) -> u64 {
    let (num, den) = rk_over_f;
    (num * 1_100 * cores * 1_000).div_ceil(den * 1_000) + n_c * a_cores * 1_000
}

fn populations() -> [Spec; 3] {
    [
        Spec::flat("wide", 1_500, 60, light_warm(2_000, GIB)),
        Spec::flat("single", 1_000, 200, light_warm(1_000, GIB)),
        Spec::flat("light", 300, 8, light_warm(500, GIB / 4)),
    ]
}

fn three_nodes(config: LoadConfig, seed: u64) -> (World, Vec<String>) {
    let order = names(seed, 3);
    let mut w = World::new(config);
    for name in &order {
        w.linux(name, 88, 512);
    }
    (w, order)
}

/// Sim a over three 88-thread workers: the mixed workload of sim a, three times over,
/// with the workers registered in a different order and named differently for each of
/// twelve seeds. The demand on every worker stays within `1.47 C`, as on one worker. The
/// same bound is violated by the booked rule on the wide population (the model can fail).
#[test]
fn the_demand_bound_of_sim_a_holds_for_any_order_and_hash() {
    for seed in 1..=12 {
        let (mut world, order) = three_nodes(LoadConfig::default(), seed);
        let [wide, single, light] = populations();
        for _ in 0..4 {
            world.submit_many(90, &wide);
            world.submit_many(45, &single);
            world.submit_many(300, &light);
        }
        drain(&mut world, 3_000);
        let busy = order
            .iter()
            .filter(|n| world.nodes[*n].peak_demand > 0)
            .count();
        assert!(busy >= 2, "seed {seed}: the load used {busy} workers");
        for name in &order {
            let peak = world.nodes[name].peak_demand;
            assert!(peak <= BOUND_88, "seed {seed}: {name} at {peak}");
        }
    }
    let (mut booked, order) = three_nodes(
        LoadConfig {
            mode: PlacementLoad::Off,
            ..LoadConfig::default()
        },
        1,
    );
    booked.submit_many(360, &populations()[0]);
    booked.run(3);
    let worst = order
        .iter()
        .map(|n| booked.nodes[n].peak_demand)
        .max()
        .unwrap();
    assert!(worst > BOUND_88, "the booked rule reached {worst}");
}

/// Sim h (idle prelude) over two 16-thread workers, for twelve seeds of order and names:
/// 120 sleepers (a core predicted, idle 40 ticks, then four cores) are admitted batch
/// after batch while the earlier ones idle, at most 24 on a worker, the covered charge
/// of each worker never below `f` of its covered predictions, and the demand on each
/// worker stays within `(r k / f) (1 + s) C + n_c A`.
#[test]
fn the_idle_prelude_bound_of_sim_h_holds_for_any_order_and_hash() {
    for seed in 1..=12 {
        let order = names(seed, 2);
        let mut world = World::new(LoadConfig::default());
        for name in &order {
            world.linux(name, 16, 64);
        }
        let sleeper = Spec {
            demand: vec![(40, 0), (60, 4_000)],
            mem: vec![(100, 0)],
            ..Spec::flat("sleeper", 0, 0, light_warm(1_000, 0))
        };
        world.submit_many(120, &sleeper);
        let mut floor_held = true;
        for _ in 0..39 {
            world.step();
            for name in &order {
                let view = world
                    .s
                    .load_view(&js_types::sched::WorkerId::new(name))
                    .unwrap();
                if !view.fallback {
                    let covered = view.predicted_cpu_millis - view.uncovered_cpu_millis;
                    floor_held &= view.left_cpu_millis >= covered * 3 / 4;
                }
            }
        }
        assert!(
            floor_held,
            "seed {seed}: the covered charge fell below the floor"
        );
        let admitted = world.stats["sleeper"].started;
        world.run(30);
        assert_eq!(world.stats["sleeper"].started, admitted, "seed {seed}");
        assert!(admitted <= 48, "seed {seed}: {admitted}");
        let bound = cpu_bound((4, 1), 16, 2, 4);
        for name in &order {
            let peak = world.nodes[name].peak_demand;
            assert!(peak <= bound, "seed {seed}: {name} at {peak}");
        }
    }
}

/// Sim h (rise) over two 16-thread workers, for twelve seeds: ten leases predicted at two
/// cores wake to four after 20 ticks; more are queued while they idle and after. No worker
/// is asked for more than `(r k / f) (1 + s) C + n_c A` of demand.
#[test]
fn the_rise_bound_of_sim_h_holds_for_any_order_and_hash() {
    for seed in 1..=12 {
        let order = names(seed, 2);
        let mut world = World::new(LoadConfig::default());
        for name in &order {
            world.linux(name, 16, 64);
        }
        let phase = Spec {
            demand: vec![(20, 0), (100, 4_000)],
            mem: vec![(120, 0)],
            ..Spec::flat("phase", 0, 0, light_warm(2_000, 0))
        };
        world.submit_many(10, &phase);
        world.run(21);
        let late = Spec::flat("late", 2_000, 300, light_warm(2_000, 0));
        world.submit_many(20, &late);
        world.run(60);
        let bound = cpu_bound((2, 1), 16, 2, 4);
        for name in &order {
            let peak = world.nodes[name].peak_demand;
            assert!(peak <= bound, "seed {seed}: {name} at {peak}");
        }
    }
}
