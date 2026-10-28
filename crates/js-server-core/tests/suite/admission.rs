//! Which hosts the core hears, the lock wait a submit reports, and how a core is built.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::common::{
    Rig, at, beat, begin, block_on, core, reported, resend, submit, try_up, up, worker,
};
use js_sched::{FINISHED_RETENTION, UNSERVABLE_WAIT};
use js_server_core::{
    Admission, Beat, Config, Core, Denied, NodeAction, Observer, Reported, SelfFencedOnly,
};
use js_types::sched::{LeaseId, WorkerId};

/// Refuses the registrations, heartbeats and results of one worker.
pub(crate) struct Deny(pub(crate) &'static str);

impl Admission for Deny {
    fn allow_host(&self, worker: &WorkerId, _: &Reported) -> Result<(), Denied> {
        if worker.as_str() == self.0 {
            Err(Denied::new("denied host"))
        } else {
            Ok(())
        }
    }

    fn allow_beat(&self, worker: &WorkerId, _: &Beat) -> Result<(), Denied> {
        if worker.as_str() == self.0 {
            Err(Denied::new("denied beat"))
        } else {
            Ok(())
        }
    }

    fn allow_result(&self, worker: &WorkerId, _: LeaseId) -> Result<(), Denied> {
        if worker.as_str() == self.0 {
            Err(Denied::new("denied result"))
        } else {
            Ok(())
        }
    }
}

/// Catches: a host an operator denied reaching the scheduler through any of the doors, and
/// a host that is not denied turned away.
#[test]
fn a_denied_host_is_heard_in_none_of_register_resize_and_heartbeat() {
    begin();
    let core = Core::new(&Config::new(1), Rig::default()).with_admission(Deny("node-x"));
    let stream = up(&core, 0, "node-a", "d1");
    let refused = try_up(&core, 1, "node-x", ("d1", ""), reported(8));
    assert_eq!(refused, Err(Denied::new("denied host")));
    assert_eq!(core.sessions(), 1);

    let resized = resend(&core, 2, ("node-x", stream), reported(8));
    assert_eq!(resized, Err(Denied::new("denied host")));
    assert_eq!(core.with(|_, rig| rig.resized), 0);
    let beat = block_on(core.heartbeat(at(3), &worker("node-x"), stream, Beat::new(1, vec![])));
    assert_eq!(beat, Err(Denied::new("denied beat")));
    assert_eq!(Denied::new("why").to_string(), "why");
}

/// Catches: a host that does not stop its own runs registering with a scheduler that
/// gives its lease to another host after a grace shorter than its run.
#[test]
fn a_host_that_does_not_fence_itself_is_refused_when_that_is_the_policy() {
    begin();
    let core = Core::new(&Config::new(1), Rig::default()).with_admission(SelfFencedOnly);
    let register = |reported: Reported| try_up(&core, 0, "node-a", ("d1", ""), reported);
    assert!(register(reported(8)).is_ok());
    let refused = register(reported(8).without_self_fence()).unwrap_err();
    assert_eq!(
        refused.reason,
        "node-a does not report that it fences itself"
    );
    // The other doors are open to a host that registered.
    let stream = register(reported(8)).expect("registered");
    let beat = block_on(core.heartbeat(at(1), &worker("node-a"), stream, Beat::new(1, vec![])));
    assert_eq!(beat, Ok(true));
}

struct Count(Arc<AtomicUsize>);

impl Observer for Count {
    fn lock_waited(&self, _: Duration) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Catches: the lock wait observed anywhere but in a submit (a gauge that every
/// acquisition moves), or not observed by a submit.
#[test]
fn the_lock_wait_is_observed_in_a_submit_only() {
    begin();
    let waits = Arc::new(AtomicUsize::new(0));
    let core = Core::new(&Config::new(1), Rig::default()).with_observer(Count(Arc::clone(&waits)));
    let stream = up(&core, 0, "node-a", "d1");
    let _ = core.with(|_, rig| rig.open.len());
    assert!(beat(&core, 1, ("node-a", stream), 1, &[]));
    block_on(core.tick(at(2)));
    core.place(at(2), &worker("node-a"), NodeAction::Cordon)
        .expect("placed");
    let _ = core.queued_operations();
    assert_eq!(waits.load(Ordering::SeqCst), 0);
    submit(&core, 3, 1);
    assert_eq!(waits.load(Ordering::SeqCst), 1);
    submit(&core, 4, 2);
    assert_eq!(waits.load(Ordering::SeqCst), 2);
}

/// Catches: a default configuration other than the scheduler's, and a builder that sets
/// the wrong field.
#[test]
fn a_configuration_is_the_schedulers_defaults_under_its_term() {
    let config = Config::new(9);
    assert_eq!(config.term, 9);
    assert_eq!(config.unservable_wait, UNSERVABLE_WAIT);
    assert_eq!(config.finished_retention, FINISHED_RETENTION);
    let set = config
        .with_unservable_wait(Duration::from_secs(1))
        .with_finished_retention(Duration::from_secs(2));
    assert_eq!(set.unservable_wait, Duration::from_secs(1));
    assert_eq!(set.finished_retention, Duration::from_secs(2));
}

/// Catches: a request that joins an operation in flight being given a new operation, or a
/// distinct request joining another's.
#[test]
fn a_request_for_work_in_flight_joins_it() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let first = submit(&core, 1, 1);
    let joined = submit(&core, 2, 1);
    let other = submit(&core, 3, 2);
    assert_eq!(joined.operation, first.operation);
    assert!(joined.joined && !first.joined && !other.joined);
    assert_ne!(joined.waiter, first.waiter);
    assert_ne!(other.operation, first.operation);
}
