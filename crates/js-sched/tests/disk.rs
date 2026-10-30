//! Disk placement: a request that names scratch disk goes only to a node with that much
//! disk free, and the disk is held by one lease from its grant until the lease ends.

#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

use crate::digest::{Digest, DigestFunction};
use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseGrant, OperationId, Outcome, Qos, Resources,
    StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// Action `n`, one core and 1 GiB, and `disk` bytes of disk.
fn request(n: u8, disk: u64) -> Request {
    Request {
        key: crate::digest::work_key("main", digest(n)),
        qos: Qos::Ci,
        kind: js_types::sched::LeaseKind::Action,
        resources: Resources::new(1_000, GIB).with_disk_bytes(disk),
        hermetic: true,
        do_not_cache: false,
        needs: js_caps::Request::default(),
        whole_worker: false,
    }
}

/// A Linux x86-64 node, as its report describes it.
fn caps() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// A scheduler at a fixed farm time: nothing here waits on a grace period.
struct Harness(Scheduler);

impl Harness {
    fn new() -> Self {
        Self(Scheduler::new(1))
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.0
            .apply(Input::new(FarmTime::from_millis(1_000), event))
    }

    /// Registers `name` (again) with 8 cores, 16 GiB and `disk` bytes of disk.
    fn worker(&mut self, name: &str, disk: u64) {
        let capacity = Resources::new(8_000, 16 * GIB).with_disk_bytes(disk);
        assert!(
            self.feed(Event::WorkerUp {
                worker: w(name),
                instance: DaemonInstance::new(name),
                predecessor_stopped: false,
                capacity,
                caps: caps(),
            })
            .is_empty()
        );
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// Ticks and returns the grants proposed. A request larger than every node waits
    /// with a reason; that is checked in `platform.rs`, and skipped here.
    fn tick(&mut self) -> Vec<LeaseGrant> {
        self.feed(Event::Tick)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Commit(ControlRecord::Lease(g)) => Some(g),
                Effect::Waiting(_) => None,
                other => panic!("a tick proposed {other:?}"),
            })
            .collect()
    }

    /// Commits `grant` and reports it started.
    fn start(&mut self, grant: &LeaseGrant) {
        let effects = self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
        assert!(
            matches!(effects.as_slice(), [Effect::Start(_)]),
            "{effects:?}"
        );
        self.feed(Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        });
    }

    /// Reports `grant` completed and commits the result.
    fn finish(&mut self, grant: &LeaseGrant) {
        let outcome = Outcome::Completed {
            action_result: digest(200).into(),
        };
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome,
        });
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("{proposed:?}");
        };
        let answered = self.feed(Event::Committed(record.clone()));
        assert!(
            matches!(answered.as_slice(), [Effect::Answer(_)]),
            "{answered:?}"
        );
    }

    fn disk_booked(&self, name: &str) -> u64 {
        self.0.booked(&w(name)).expect("registered").disk_bytes
    }
}

/// Catches: disk booked as if it were shared, so two 60 GiB requests run on a 100 GiB
/// node at once; disk not given back when its lease ends, so the next request queues
/// forever; and disk-free work held back by a booked disk.
#[test]
fn a_hundred_gib_node_holds_one_sixty_gib_lease_at_a_time() {
    let mut h = Harness::new();
    h.worker("d", 100 * GIB);
    h.submit(1, request(1, 60 * GIB));
    h.submit(2, request(2, 60 * GIB));
    let [first] = h.tick().try_into().unwrap();
    assert_eq!(first.worker, w("d"));
    assert_eq!(h.disk_booked("d"), 60 * GIB);
    assert!(
        h.tick().is_empty(),
        "a second 60 GiB lease on a 100 GiB node"
    );

    // The node has room for work that names no disk beside the disk lease.
    h.submit(3, request(3, 0));
    let [plain] = h.tick().try_into().unwrap();
    assert_eq!(plain.worker, w("d"));
    assert_eq!(h.disk_booked("d"), 60 * GIB);

    h.start(&first);
    h.finish(&first);
    assert_eq!(h.disk_booked("d"), 0, "the disk outlived its lease");
    let [second] = h.tick().try_into().unwrap();
    assert_eq!(second.worker, w("d"));
    assert_eq!(h.disk_booked("d"), 60 * GIB);
}

/// Catches: a request placed on a node with less disk than it asks for (`a`, the first in
/// name order and the least loaded, would be picked), and a request larger than every
/// node being granted anywhere.
#[test]
fn a_disk_request_waits_for_a_node_with_the_disk() {
    let mut h = Harness::new();
    h.worker("a", 10 * GIB);
    h.worker("b", 50 * GIB);
    h.submit(1, request(1, 80 * GIB));
    h.submit(2, request(2, 30 * GIB));
    let [one] = h.tick().try_into().unwrap();
    assert_eq!((one.operation, one.worker), (OperationId(1), w("b")));
    assert_eq!(
        h.0.queued().collect::<Vec<_>>(),
        [OperationId(0)],
        "the 80 GiB request was placed"
    );
    assert_eq!(h.disk_booked("a"), 0);

    h.worker("c", 100 * GIB);
    let [two] = h.tick().try_into().unwrap();
    assert_eq!((two.operation, two.worker), (OperationId(0), w("c")));
    assert_eq!(h.disk_booked("c"), 80 * GIB);
}
