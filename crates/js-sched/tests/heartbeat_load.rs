//! A scheduler that was not given a load configuration (the booked rule) accepts a
//! heartbeat's `load` and does not act on it: it answers a heartbeat with a load exactly
//! as it answers the same heartbeat without one.

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Input, Scheduler};
use js_types::sched::usage::NodeLoad;
use js_types::sched::{Effect, FarmTime, LeaseId, Resources, StateMachine, WorkerId};

const GIB: u64 = 1 << 30;

fn farm() -> Scheduler {
    let mut s = Scheduler::new(1);
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let up = Event::WorkerUp {
        worker: WorkerId::new("node-a"),
        instance: DaemonInstance::new("one"),
        predecessor_stopped: false,
        capacity: Resources::new(1_000, GIB),
        caps,
    };
    assert!(s.apply(Input::new(FarmTime::from_millis(0), up)).is_empty());
    s
}

fn beat(s: &mut Scheduler, load: Option<NodeLoad>) -> Vec<Effect> {
    let event = Event::Heartbeat {
        worker: WorkerId::new("node-a"),
        running: Vec::new(),
        load,
    };
    s.apply(Input::new(FarmTime::from_millis(5_000), event))
}

#[test]
fn a_load_changes_nothing_the_scheduler_does() {
    let load = NodeLoad {
        seq: 7,
        cpu_usage_usec: u64::MAX,
        mono_usec: 1,
        working_set_bytes: u64::MAX,
        covered: vec![LeaseId::new(1, 1)],
        actions_cpu_limited: true,
        throttled_usec: 3,
        pressure_some_total_usec: 4,
    };
    let (mut with, mut without) = (farm(), farm());
    assert_eq!(
        beat(&mut with, Some(load.clone())),
        beat(&mut without, None)
    );
    assert_eq!(beat(&mut with, None), beat(&mut without, None));
}
