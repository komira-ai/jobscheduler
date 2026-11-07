//! What the tests of the whole-node calls a caller makes from outside share
//! ([`js_sched::Scheduler::restore_whole`], [`js_sched::Scheduler::bar_whole`],
//! [`js_sched::Scheduler::cancel_whole`] and the reads): settings, farms and the restored
//! states they build. Every time is a value fed to the scheduler.

use std::collections::BTreeMap;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Event, Request, RestoredMark, RestoredWhole, Scheduler};
use js_types::sched::{FarmTime, LeaseGrant, LeaseKind, OperationId, Qos, WaiterId, WorkerId};
use js_whole::WholeNodeConfig;

use crate::booked;
use crate::whole_rig::{CAPACITY, Farm, request, secs, w};

pub const MAX_WALL: u64 = 600;

/// The farm time `ms` milliseconds after the epoch.
pub fn t(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

/// Whole-node settings: a drain deadline of a minute, a back-off of ten seconds, and
/// `drain_after_ms` and `attempts` as given.
pub fn cfg(drain_after_ms: u64, attempts: u32) -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms,
        drain_deadline_ms: Some(60_000),
        reserve_backoff_ms: 10_000,
        max_reserve_attempts: attempts,
        ..WholeNodeConfig::dedicated(4)
    }
}

/// A scheduler placing by the booked rule with whole-node settings `config`.
pub fn farm(config: WholeNodeConfig) -> Farm {
    Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(config),
    )
}

/// One worker `a` with a lease that may run ten minutes, from farm time zero.
pub fn busy(config: WholeNodeConfig) -> Farm {
    let mut f = farm(config);
    f.worker("a");
    f.lease(secs(MAX_WALL));
    f
}

/// Submits a whole-machine request with the ten-minute wall time at the current farm time
/// and runs a round in which nothing is granted.
pub fn ask(f: &mut Farm) -> OperationId {
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    assert!(f.grants().is_empty());
    op
}

/// Each grant's operation and worker.
pub fn placed(grants: &[LeaseGrant]) -> Vec<(OperationId, WorkerId)> {
    grants
        .iter()
        .map(|g| (g.operation, g.worker.clone()))
        .collect()
}

/// Everything the scheduler holds, to compare before and after a call.
pub fn snapshot(f: &Farm) -> String {
    format!("{:?}", f.s)
}

/// The caller of the last request [`Farm`] submitted.
pub fn waiter(f: &Farm) -> WaiterId {
    WaiterId(u64::from(f.next_n))
}

/// A restored state for `operation` queued at zero, with nothing barred.
pub fn restored(operation: OperationId, mark: Option<RestoredMark>) -> RestoredWhole {
    RestoredWhole {
        operation,
        queued_at: t(0),
        bars: BTreeMap::new(),
        mark,
    }
}

/// A restored mark on `worker`, made at zero, due at ten minutes, tail unknown.
pub fn mark_on(worker: &str) -> RestoredMark {
    RestoredMark {
        worker: w(worker),
        made: t(0),
        deadline: t(600_000),
        tail_end: None,
    }
}

/// Registers `name` with no whole-machine driver.
pub fn ordinary_worker(f: &mut Farm, name: &str) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"]);
    f.feed(Event::WorkerUp {
        worker: w(name),
        instance: DaemonInstance::new(name),
        predecessor_stopped: false,
        capacity: CAPACITY,
        caps,
    });
    f.workers.push(w(name));
}

/// A ceiling no wall time in these tests reaches.
pub const DAY: Duration = Duration::from_secs(86_400);

/// Submits an action that may run `wall` at the farm time now and runs a round: the
/// worker it was placed on, if it was, its run started.
pub fn job(f: &mut Farm, wall: Duration) -> Option<WorkerId> {
    f.next_n += 1;
    let operation = f.s.next_operation();
    f.s.set_max_wall(operation, wall);
    f.submit(Request {
        qos: Qos::Ci,
        ..request(f.next_n, LeaseKind::Action)
    });
    let grant = f.grants().into_iter().find(|g| g.operation == operation)?;
    f.start(&grant);
    Some(grant.worker)
}
