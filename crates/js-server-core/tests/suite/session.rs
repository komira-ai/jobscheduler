//! A host's session: the stream a worker registers on, the heartbeats it sends on it, the
//! report it resends, and its phase (moved from the farm core's tests of the same rules).

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::common::{
    Call, Rig, at, beat, block_on, calls, close, core, lease, reported, resend, started, submit,
    up, up_after, worker,
};
use js_sched::NodePhase;
use js_sched::fence::LEASE_GRACE;
use js_server_core::{Beat, Core, Stream};
use js_types::sched::usage::NodeLoad;

fn requeued(core: &Core<Rig>) -> bool {
    calls(core)
        .iter()
        .any(|call| matches!(call, Call::Requeued(_)))
}

/// Catches: a replaced stream still heard (its heartbeat acknowledged, or fed), and a host
/// the core never heard of acknowledged.
#[test]
fn only_the_newest_stream_of_a_worker_is_heard() {
    let core = core();
    let first = up(&core, 0, "node-a", "d1");
    let second = up(&core, 1, "node-a", "d1");
    assert!(second > first, "streams are numbered as hosts register");
    assert_eq!(core.sessions(), 1);
    assert!(!beat(&core, 2, ("node-a", first), 1, &[]));
    assert!(beat(&core, 2, ("node-a", second), 1, &[]));
    assert!(!beat(&core, 3, ("node-b", second), 1, &[]));
}

/// Catches: a `Start` that names a heartbeat older than the newest the stream has taken
/// (the host would act on it within the wrong window), or the stream of an earlier session.
#[test]
fn a_start_names_the_newest_heartbeat_of_the_newest_stream() {
    let core = core();
    let first = up(&core, 0, "node-a", "d1");
    let second = up(&core, 1, "node-a", "d1");
    beat(&core, 2, ("node-a", second), 5, &[]);
    beat(&core, 3, ("node-a", second), 3, &[]);
    beat(&core, 3, ("node-a", first), 9, &[]);
    submit(&core, 4, 1);
    let start = calls(&core)
        .into_iter()
        .find(|call| matches!(call, Call::Start { .. }));
    assert_eq!(
        start,
        Some(Call::Start {
            lease: lease(1, 0),
            worker: worker("node-a"),
            stream: Some(second),
            beat: 5,
        })
    );
}

/// Catches: a registration that proves the earlier process stopped on less than the new
/// daemon naming it and its stream having closed, or that does not when both hold; and a
/// restart of the same process that is not told from a second daemon.
#[test]
fn a_registration_proves_the_earlier_process_stopped_only_when_it_has_closed() {
    // (the new instance, the predecessor it names, whether the old stream closed first,
    // whether the lease it left out of its first heartbeat is lost at once)
    let cases = [
        ("d2", "d1", true, true),
        ("d2", "d1", false, false),
        ("d2", "other", true, false),
        ("d2", "", true, false),
        ("d1", "", false, true),
    ];
    for (instance, predecessor, closed, lost) in cases {
        let core = core();
        let old = up(&core, 0, "node-a", "d1");
        submit(&core, 1, 1);
        if closed {
            close(&core, "node-a", old);
        }
        let new = up_after(&core, 2, "node-a", (instance, predecessor), reported(8));
        beat(&core, 3, ("node-a", new), 1, &[]);
        assert_eq!(
            requeued(&core),
            lost,
            "{instance} after {predecessor}, closed {closed}"
        );
    }
}

/// Catches: a resent report applied to a replaced stream, or one that does not reach the
/// scheduler (the phase of a quarantined host is taken either way).
#[test]
fn a_resent_report_changes_the_newest_stream_only() {
    let core = core();
    let first = up(&core, 0, "node-a", "d1");
    let second = up(&core, 1, "node-a", "d1");
    let resent = reported(8).in_phase(NodePhase::Quarantined("cleanup failed".to_owned()));
    resend(&core, 2, ("node-a", first), resent.clone()).expect("admitted");
    assert_eq!(
        core.with(|_, rig| rig.resized),
        0,
        "a replaced stream is ignored"
    );
    resend(&core, 3, ("node-a", second), resent).expect("admitted");
    assert_eq!(core.with(|_, rig| rig.resized), 1);
    assert!(calls(&core).contains(&Call::Phased {
        worker: worker("node-a"),
        before: NodePhase::Serving,
        after: NodePhase::Quarantined("cleanup failed".to_owned()),
    }));
    submit(&core, 4, 1);
    assert_eq!(started(&core), [], "a quarantined host is offered no lease");
}

/// The time, in milliseconds, after which a worker silent since the start has lost its leases.
fn silent() -> u64 {
    u64::try_from(LEASE_GRACE.as_millis()).expect("small") + 1
}

/// Lease 1.0, given in silence to a worker on `stream` and given up, is granted again as
/// 1.1 by the worker's next heartbeat.
fn lease_replaced(core: &Core<Rig>, stream: Stream) {
    block_on(core.tick(at(silent())));
    beat(core, silent() + 1, ("node-a", stream), 1, &[]);
}

/// Catches: a switching host whose lease the scheduler holds not getting its `Start` again
/// on the new session, and one naming a lease the scheduler does not hold there not being
/// told to stop it.
#[test]
fn a_switching_lease_is_started_again_or_cancelled() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    let held = lease(1, 0);
    let switching = reported(8).in_phase(NodePhase::Preparing(held));
    let new = up_after(&core, 2, "node-a", ("d1", ""), switching);
    let starts: Vec<Option<Stream>> = calls(&core)
        .into_iter()
        .filter_map(|call| match call {
            Call::Start { stream, .. } => Some(stream),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[1],
        Some(new),
        "the second Start is for the new session"
    );

    // Once the lease is given up, a host that still names it in a switch is told to stop it.
    resend(&core, 3, ("node-a", new), reported(8)).expect("admitted");
    lease_replaced(&core, new);
    let restoring = reported(8).in_phase(NodePhase::Restoring(held));
    up_after(&core, silent() + 2, "node-a", ("d1", ""), restoring);
    assert!(calls(&core).contains(&Call::Cancel(worker("node-a"), held)));
}

/// Catches: a lease a host lists that the scheduler no longer holds, because it was given up
/// and granted again, not being told to stop.
#[test]
fn a_heartbeat_listing_a_lease_the_scheduler_does_not_hold_cancels_it() {
    let core = core();
    let stream = up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    lease_replaced(&core, stream);
    assert_eq!(started(&core), [lease(1, 0), lease(1, 1)]);
    let listed = [lease(1, 0), lease(1, 1)];
    assert!(beat(&core, silent() + 2, ("node-a", stream), 2, &listed));
    let cancelled: Vec<Call> = calls(&core)
        .into_iter()
        .filter(|call| matches!(call, Call::Cancel(..)))
        .collect();
    assert_eq!(cancelled, [Call::Cancel(worker("node-a"), lease(1, 0))]);
}

/// Catches: a heartbeat that carries the node's load being refused or its load dropped
/// before the scheduler.
#[test]
fn a_heartbeat_with_a_load_is_taken() {
    let core = core();
    let stream = up(&core, 0, "node-a", "d1");
    let load = NodeLoad {
        seq: 1,
        ..NodeLoad::default()
    };
    let beat = Beat::new(1, vec![]).with_load(load.clone());
    assert_eq!(beat.load, Some(load));
    let taken = block_on(core.heartbeat(at(1), &worker("node-a"), stream, beat));
    assert_eq!(taken, Ok(true));
}

/// Catches: a core that stays locked for ever once code run under the lock panicked.
#[test]
fn a_panic_under_the_lock_does_not_wedge_the_core() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        core.with(|_, _| panic!("a closure that panics under the lock"));
    }));
    assert!(outcome.is_err());
    assert_eq!(core.sessions(), 1);
    submit(&core, 1, 1);
}
