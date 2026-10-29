//! An action that asks for a whole worker (`Request::whole_worker`): it runs alone on a
//! worker that holds no other lease, books all of it, holds a worker while it waits for
//! it to empty, fits workers of any size, and has nowhere to climb when its limit is hit.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

use crate::digest::{Digest, DigestFunction};
use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Request, Scheduler, harness};
use js_types::sched::{
    Effect, Failure, FarmTime, LeaseKind, OperationId, Outcome, Qos, Resources, StartLease,
    WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
const SMALL: Resources = Resources::new(4_000, 8 * GIB);
const LARGE: Resources = Resources::new(16_000, 64 * GIB);

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// Action `n`: one core and 1 GiB.
fn action(n: u8) -> Request {
    Request {
        key: crate::digest::work_key("main", digest(n)),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
        needs: js_caps::Request::default(),
    }
}

/// Action `n`, asking for a whole worker. Its memory is a size no worker has.
fn whole(n: u8) -> Request {
    Request {
        whole_worker: true,
        resources: Resources::new(1_000, u64::MAX),
        ..action(n)
    }
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

struct Rig {
    s: Scheduler,
}

impl Rig {
    fn new() -> Self {
        Self {
            s: Scheduler::new(1).with_load_config(booked::current()),
        }
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        harness::feed(&mut self.s, FarmTime::from_millis(1_000), event)
    }

    fn worker(&mut self, name: &str, capacity: Resources) {
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .unwrap()
            .with_drivers(["container"]);
        let up = Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity,
            caps,
        };
        assert!(self.feed(up).is_empty());
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        self.feed(Event::Submit { waiter, request });
    }

    /// A placement round: the leases it starts.
    fn starts(&mut self) -> Vec<StartLease> {
        self.feed(Event::Tick)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Start(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// The worker the operation's lease is on, if it holds one.
    fn worker_of(&self, start: &StartLease) -> WorkerId {
        start.worker.clone()
    }

    /// `start`'s lease ends with `outcome`; the effects of that and of a placement round.
    fn end(&mut self, start: &StartLease, outcome: Outcome) -> Vec<Effect> {
        let mut effects = self.feed(Event::Report {
            operation: start.operation,
            lease: start.lease,
            outcome,
        });
        effects.extend(self.feed(Event::Tick));
        effects
    }

    fn completes(&mut self, start: &StartLease) -> Vec<StartLease> {
        let outcome = Outcome::Completed {
            action_result: digest(200).into(),
        };
        self.end(start, outcome)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Start(s) => Some(s),
                _ => None,
            })
            .collect()
    }
}

both_rules! {
/// Catches: a whole-worker request refused or placed short on an idle worker: it must
/// be placed, its `Start` must carry the worker's whole capacity (not the request's
/// resources, which no worker has), and the worker's booking must be all of it.
fn a_whole_worker_request_places_on_an_idle_worker_and_books_all_of_it() {
    let mut r = Rig::new();
    r.worker("a", SMALL);
    r.submit(1, whole(1));
    let [start] = r.starts().try_into().unwrap();
    assert_eq!(start.worker, w("a"));
    assert_eq!(start.resources, SMALL);
    assert_eq!(r.s.booked(&w("a")), Some(SMALL));
    assert!(r.s.unservable(OperationId(0)).is_none());
}
}

both_rules! {
/// Catches: a whole-worker request placed beside a lease (it must wait until the worker
/// is empty), one that never places after the lease ends, and a worker whose booking is
/// not released with the whole-worker lease.
fn a_whole_worker_request_waits_for_the_worker_to_empty() {
    let mut r = Rig::new();
    r.worker("a", SMALL);
    r.submit(1, action(1));
    let [first] = r.starts().try_into().unwrap();

    r.submit(2, whole(2));
    assert!(r.starts().is_empty(), "placed beside a lease");
    assert_eq!(r.s.reservation(OperationId(1)), Some(&w("a")));

    let [second] = r.completes(&first).try_into().unwrap();
    assert_eq!(second.operation, OperationId(1));
    assert_eq!(second.resources, SMALL);

    assert!(r.completes(&second).is_empty());
    assert_eq!(r.s.booked(&w("a")), Some(Resources::new(0, 0)));
}
}

both_rules! {
/// Catches: a worker refilled by small work while a whole-worker request waits for it
/// (the request would starve behind a stream of them): a request submitted after it is
/// not placed on the worker it holds, and goes to another worker when one is free. And:
/// work placed beside a whole-worker lease.
fn a_worker_held_for_a_whole_worker_request_takes_no_later_work() {
    let mut r = Rig::new();
    r.worker("a", SMALL);
    r.submit(1, action(1));
    let [first] = r.starts().try_into().unwrap();

    r.submit(2, whole(2));
    r.submit(3, action(3));
    assert!(r.starts().is_empty(), "work landed on a held worker");

    // The held worker empties: the whole-worker request has it, the later one waits.
    let [second] = r.completes(&first).try_into().unwrap();
    assert_eq!(second.operation, OperationId(1));
    assert_eq!(r.worker_of(&second), w("a"));
    assert!(r.starts().is_empty(), "work landed beside a whole-worker lease");

    // A second worker takes the waiting request, not the first.
    r.worker("b", SMALL);
    let [third] = r.starts().try_into().unwrap();
    assert_eq!((third.operation, third.worker), (OperationId(2), w("b")));
}
}

both_rules! {
/// Catches: a whole-worker request that is not servable on a cluster of mixed worker
/// sizes (a request for the largest worker's memory is unservable once only a smaller
/// worker is live), or one placed on a busy worker because it was idle elsewhere.
fn a_whole_worker_request_is_servable_by_any_worker_and_takes_an_idle_one() {
    let mut r = Rig::new();
    r.worker("small", SMALL);
    r.submit(1, whole(1));
    let [start] = r.starts().try_into().unwrap();
    assert_eq!(start.resources, SMALL);

    // The same size asked as a booking is not servable by the small worker.
    let sized = Request {
        whole_worker: false,
        ..whole(2)
    };
    r.submit(2, sized);
    assert!(r.starts().is_empty());
    assert!(r.s.unservable(OperationId(1)).is_some());

    // A large worker joins; a whole-worker request takes it, whole.
    r.worker("large", LARGE);
    r.submit(3, whole(3));
    let starts = r.starts();
    let on_large: Vec<_> = starts.iter().filter(|s| s.operation == OperationId(2)).collect();
    let [third] = on_large.as_slice() else {
        panic!("{starts:?}");
    };
    assert_eq!((third.worker.clone(), third.resources), (w("large"), LARGE));
}
}

both_rules! {
/// Catches: an own-limit kill of a whole-worker request that climbs the memory ladder
/// to a larger worker's memory (it already holds a whole worker; a rerun asks for the
/// same thing) instead of failing like a kill at the cap.
fn an_own_limit_kill_of_a_whole_worker_request_fails_without_climbing() {
    let mut r = Rig::new();
    r.worker("small", SMALL);
    r.submit(1, whole(1));
    let [start] = r.starts().try_into().unwrap();
    // A larger worker that could run it is live when it is killed.
    r.worker("large", LARGE);

    let effects = r.end(&start, Outcome::Failed(Failure::OutOfMemory));
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Start(_))),
        "{effects:?}"
    );
    let answers: Vec<_> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Answer(a) => Some(a.outcome),
            _ => None,
        })
        .collect();
    assert_eq!(answers, [Outcome::Failed(Failure::OutOfMemory)]);
}
}

both_rules! {
/// Catches: a default request changed by the new field: with `whole_worker` false an
/// action is placed beside another, books only what it asked for, and holds no worker.
fn without_the_flag_a_request_shares_its_worker_as_before() {
    let mut r = Rig::new();
    r.worker("a", SMALL);
    r.submit(1, action(1));
    r.submit(2, action(2));
    let starts = r.starts();
    assert_eq!(starts.len(), 2);
    assert!(starts.iter().all(|s| s.resources == Resources::new(1_000, GIB)));
    assert_eq!(r.s.reservation(OperationId(0)), None);
}
}
