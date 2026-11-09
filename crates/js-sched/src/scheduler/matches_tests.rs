//! The platform matches a scheduler holds between rounds: what it places is what a scheduler
//! that matches afresh each round places, over generated events, and each live worker is
//! matched once per request and kind until a worker's capabilities or the live workers
//! change, or the matches are dropped.

use js_caps::NodeCaps;
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseKind, Qos, Resources, StateMachine, WaiterId, WorkKey,
    WorkerId,
};

use crate::{DaemonInstance, Event, Input, Request, Scheduler};

const GIB: u64 = 1 << 30;

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// A node that runs containers, reporting `report`.
fn caps(report: &[(&str, &str)]) -> NodeCaps {
    NodeCaps::from_report(report.iter().copied())
        .unwrap()
        .with_drivers(["container"])
}

fn linux() -> NodeCaps {
    caps(&[("arch", "x86_64"), ("os", "linux")])
}

fn mac() -> NodeCaps {
    caps(&[("arch", "arm64"), ("os", "macos")])
}

/// Action `n` of `cores` cores and `gib` GiB, needing the platform `platform`.
fn action(n: u64, (cores, gib): (u64, u64), platform: &[(&str, &str)]) -> Request {
    Request {
        key: WorkKey::new("t", n.to_string()),
        qos: Qos::Batch,
        kind: LeaseKind::Action,
        resources: Resources::new(cores * 1_000, gib * GIB),
        needs: js_caps::Request::from_platform(platform.iter().copied()).unwrap(),
        hermetic: false,
        do_not_cache: true,
        whole_worker: false,
        pin: None,
    }
}

/// A one-core, 1 GiB Linux action numbered `n`.
fn on_linux(n: u64) -> Request {
    action(n, (1, 1), &[("OSFamily", "linux")])
}

/// A scheduler for term 1 on a clock the test sets.
struct Rig {
    s: Scheduler,
    now: FarmTime,
}

impl Rig {
    /// A scheduler that holds its matches if `hold` says so.
    fn new(hold: bool) -> Self {
        let mut s = Scheduler::new(1);
        if hold {
            s.hold_matches();
        }
        Self {
            s,
            now: FarmTime::default(),
        }
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(self.now, event))
    }

    /// Registers `name` with 8 cores, 16 GiB and `caps`.
    fn up(&mut self, name: &str, caps: NodeCaps) {
        self.up_with(name, 8, caps);
    }

    /// Registers `name` with `cores` cores, 16 GiB and `caps`.
    fn up_with(&mut self, name: &str, cores: u64, caps: NodeCaps) {
        let event = Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(cores * 1_000, 16 * GIB),
            caps,
        };
        assert!(self.feed(event).is_empty());
    }

    /// Submits `request` as waiter `n` and ticks: one placement round. The worker of each
    /// lease it granted.
    fn round(&mut self, n: u64, request: Request) -> Vec<WorkerId> {
        let submit = Event::Submit {
            waiter: WaiterId(n),
            request,
        };
        let mut effects = self.feed(submit);
        effects.extend(self.feed(Event::Tick));
        effects
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Commit(ControlRecord::Lease(grant)) => Some(grant.worker),
                _ => None,
            })
            .collect()
    }

    fn evaluations(&self) -> u64 {
        self.s.match_evaluations()
    }
}

/// Catches: matches held between rounds still made again in every round (each round of a pass
/// matching every live worker), or not counted. Four rounds over three workers, each placing
/// one Linux action: holding the matches, each worker is matched once, three in all; matching
/// afresh, twelve. The two place the same.
#[test]
fn held_matches_match_each_live_worker_once_over_the_rounds() {
    let mut runs = [true, false].map(|hold| {
        let mut rig = Rig::new(hold);
        for name in ["a", "b", "c"] {
            rig.up(name, linux());
        }
        let placed: Vec<Vec<WorkerId>> = (0..4).map(|n| rig.round(n, on_linux(n))).collect();
        (placed, rig.evaluations())
    });
    let [(held, held_count), (fresh, fresh_count)] = &mut runs;
    assert_eq!(held, fresh);
    assert!(held.iter().all(|grants| grants.len() == 1), "{held:?}");
    assert_eq!((*held_count, *fresh_count), (3, 12));
}

/// Catches: a held match kept after the worker's capabilities changed, so a Linux action is
/// placed on a worker that now reports macOS, or a capacity report that does not make the next
/// round match again. `a` turns into a Mac after the first round: every later Linux action goes
/// to `b`, and the round after the report matches both workers again.
#[test]
fn a_capacity_report_forgets_the_held_matches() {
    let mut rig = Rig::new(true);
    rig.up("a", linux());
    rig.up("b", linux());
    assert_eq!(rig.round(0, on_linux(0)).len(), 1);
    assert_eq!(rig.evaluations(), 2);
    let report = Event::Capacity {
        worker: w("a"),
        capacity: Resources::new(8_000, 16 * GIB),
        caps: mac(),
    };
    assert!(rig.feed(report).is_empty());
    for n in 1..4 {
        assert_eq!(rig.round(n, on_linux(n)), [w("b")]);
    }
    assert_eq!(rig.evaluations(), 4);
}

/// Catches: a held match kept over a registration that changed the worker's capabilities, or
/// one kept when a new worker joined, so the new worker is never offered work. `b` has one
/// core; `a` registers again as a Mac and `c` joins: the two-core Linux actions after can only
/// go to `c`, and do, matched again over the three live workers.
#[test]
fn a_registration_forgets_the_held_matches() {
    let mut rig = Rig::new(true);
    rig.up("a", linux());
    rig.up_with("b", 1, linux());
    let two_cores = |n| action(n, (2, 1), &[("OSFamily", "linux")]);
    assert_eq!(rig.round(0, two_cores(0)), [w("a")]);
    rig.up("a", mac());
    rig.up("c", linux());
    for n in 1..4 {
        assert_eq!(rig.round(n, two_cores(n)), [w("c")]);
    }
    assert_eq!(rig.evaluations(), 2 + 3);
}

/// Catches: held matches reused when the round has other live workers (their indices then
/// name other workers: a cordoned worker given work, or a worker left out). `b` has one core.
/// A two-core action goes to `a`, which is then cordoned: the next round matches the one live
/// worker, `b`, and fills it; after the uncordon, both are matched again and the next action
/// goes to `a`, the one with room.
#[test]
fn a_change_of_the_live_workers_matches_again() {
    let mut rig = Rig::new(true);
    rig.up("a", linux());
    rig.up_with("b", 1, linux());
    assert_eq!(
        rig.round(0, action(0, (2, 1), &[("OSFamily", "linux")])),
        [w("a")]
    );
    assert_eq!(rig.evaluations(), 2);
    assert!(rig.feed(Event::Cordon { worker: w("a") }).is_empty());
    assert_eq!(rig.round(1, on_linux(1)), [w("b")]);
    assert_eq!(rig.evaluations(), 3);
    assert!(rig.feed(Event::Uncordon { worker: w("a") }).is_empty());
    assert_eq!(
        rig.evaluations(),
        3,
        "an uncordon with nothing queued matches nothing"
    );
    assert_eq!(rig.round(2, on_linux(2)), [w("a")]);
    assert_eq!(rig.evaluations(), 5);
}

/// Catches: dropped matches still used by the next round, or a drop that leaves the scheduler
/// holding them. After the drop, every round matches afresh.
#[test]
fn dropped_matches_are_made_again_each_round() {
    let mut rig = Rig::new(true);
    rig.up("a", linux());
    rig.round(0, on_linux(0));
    rig.round(1, on_linux(1));
    assert_eq!(rig.evaluations(), 1);
    rig.s.drop_matches();
    rig.round(2, on_linux(2));
    rig.round(3, on_linux(3));
    assert_eq!(rig.evaluations(), 3);
}

/// Catches: held matches keyed by the platform alone, so a request of another lease kind with
/// the same platform reuses the workers matched for the first kind. `a` runs containers and
/// `v` runs VMs, both Linux: a Linux action goes to `a`, then a VM with the same platform goes
/// to `v`, the kind matched on its own over both workers.
#[test]
fn held_matches_are_kept_per_lease_kind() {
    let mut rig = Rig::new(true);
    rig.up("a", linux());
    let vm_node = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["vm"]);
    rig.up("v", vm_node);
    assert_eq!(rig.round(0, on_linux(0)), [w("a")]);
    let vm = Request {
        kind: LeaseKind::Vm,
        ..on_linux(1)
    };
    assert_eq!(rig.round(1, vm), [w("v")]);
    assert_eq!(rig.evaluations(), 2 + 2);
}

/// A generator of test events: xorshift64, seeded.
struct Gen(u64);

impl Gen {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// The platforms the generated actions ask for, and the nodes the generated workers report.
const PLATFORMS: [&[(&str, &str)]; 4] = [
    &[],
    &[("OSFamily", "linux")],
    &[("OSFamily", "Darwin")],
    &[("ISA", "x86-64")],
];

/// The event numbered `n` the generator picks: a worker of `names` registering or reporting
/// new capabilities, a cordon or an uncordon, an action submitted, a tick, or the clock moved
/// on (the caller's to do: `None`).
fn event(g: &mut Gen, n: u64) -> Option<Event> {
    const NAMES: [&str; 4] = ["a", "b", "c", "d"];
    let name = NAMES[usize::try_from(g.below(4)).unwrap()];
    let node = [linux(), mac()][usize::try_from(g.below(2)).unwrap()].clone();
    let event = match g.below(10) {
        0 => Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new((1 + g.below(8)) * 1_000, (1 + g.below(16)) * GIB),
            caps: node,
        },
        1 => Event::Capacity {
            worker: w(name),
            capacity: Resources::new((1 + g.below(8)) * 1_000, (1 + g.below(16)) * GIB),
            caps: node,
        },
        2 => Event::Cordon { worker: w(name) },
        3 => Event::Uncordon { worker: w(name) },
        4..=6 => {
            let size = (1 + g.below(4), 1 + g.below(8));
            let platform = PLATFORMS[usize::try_from(g.below(4)).unwrap()];
            Event::Submit {
                waiter: WaiterId(n),
                request: action(n, size, platform),
            }
        }
        7 | 8 => Event::Tick,
        _ => return None,
    };
    Some(event)
}

/// Catches: a held match that outlives what it was made from (a worker's capabilities, the
/// live workers: a registration, a capacity report, a cordon, an uncordon, a worker gone
/// silent) and changes a placement, a wait or a refusal. Over generated runs of workers that
/// register, report, are cordoned and uncordoned and fall silent as the clock moves, with
/// actions of several platforms and sizes submitted and placed, a scheduler that holds its
/// matches proposes exactly the effects of one that matches afresh each round, event by event.
/// Over all the runs, leases are granted, and the held scheduler matches less often.
#[test]
fn held_matches_place_as_fresh_matches_do_over_generated_runs() {
    let (mut grants, mut held_total, mut fresh_total) = (0, 0, 0);
    for seed in 1..=64u64 {
        let mut g = Gen(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut rigs = [Rig::new(true), Rig::new(false)];
        for n in 0..200 {
            let Some(event) = event(&mut g, n) else {
                // On by up to 20 s: past the lease grace, a worker not heard from falls
                // silent.
                let by = g.below(20_000);
                for rig in &mut rigs {
                    rig.now = FarmTime::from_millis(rig.now.as_millis() + by);
                }
                continue;
            };
            let [held, fresh] = &mut rigs;
            let effects = held.feed(event.clone());
            let granted =
                |effect: &Effect| matches!(effect, Effect::Commit(ControlRecord::Lease(_)));
            grants += effects.iter().filter(|effect| granted(effect)).count();
            assert_eq!(effects, fresh.feed(event), "seed {seed}, event {n}");
        }
        let [held, fresh] = &rigs;
        assert!(held.evaluations() <= fresh.evaluations(), "seed {seed}");
        held_total += held.evaluations();
        fresh_total += fresh.evaluations();
    }
    assert!(grants > 0);
    assert!(
        held_total < fresh_total,
        "{held_total} against {fresh_total}"
    );
}
