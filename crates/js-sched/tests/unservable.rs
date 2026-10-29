//! The typed reason a request is unservable: each variant is produced by its case, its
//! `Display` is the text callers are told, and `Scheduler::waiting` agrees with both.

#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

use crate::digest::{Digest, DigestFunction};

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Input, Request, Scheduler, Unservable};
use js_types::sched::{
    Effect, FarmTime, LeaseKind, NodePhase, OperationId, Qos, Resources, StateMachine, WaiterId,
    Waiting, WorkerId,
};

const GIB: u64 = 1 << 30;

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn request(kind: LeaseKind, platform: &[(&str, &str)], resources: Resources) -> Request {
    Request {
        key: crate::digest::work_key("main", Digest::new(DigestFunction::Sha256, [1; 32], 1)),
        qos: Qos::Ci,
        kind,
        resources,
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
        needs: js_caps::Request::from_platform(platform.iter().copied()).unwrap(),
    }
}

fn caps(os: &str, drivers: &[&str]) -> NodeCaps {
    let arch = if os == "macos" { "arm64" } else { "x86_64" };
    NodeCaps::from_report([("arch", arch), ("os", os)])
        .unwrap()
        .with_drivers(drivers.iter().copied())
}

struct Harness {
    s: Scheduler,
}

impl Harness {
    fn new() -> Self {
        Self {
            s: Scheduler::new(1),
        }
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(FarmTime::default(), event))
    }

    fn worker(&mut self, name: &str, caps: NodeCaps) {
        self.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(4_000, 8 * GIB),
            caps,
        });
    }

    /// Submits `request` and ticks: the typed reason the scheduler records for it, and
    /// the text its callers are told.
    fn wait_on(&mut self, request: Request) -> (Unservable, String) {
        self.feed(Event::Submit {
            waiter: WaiterId(0),
            request,
        });
        let told = self.feed(Event::Tick);
        let op = OperationId(0);
        let cause = self.s.unservable(op).expect("it waits").clone();
        let text = self.s.waiting(op).expect("it waits").to_owned();
        assert_eq!(cause.to_string(), text, "one reason, two forms");
        assert!(
            told.iter().any(
                |e| matches!(e, Effect::Waiting(Waiting { reason: Some(r), .. }) if *r == text)
            ),
            "{told:?}"
        );
        (cause, text)
    }
}

fn action(platform: &[(&str, &str)]) -> Request {
    request(LeaseKind::Action, platform, Resources::new(1_000, GIB))
}

#[test]
fn no_connected_worker() {
    let mut h = Harness::new();
    let (cause, text) = h.wait_on(action(&[]));
    assert_eq!(cause, Unservable::NoWorker);
    assert_eq!(text, "no worker is connected");
}

#[test]
fn every_connected_worker_cordoned() {
    let mut h = Harness::new();
    h.worker("a", caps("linux", &["container"]));
    h.feed(Event::Cordon { worker: w("a") });
    // None of them could run it even uncordoned, so the cordon is not the reason.
    let (cause, text) = h.wait_on(action(&[("OSFamily", "macos")]));
    assert_eq!(cause, Unservable::EveryWorkerCordoned);
    assert_eq!(text, "every connected worker is cordoned");
}

#[test]
fn every_connected_worker_held_back() {
    let mut h = Harness::new();
    h.worker("a", caps("linux", &["container"]));
    h.worker("b", caps("linux", &["container"]));
    h.feed(Event::Cordon { worker: w("a") });
    h.feed(Event::Phase {
        worker: w("b"),
        phase: NodePhase::Quarantined("probe".to_owned()),
    });
    let (cause, text) = h.wait_on(action(&[("OSFamily", "macos")]));
    assert_eq!(cause, Unservable::EveryWorkerHeldBack);
    assert_eq!(text, "every connected worker is cordoned or not serving");
}

#[test]
fn no_worker_serves_the_lease_kind() {
    let mut h = Harness::new();
    h.worker("a", caps("linux", &["container"]));
    h.worker("b", caps("linux", &["container"]));
    let (cause, text) = h.wait_on(request(
        LeaseKind::WholeMachine,
        &[],
        Resources::new(1_000, GIB),
    ));
    assert_eq!(
        cause,
        Unservable::NoDriver {
            live: 2,
            kind: LeaseKind::WholeMachine
        }
    );
    assert_eq!(
        text,
        "none of the 2 live worker(s) serves lease kind whole_machine: that needs a driver \
         among native-whole-machine"
    );
}

#[test]
fn the_platform_matches_no_worker() {
    let mut h = Harness::new();
    h.worker("a", caps("linux", &["container"]));
    let (cause, text) = h.wait_on(action(&[("OSFamily", "macos")]));
    assert_eq!(
        cause,
        Unservable::PlatformMismatch {
            serving: 1,
            some_cannot_serve: false,
            kind: LeaseKind::Action,
            closest: w("a"),
            lacks: vec!["os=macos".to_owned()],
        }
    );
    assert_eq!(
        text,
        "none of the 1 live worker(s) satisfies the action's platform; the closest, a, \
         lacks os=macos"
    );
}

#[test]
fn the_platform_matches_no_worker_that_serves_the_kind() {
    let mut h = Harness::new();
    h.worker("a", caps("linux", &["container"]));
    h.worker("b", caps("linux", &["vm"]));
    let (cause, text) = h.wait_on(action(&[("OSFamily", "macos")]));
    assert!(
        matches!(
            cause,
            Unservable::PlatformMismatch {
                serving: 1,
                some_cannot_serve: true,
                ..
            }
        ),
        "{cause:?}"
    );
    assert_eq!(
        text,
        "none of the 1 live worker(s) serving lease kind action satisfies the action's \
         platform; the closest, a, lacks os=macos"
    );
}

#[test]
fn every_matching_worker_is_smaller_than_the_request() {
    let mut h = Harness::new();
    h.worker("a", caps("linux", &["container"]));
    h.worker("b", caps("linux", &["container"]));
    h.worker("m", caps("macos", &["native"]));
    let asked = Resources::new(64_000, 128 * GIB);
    let (cause, text) = h.wait_on(request(LeaseKind::Action, &[("OSFamily", "linux")], asked));
    assert_eq!(cause, Unservable::TooLarge { matching: 2, asked });
    assert_eq!(
        text,
        format!(
            "the 2 live worker(s) that satisfy the action's platform are all smaller than its \
             request (64000 millicores, {} bytes of memory, 0 GPU(s), 0 VM slot(s))",
            128 * GIB
        )
    );
}

#[test]
fn only_cordoned_workers_could_run_it() {
    let mut h = Harness::new();
    h.worker("a", caps("linux", &["container"]));
    h.worker("m", caps("macos", &["native"]));
    h.feed(Event::Cordon { worker: w("m") });
    let (cause, text) = h.wait_on(action(&[("OSFamily", "macos")]));
    assert_eq!(
        cause,
        Unservable::Cordoned {
            workers: vec![(w("m"), NodePhase::Serving)]
        }
    );
    assert_eq!(text, "every live worker that can run it is cordoned: m");
}

#[test]
fn only_a_worker_not_serving_could_run_it() {
    let mut h = Harness::new();
    h.worker("a", caps("linux", &["container"]));
    h.worker("m", caps("macos", &["native"]));
    h.worker("n", caps("macos", &["native"]));
    h.feed(Event::Cordon { worker: w("m") });
    h.feed(Event::Phase {
        worker: w("n"),
        phase: NodePhase::Quarantined("probe".to_owned()),
    });
    let (cause, text) = h.wait_on(action(&[("OSFamily", "macos")]));
    assert_eq!(
        cause,
        Unservable::Cordoned {
            workers: vec![
                (w("m"), NodePhase::Serving),
                (w("n"), NodePhase::Quarantined("probe".to_owned()))
            ]
        }
    );
    assert_eq!(
        text,
        "every live worker that can run it is cordoned or not serving: m, n (quarantined)"
    );
}

/// The text of a value built by hand, so a change to `Display` shows without a scheduler.
#[test]
fn display_of_each_variant_is_pinned() {
    let cases = [
        (Unservable::NoWorker, "no worker is connected"),
        (
            Unservable::EveryWorkerCordoned,
            "every connected worker is cordoned",
        ),
        (
            Unservable::EveryWorkerHeldBack,
            "every connected worker is cordoned or not serving",
        ),
        (
            Unservable::NoDriver {
                live: 3,
                kind: LeaseKind::Vm,
            },
            "none of the 3 live worker(s) serves lease kind vm: that needs a driver among vm",
        ),
        (
            Unservable::NoDriver {
                live: 1,
                kind: LeaseKind::Action,
            },
            "none of the 1 live worker(s) serves lease kind action: that needs a driver \
             among container, native, fake, local",
        ),
        (
            Unservable::PlatformMismatch {
                serving: 2,
                some_cannot_serve: false,
                kind: LeaseKind::Action,
                closest: w("x"),
                lacks: vec!["os=macos".to_owned(), "arch=arm64".to_owned()],
            },
            "none of the 2 live worker(s) satisfies the action's platform; the closest, x, \
             lacks os=macos, arch=arm64",
        ),
        (
            Unservable::TooLarge {
                matching: 1,
                asked: Resources {
                    gpus: 2,
                    vms: 3,
                    ..Resources::new(500, 7)
                },
            },
            "the 1 live worker(s) that satisfy the action's platform are all smaller than \
             its request (500 millicores, 7 bytes of memory, 2 GPU(s), 3 VM slot(s))",
        ),
    ];
    for (cause, text) in cases {
        assert_eq!(cause.to_string(), text);
    }
}
