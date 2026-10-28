//! The caps on whole-machine holds ([`Scheduler::with_whole_node`]): how many workers may be
//! held whole or drained at once, how many are kept for ordinary work, the last worker of
//! an older ordinary request, and the ceiling that falls when workers leave the pool. A
//! request a cap stops holds no worker and says why ([`Scheduler::whole_wait`]). Every time
//! is farm time fed to the scheduler; nothing waits.

#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[path = "load_rig/mod.rs"]
mod load_rig;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, LoadConfig, Request, Scheduler, WholeMark, WholeWait};
use js_types::sched::{LeaseGrant, LeaseKind, NodePhase, OperationId, Resources, WorkerId};
use js_whole::WholeNodeConfig;
use js_whole::caps::Blocked;

use crate::whole_rig::{CAPACITY, Farm, GIB, caps, request, secs, w};

/// Whole-node settings with room everywhere (every cap far above three workers, no
/// ordinary work kept, a deadline that does not come), `edit` changing what a test is
/// about.
fn cfg(edit: impl FnOnce(&mut WholeNodeConfig)) -> WholeNodeConfig {
    let mut c = WholeNodeConfig {
        max_draining: 9,
        drain_deadline_ms: Some(3_600_000),
        ..WholeNodeConfig::dedicated(9)
    };
    edit(&mut c);
    c
}

/// A farm of `names`, each running one action that may run ten minutes. Actions go to the
/// least loaded worker, so each worker has exactly one.
fn busy(config: Option<WholeNodeConfig>, names: &[&str]) -> (Farm, Vec<LeaseGrant>) {
    let s = config.map_or_else(
        || Scheduler::new(1),
        |c| Scheduler::new(1).with_whole_node(c),
    );
    let mut f = Farm::new(s);
    let mut grants = Vec::new();
    for name in names {
        f.worker(name);
        grants.push(f.lease(secs(600)));
    }
    let mut at: Vec<&WorkerId> = grants.iter().map(|g| &g.worker).collect();
    at.sort();
    at.dedup();
    assert_eq!(at.len(), names.len(), "one action on each worker");
    (f, grants)
}

/// Asks for a whole machine that may run ten minutes, ten seconds after the last ask,
/// and runs a round: the operation. Nothing is placed: every worker is busy.
fn ask(f: &mut Farm) -> OperationId {
    let now = f.now;
    f.advance(now + 10_000);
    let op = f.submit_whole(Some(secs(600)));
    assert!(f.grants().is_empty(), "no worker is empty");
    op
}

fn mark(f: &Farm, op: OperationId) -> Option<WholeMark> {
    f.s.whole_mark(op).cloned()
}

fn wait(f: &Farm, op: OperationId) -> Option<WholeWait> {
    f.s.whole_wait(op)
}

/// The worker `op` holds, asserting that it holds one with a mark.
fn marked(f: &Farm, op: OperationId) -> WorkerId {
    let held = f.held_for(op).expect("holds a worker").clone();
    assert_eq!(mark(f, op).expect("marked").worker, held);
    assert_eq!(wait(f, op), None);
    held
}

fn waits(f: &Farm, op: OperationId, why: WholeWait) {
    assert_eq!(wait(f, op), Some(why));
    assert_eq!(mark(f, op), None, "no mark");
    assert_eq!(f.held_for(op), None, "no worker held");
}

fn blocked(why: Blocked) -> WholeWait {
    WholeWait::Cap(why)
}

/// Ends the action `grant` and runs a round: the grants it makes.
fn end(f: &mut Farm, grant: &LeaseGrant) -> Vec<LeaseGrant> {
    f.finish(grant);
    let at = f.now + 1_000;
    f.advance(at)
}

fn phase(f: &mut Farm, worker: &WorkerId, phase: NodePhase) {
    f.feed(Event::Phase {
        worker: worker.clone(),
        phase,
    });
}

/// Catches: `max_whole_node` not applied, or applied one off: with two as the cap the
/// first two requests hold a worker each and the third, though a third worker is busy and
/// free of holds, holds none and says the ceiling stopped it.
#[test]
fn max_whole_node_blocks_the_request_past_it() {
    let (mut f, _) = busy(Some(cfg(|c| c.max_whole_node = 2)), &["a", "b", "c"]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    let third = ask(&mut f);
    let (x, y) = (marked(&f, first), marked(&f, second));
    assert_ne!(x, y);
    waits(&f, third, blocked(Blocked::Ceiling));
}

/// Catches: the cap one too low: with three as the cap, all three requests hold a worker.
#[test]
fn one_below_max_whole_node_every_request_holds() {
    let (mut f, _) = busy(Some(cfg(|c| c.max_whole_node = 3)), &["a", "b", "c"]);
    let asked: Vec<OperationId> = (0..3).map(|_| ask(&mut f)).collect();
    let mut held: Vec<WorkerId> = asked.iter().map(|op| marked(&f, *op)).collect();
    held.sort();
    held.dedup();
    assert_eq!(held.len(), 3);
}

/// Catches: a whole lease not counted once its mark became a lease: the worker the first
/// request holds empties and is granted to it, and with one as the cap the second
/// request, though a worker is busy and free of holds, is stopped by the ceiling.
#[test]
fn a_held_whole_lease_counts_toward_max_whole_node() {
    let (mut f, grants) = busy(Some(cfg(|c| c.max_whole_node = 1)), &["a", "b"]);
    let first = ask(&mut f);
    let at = marked(&f, first);
    let own = grants.iter().find(|g| g.worker == at).unwrap();
    let granted = end(&mut f, own);
    assert_eq!(granted.len(), 1);
    assert_eq!((granted[0].operation, &granted[0].worker), (first, &at));
    assert_eq!(mark(&f, first), None, "a lease, not a mark");
    let second = ask(&mut f);
    waits(&f, second, blocked(Blocked::Ceiling));
}

/// Catches: `max_draining` not applied: with one as the cap the second request holds
/// nothing and says so, while `max_whole_node` has room.
#[test]
fn max_draining_blocks_the_request_past_it() {
    let (mut f, _) = busy(Some(cfg(|c| c.max_draining = 1)), &["a", "b", "c"]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    marked(&f, first);
    waits(&f, second, blocked(Blocked::MaxDraining));
}

/// Catches: `max_draining` one too low: with two as the cap the first two hold a worker
/// each and the third is stopped.
#[test]
fn one_below_max_draining_the_next_request_holds() {
    let (mut f, _) = busy(Some(cfg(|c| c.max_draining = 2)), &["a", "b", "c"]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    let third = ask(&mut f);
    marked(&f, first);
    marked(&f, second);
    waits(&f, third, blocked(Blocked::MaxDraining));
}

/// Catches: a worker held by a lease counted as draining: the first request's worker
/// empties and is granted to it, and with one draining at most the second request holds a
/// worker, since nothing is draining any more.
#[test]
fn a_held_whole_lease_is_not_draining() {
    let (mut f, grants) = busy(Some(cfg(|c| c.max_draining = 1)), &["a", "b"]);
    let first = ask(&mut f);
    let at = marked(&f, first);
    let own = grants.iter().find(|g| g.worker == at).unwrap();
    assert_eq!(end(&mut f, own).len(), 1);
    let second = ask(&mut f);
    assert_ne!(marked(&f, second), at);
}

/// Catches: `keep_for_ordinary` not applied: with one kept, three capable workers give two
/// whole; the third request holds nothing and says the ordinary work's share stopped it.
#[test]
fn keep_for_ordinary_leaves_a_capable_worker_unheld() {
    let (mut f, _) = busy(Some(cfg(|c| c.keep_for_ordinary = 1)), &["a", "b", "c"]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    let third = ask(&mut f);
    marked(&f, first);
    marked(&f, second);
    waits(&f, third, blocked(Blocked::KeepForOrdinary));
}

/// Catches: the kept count read as one whatever it is: with two kept, three workers give
/// one whole.
#[test]
fn keep_for_ordinary_keeps_as_many_as_it_says() {
    let (mut f, _) = busy(Some(cfg(|c| c.keep_for_ordinary = 2)), &["a", "b", "c"]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    marked(&f, first);
    waits(&f, second, blocked(Blocked::KeepForOrdinary));
}

/// Catches: `keep_for_ordinary` applied when it is zero: with none kept all three capable
/// workers are held.
#[test]
fn with_nothing_kept_every_capable_worker_may_be_held() {
    let (mut f, _) = busy(Some(cfg(|_| {})), &["a", "b", "c"]);
    let asked: Vec<OperationId> = (0..3).map(|_| ask(&mut f)).collect();
    asked.iter().for_each(|op| {
        marked(&f, *op);
    });
}

/// Catches: a worker that cannot be held whole counted as a capable one: `c` offers only
/// the container driver, so with one kept two capable workers give one whole.
#[test]
fn a_worker_that_cannot_be_held_whole_is_not_capable() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(|c| c.keep_for_ordinary = 1)));
    f.worker("a");
    f.worker("b");
    f.feed(Event::WorkerUp {
        worker: w("c"),
        instance: DaemonInstance::new("c"),
        predecessor_stopped: false,
        capacity: CAPACITY,
        caps: NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .unwrap()
            .with_drivers(["container"]),
    });
    f.workers.push(w("c"));
    for _ in 0..3 {
        f.lease(secs(600));
    }
    let first = ask(&mut f);
    let second = ask(&mut f);
    marked(&f, first);
    waits(&f, second, blocked(Blocked::KeepForOrdinary));
}

/// Worker `a` of 16 cores with one action, and `b` of 8 cores with one, as a farm with a
/// whole-machine request's worker `a` the cheaper to drain (its action ends sooner). An
/// ordinary action of 16 cores, which only `a` can ever run, waits ahead of the request
/// the caller asks next.
fn last_capable_farm(config: WholeNodeConfig, with_b: bool) -> (Farm, OperationId) {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(config));
    let big = Event::WorkerUp {
        worker: w("a"),
        instance: DaemonInstance::new("a"),
        predecessor_stopped: false,
        capacity: Resources::new(16_000, 32 * GIB),
        caps: caps(),
    };
    f.feed(big);
    f.workers.push(w("a"));
    let on_a = f.lease(secs(60));
    assert_eq!(on_a.worker, w("a"));
    if with_b {
        f.feed(Event::WorkerUp {
            worker: w("b"),
            instance: DaemonInstance::new("b"),
            predecessor_stopped: false,
            capacity: Resources::new(8_000, 16 * GIB),
            caps: caps(),
        });
        f.workers.push(w("b"));
        let on_b = f.lease(secs(600));
        assert_eq!(on_b.worker, w("b"));
    }
    f.next_n += 1;
    let ordinary = Request {
        resources: Resources::new(16_000, GIB),
        ..request(f.next_n, LeaseKind::Action)
    };
    f.submit(ordinary);
    assert!(f.grants().is_empty(), "a is not free enough for it");
    let whole = ask(&mut f);
    (f, whole)
}

/// Catches: the last capable worker of an older ordinary request held for a whole
/// machine: `a` is the only worker the 16-core action can run on, so the request holds
/// nothing and says so.
#[test]
fn the_last_capable_worker_of_an_older_request_is_not_held() {
    let (f, whole) = last_capable_farm(cfg(|_| {}), false);
    waits(&f, whole, WholeWait::LastCapable);
}

/// Catches: `allow_last_capable` ignored: with it on the request holds `a`.
#[test]
fn allow_last_capable_holds_the_last_capable_worker() {
    let (f, whole) = last_capable_farm(cfg(|c| c.allow_last_capable = true), false);
    assert_eq!(marked(&f, whole), w("a"));
}

/// Catches: the request giving up when its first choice is protected: `a` is the cheaper
/// to drain but the last capable worker of the older action, so it holds `b`.
#[test]
fn a_protected_worker_is_passed_over_for_the_next() {
    let (f, whole) = last_capable_farm(cfg(|_| {}), true);
    assert_eq!(marked(&f, whole), w("b"));
}

/// Catches: work queued after the request protecting its worker (a stream of it would
/// starve the request): the 16-core action is submitted after the whole machine, which
/// holds `a`.
#[test]
fn a_younger_ordinary_request_does_not_protect_its_worker() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(|_| {})));
    f.feed(Event::WorkerUp {
        worker: w("a"),
        instance: DaemonInstance::new("a"),
        predecessor_stopped: false,
        capacity: Resources::new(16_000, 32 * GIB),
        caps: caps(),
    });
    f.workers.push(w("a"));
    f.lease(secs(60));
    let whole = ask(&mut f);
    f.next_n += 1;
    let younger = Request {
        resources: Resources::new(16_000, GIB),
        ..request(f.next_n, LeaseKind::Action)
    };
    f.submit(younger);
    assert!(f.grants().is_empty());
    assert_eq!(marked(&f, whole), w("a"));
}

/// Two workers `a` and `b`, each running one action, and an ordinary action of a whole
/// worker's cores that can run on either and waits ahead of the whole-machine requests the
/// caller asks next.
fn two_workers_one_ordinary(config: WholeNodeConfig) -> Farm {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(config));
    f.worker("a");
    f.lease(secs(60));
    f.worker("b");
    f.lease(secs(600));
    f.next_n += 1;
    let ordinary = Request {
        resources: CAPACITY,
        ..request(f.next_n, LeaseKind::Action)
    };
    f.submit(ordinary);
    assert!(f.grants().is_empty(), "both workers are busy");
    f
}

/// Catches: a worker another request holds counted among those an ordinary request could
/// still run on: the first whole-machine request holds `a`, which leaves `b` the only
/// worker the older action can run on, so the second request is not given `b`.
#[test]
fn a_worker_held_for_another_request_is_not_one_an_older_request_can_use() {
    let mut f = two_workers_one_ordinary(cfg(|_| {}));
    let first = ask(&mut f);
    let second = ask(&mut f);
    assert_eq!(marked(&f, first), w("a"));
    waits(&f, second, WholeWait::LastCapable);
}

/// Catches: the same with `allow_last_capable` on ignored: the second request holds `b`.
#[test]
fn allow_last_capable_lets_the_second_request_hold_what_is_left() {
    let mut f = two_workers_one_ordinary(cfg(|c| c.allow_last_capable = true));
    let first = ask(&mut f);
    let second = ask(&mut f);
    assert_eq!(marked(&f, first), w("a"));
    assert_eq!(marked(&f, second), w("b"));
}

/// Two requests hold `x` and `y` (the farm has a third worker `z` and a cap that leaves
/// one for ordinary work, so the ceiling is two); the older held at 10 s, the newer at
/// 20 s. The worker held by neither.
fn two_held(config: WholeNodeConfig) -> (Farm, Vec<LeaseGrant>, [OperationId; 2], WorkerId) {
    let (mut f, grants) = busy(Some(config), &["a", "b", "c"]);
    let older = ask(&mut f);
    let newer = ask(&mut f);
    let (x, y) = (marked(&f, older), marked(&f, newer));
    let spare = ["a", "b", "c"]
        .into_iter()
        .map(w)
        .find(|n| *n != x && *n != y)
        .unwrap();
    (f, grants, [older, newer], spare)
}

fn keep_one() -> WholeNodeConfig {
    cfg(|c| c.keep_for_ordinary = 1)
}

/// Catches: marks left standing over a ceiling that fell, or the oldest dropped: when the
/// spare worker is quarantined (two capable, one kept, ceiling one) the newer request's
/// mark goes and the older keeps its worker. The newer request waits, is not given up (no
/// count), and holds a worker again when the spare returns.
#[test]
fn a_falling_ceiling_drops_the_newest_mark() {
    let (mut f, _, [older, newer], spare) = two_held(keep_one());
    let kept = mark(&f, older).unwrap();
    phase(
        &mut f,
        &spare,
        NodePhase::Quarantined("scan dirty".to_owned()),
    );
    assert!(f.grants().is_empty());
    assert_eq!(mark(&f, older), Some(kept));
    waits(&f, newer, blocked(Blocked::KeepForOrdinary));
    assert_eq!(f.s.whole_give_ups(newer), 0);
    phase(&mut f, &spare, NodePhase::Serving);
    assert!(f.grants().is_empty());
    marked(&f, newer);
    assert_eq!(f.s.whole_give_ups(newer), 0);
}

/// Catches: a worker leaving the pool (draining, told to stop) not lowering the ceiling.
#[test]
fn a_draining_worker_lowers_the_ceiling() {
    let (mut f, _, [older, newer], spare) = two_held(keep_one());
    phase(&mut f, &spare, NodePhase::Draining);
    assert!(f.grants().is_empty());
    marked(&f, older);
    waits(&f, newer, blocked(Blocked::KeepForOrdinary));
}

/// Catches: a lost worker (silent past the grace) taken out of the capable count: it may
/// come back, so the ceiling stays and both requests keep their marks.
#[test]
fn a_lost_worker_does_not_lower_the_ceiling() {
    let (mut f, _, [older, newer], spare) = two_held(keep_one());
    f.workers.retain(|n| *n != spare);
    f.advance(f.now + 600_000);
    marked(&f, older);
    marked(&f, newer);
}

/// Catches: a held lease revoked, or counted out, when the ceiling falls: the older
/// request's worker empties and is granted to it, the newer holds a mark, then the spare
/// is quarantined: the lease stays, the mark goes, and the request does not hold again
/// while the lease alone is at the ceiling.
#[test]
fn a_falling_ceiling_never_drops_a_held_lease() {
    let (mut f, grants, [older, newer], spare) = two_held(keep_one());
    let at = f.held_for(older).unwrap().clone();
    let own = grants.iter().find(|g| g.worker == at).unwrap();
    let granted = end(&mut f, own);
    assert_eq!(granted.len(), 1);
    marked(&f, newer);
    phase(
        &mut f,
        &spare,
        NodePhase::Quarantined("scan dirty".to_owned()),
    );
    assert!(f.grants().is_empty());
    assert_eq!(f.s.leases_held(), 3, "two actions and the whole lease");
    waits(&f, newer, blocked(Blocked::KeepForOrdinary));
}

/// Catches: `max_whole_node_per_consumer` applied as if the scheduler knew a consumer:
/// nothing in a request names one, so the limit of one is not enforced, and both requests
/// hold a worker.
#[test]
fn per_consumer_is_not_enforced() {
    let config = cfg(|c| c.max_whole_node_per_consumer = Some(1));
    let (mut f, _) = busy(Some(config), &["a", "b"]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    marked(&f, first);
    marked(&f, second);
}

/// Catches: the caps applied without whole-node settings: three requests hold a worker
/// each, with no mark and no reason, as before the settings existed.
#[test]
fn without_settings_no_cap_applies() {
    let (mut f, _) = busy(None, &["a", "b", "c"]);
    let asked: Vec<OperationId> = (0..3).map(|_| ask(&mut f)).collect();
    for op in asked {
        assert!(f.held_for(op).is_some());
        assert_eq!(mark(&f, op), None);
        assert_eq!(wait(&f, op), None);
    }
}

/// Catches: a request no worker could ever run counted, or left with a mark: it has no
/// worker to hold and no cap to name.
#[test]
fn a_request_no_worker_serves_holds_nothing_and_waits_for_no_cap() {
    let (mut f, _) = busy(Some(cfg(|_| {})), &["a"]);
    f.next_n += 1;
    let op = f.s.next_operation();
    f.submit(Request {
        needs: js_caps::Request::from_platform([("OSFamily", "macos")]).unwrap(),
        ..request(f.next_n, LeaseKind::WholeMachine)
    });
    assert!(f.grants().is_empty());
    assert_eq!(
        (mark(&f, op), wait(&f, op), f.held_for(op)),
        (None, None, None)
    );
}

/// A farm of two 8-core workers `a` and `b` under the load rule, as a blocked head of the
/// queue is held there: each runs four actions of a core, and a six-core head waits for
/// one of them, which it holds (`a`, the first by name), with whole-node settings of
/// `config` on.
fn head_held(config: WholeNodeConfig, workers: &[&str]) -> (load_rig::Rig, OperationId) {
    use load_rig::{GIB, Rig, w, warm};
    let mut r = Rig::new(LoadConfig::default());
    let off = std::mem::replace(&mut r.s, Scheduler::new(1));
    r.s = off.with_whole_node(config);
    let node = Resources::new(8_000, 32 * GIB);
    let whole = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container", "native-whole-machine"]);
    for name in workers {
        r.up_with(name, node, whole.clone());
        r.baseline(name);
    }
    for name in workers {
        for other in workers.iter().filter(|o| *o != name) {
            r.feed(Event::Cordon { worker: w(other) });
        }
        for _ in 0..4 {
            r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
        }
        assert_eq!(r.tick().len(), 4);
        for other in workers.iter().filter(|o| *o != name) {
            r.feed(Event::Uncordon { worker: w(other) });
        }
    }
    let head = r.action(6_000, 4 * GIB, Some(warm(6_000, 0)));
    r.round(1_000);
    assert_eq!(r.s.head_hold(head), Some(&w("a")));
    (r, head)
}

fn whole_request(r: &mut load_rig::Rig) -> OperationId {
    let request = r.request(LeaseKind::WholeMachine, 8_000, 16 * load_rig::GIB);
    let op = r.submit(request);
    r.s.set_max_wall(op, secs(600));
    r.round(1_000);
    op
}

/// Catches: a head hold counted as a whole mark: with one whole worker allowed and a
/// head holding `a`, the whole-machine request still holds `b`, not `a`, and has a mark.
#[test]
fn a_head_hold_is_not_a_whole_mark() {
    let config = cfg(|c| c.max_whole_node = 1);
    let (mut r, head) = head_held(config, &["a", "b"]);
    let op = whole_request(&mut r);
    assert_eq!(r.s.reservation(op), Some(&load_rig::w("b")));
    assert_eq!(r.s.whole_mark(op).unwrap().worker, load_rig::w("b"));
    assert_eq!(r.s.whole_wait(op), None);
    assert_eq!(
        r.s.head_hold(head),
        Some(&load_rig::w("a")),
        "the head keeps its hold"
    );
}

/// Catches: a whole mark taken on the worker a head holds for another request: with `a`
/// the only worker and held by the head, the whole-machine request holds nothing.
#[test]
fn a_worker_a_head_holds_is_not_marked_for_a_whole_machine() {
    let (mut r, head) = head_held(cfg(|_| {}), &["a"]);
    let op = whole_request(&mut r);
    assert_eq!(r.s.reservation(op), None);
    assert_eq!(r.s.whole_mark(op), None);
    assert_eq!(
        r.s.whole_wait(op),
        None,
        "no cap stops it: no worker is free of the hold"
    );
    assert_eq!(r.s.head_hold(head), Some(&load_rig::w("a")));
}
