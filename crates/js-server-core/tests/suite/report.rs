//! A host's `Result`: what its lease stands for, how the scheduler takes it, and when it
//! is acknowledged. The tests are new, written against the rules that moved from kbf's farm.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::admission::Deny;
use crate::common::{
    Call, GIB, Gate, Trace, at, beat, block_on, calls, core, core_by, gated, gated_by, lease,
    lines, request, started, submit, submit_request, up, worker,
};
use js_sched::fence::{LEASE_GRACE, START_GRACE};
use js_server_core::{Attempt, Config, Denied, Reason, Report, Standing, Taken};
use js_types::sched::{Failure, OperationId, Outcome, Resources, ResultRef};

fn done() -> Outcome {
    Outcome::Completed {
        action_result: ResultRef::new([7; 32], 1),
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("small")
}

/// Catches: a lease nobody sent being taken for a known one, a `Result` of the wrong node
/// or of a lease given up taken as current, and a current lease refused.
#[test]
fn a_lease_stands_for_its_operation_only_on_the_node_that_holds_it() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    let node_a = worker("node-a");
    assert_eq!(
        core.standing(&node_a, lease(1, 0)),
        Ok(Standing::Current(admitted.operation))
    );
    assert_eq!(
        core.standing(&worker("node-b"), lease(1, 0)),
        Ok(Standing::Refused(Reason::Stale)),
        "another node never held it"
    );
    assert_eq!(
        core.standing(&node_a, lease(1, 7)),
        Ok(Standing::Refused(Reason::UnknownLease))
    );
}

/// Catches: a copy of an answered result taken again, or reported as unknown instead of as
/// a copy; and a report the scheduler does not take answered as taken.
#[test]
fn a_result_after_the_answer_is_a_duplicate_and_is_not_taken_again() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    let node_a = worker("node-a");
    let report = Report {
        operation: admitted.operation,
        lease: lease(1, 0),
        outcome: done(),
    };
    assert_eq!(
        block_on(core.report(at(20), &node_a, report, |_| {})),
        Taken::Accepted
    );
    assert_eq!(
        core.standing(&node_a, lease(1, 0)),
        Ok(Standing::Refused(Reason::Duplicate))
    );
    assert_eq!(
        block_on(core.report(at(21), &node_a, report, |_| {})),
        Taken::Dropped
    );
}

/// Catches: a report naming a lease this process never sent, or one given up and granted
/// again, taken; each is refused for its own reason, and nothing is answered.
#[test]
fn a_report_of_an_unknown_or_replaced_lease_is_refused_with_its_reason() {
    let core = core();
    let stream = up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 0, 1);
    let node_a = worker("node-a");
    let of = |lease| Report {
        operation: admitted.operation,
        lease,
        outcome: done(),
    };
    assert_eq!(
        block_on(core.report(at(5), &node_a, of(lease(1, 9)), |_| {})),
        Taken::Refused(Reason::UnknownLease)
    );

    // The lease is given up in silence and granted again.
    block_on(core.tick(at(millis(LEASE_GRACE) + 1)));
    beat(&core, millis(LEASE_GRACE) + 2, ("node-a", stream), 1, &[]);
    assert_eq!(started(&core), [lease(1, 0), lease(1, 1)]);
    assert_eq!(
        block_on(core.report(
            at(millis(LEASE_GRACE) + 3),
            &node_a,
            of(lease(1, 0)),
            |_| {}
        )),
        Taken::Refused(Reason::Stale)
    );
    assert!(
        calls(&core)
            .iter()
            .all(|call| !matches!(call, Call::Settle { .. }))
    );
}

/// Catches: a result acknowledged while its finish is not written, a copy of it from
/// another node owed the acknowledgement, and the acknowledgement lost once the finish is
/// written: it goes to the node that sent the first.
#[test]
fn a_result_whose_finish_waits_is_acknowledged_to_its_sender_when_it_is_written() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    gate.finishes(Attempt::Transient);
    let held = lease(1, 0);
    let report = Report {
        operation: admitted.operation,
        lease: held,
        outcome: done(),
    };
    let taken = block_on(core.report(at(20), &worker("node-a"), report, |rig| {
        rig.parked.insert(held);
    }));
    assert_eq!(taken, Taken::Held);
    assert!(
        core.with(|_, rig| rig.parked.contains(&held)),
        "kept for its answer"
    );
    assert_eq!(core.standing(&worker("node-a"), held), Ok(Standing::Held));
    assert_eq!(core.standing(&worker("node-b"), held), Ok(Standing::Held));

    // A refused report is parked too; no answer will use it, and the held answer's stays.
    let stray = Report {
        lease: lease(1, 9),
        ..report
    };
    block_on(core.report(at(25), &worker("node-a"), stray, |rig| {
        rig.parked.insert(lease(1, 9));
    }));
    assert_eq!(
        core.with(|_, rig| rig.parked.clone()),
        BTreeSet::from([held])
    );

    gate.finishes(Attempt::Taken);
    block_on(core.tick(at(30)));
    assert_eq!(
        calls(&core).last(),
        Some(&Call::Settle {
            operation: admitted.operation,
            lease: held,
            ack: Some(worker("node-a")),
        })
    );
}

/// Catches: a requeue of one operation taking the held answer of another with it: only the
/// held `Start` of the lease given up goes, and the answer is written and settled later.
#[test]
fn a_held_answer_stays_when_another_lease_is_given_up() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    let stream = up(&core, 0, "node-a", "d1");
    let first = submit(&core, 1, 1);
    submit(&core, 2, 2);
    gate.finishes(Attempt::Transient);
    let report = Report {
        operation: first.operation,
        lease: lease(1, 0),
        outcome: done(),
    };
    block_on(core.report(at(3), &worker("node-a"), report, |_| {}));
    // The worker lists the first lease only: the second is given up once its `Start` has
    // been out for the start grace.
    let grace = millis(START_GRACE);
    beat(&core, 3 + grace, ("node-a", stream), 1, &[lease(1, 0)]);
    assert!(calls(&core).contains(&Call::Requeued(lease(1, 1))));

    gate.finishes(Attempt::Taken);
    block_on(core.tick(at(4 + grace)));
    let settled = |call: &&Call| matches!(call, Call::Settle { lease: l, .. } if *l == lease(1, 0));
    assert_eq!(calls(&core).iter().filter(settled).count(), 1);
}

/// Catches: a result whose finish the writer refuses for good being acknowledged (the
/// daemon would forget it), or its parked report kept for ever.
#[test]
fn a_result_whose_finish_is_dropped_is_never_acknowledged() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    gate.finishes(Attempt::Dropped);
    let held = lease(1, 0);
    let report = Report {
        operation: admitted.operation,
        lease: held,
        outcome: done(),
    };
    let taken = block_on(core.report(at(20), &worker("node-a"), report, |rig| {
        rig.parked.insert(held);
    }));
    assert_eq!(taken, Taken::Dropped);
    assert_eq!(core.dropped_writes(), 1);
    assert!(calls(&core).contains(&Call::ForgotParcel(held)));
    assert!(core.with(|_, rig| rig.parked.is_empty()));
    assert_eq!(
        core.standing(&worker("node-a"), held),
        Ok(Standing::Refused(Reason::Duplicate))
    );
}

/// Catches: a parked report kept when its result is refused (no answer will ever use it).
#[test]
fn a_parked_report_no_answer_uses_is_dropped() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    let report = Report {
        operation: admitted.operation,
        lease: lease(1, 9),
        outcome: done(),
    };
    block_on(core.report(at(20), &worker("node-a"), report, |rig| {
        rig.parked.insert(lease(1, 9));
    }));
    assert!(core.with(|_, rig| rig.parked.is_empty()));
}

/// Catches: a finish found among other held effects by the wrong test (a start, a refusal,
/// an answer): the result of a lease whose answer waits is held whatever else waits.
#[test]
fn a_held_finish_is_found_among_other_held_effects() {
    let trace = Trace::default();
    let (writer, gate) = Gate::new(&trace);
    let config = Config::new(1).with_unservable_wait(Duration::from_secs(1));
    let core = gated_by(&config, &trace, writer);
    up(&core, 0, "node-a", "d1");
    let first = submit(&core, 0, 1);
    // Work no worker is big enough for, which the scheduler refuses once it has waited.
    let mut huge = request(2);
    huge.resources = Resources::new(1_000_000, GIB);
    submit_request(&core, 1, huge).expect("queued");

    // Held behind each other, oldest first: a grant, a refusal, and the answer of the
    // first operation.
    gate.all(Attempt::Transient);
    submit(&core, 2, 3);
    block_on(core.tick(at(5_000)));
    let report = Report {
        operation: first.operation,
        lease: lease(1, 0),
        outcome: done(),
    };
    let taken = block_on(core.report(at(5_001), &worker("node-a"), report, |_| {}));
    assert_eq!(taken, Taken::Held);
    let node_a = worker("node-a");
    assert_eq!(core.standing(&node_a, lease(1, 0)), Ok(Standing::Held));
    assert!(!calls(&core).contains(&Call::Refuse(OperationId(1))));

    gate.all(Attempt::Taken);
    block_on(core.tick(at(5_002)));
    let tail: Vec<String> = lines(&trace)
        .into_iter()
        .skip_while(|l| l != "write start 1.1 Taken")
        .collect();
    // Every held row is written, in order, before the first of their effects happens.
    assert_eq!(
        tail,
        [
            "write start 1.1 Taken",
            "write refuse op-1 Taken",
            "write answer 1.0 Taken",
            "start 1.1",
            "settle 1.0",
            "deliver 0"
        ]
    );
    assert!(calls(&core).contains(&Call::Refuse(OperationId(1))));
}

/// Catches: a memory kill or a lost run the scheduler runs again being refused or
/// answered: it is accepted, nothing is answered, and the operation runs again.
#[test]
fn a_kill_or_a_fault_the_scheduler_runs_again_is_accepted() {
    for failure in [Failure::OutOfMemory, Failure::Infra] {
        let core = core();
        let stream = up(&core, 0, "node-a", "d1");
        let admitted = submit(&core, 10, 1);
        let report = Report {
            operation: admitted.operation,
            lease: lease(1, 0),
            outcome: Outcome::Failed(failure),
        };
        let taken = block_on(core.report(at(20), &worker("node-a"), report, |_| {}));
        assert_eq!(taken, Taken::Accepted, "{failure:?}");
        beat(&core, 30, ("node-a", stream), 1, &[]);
        assert!(started(&core).len() >= 2, "{failure:?}: run again");
        assert!(
            calls(&core)
                .iter()
                .all(|call| !matches!(call, Call::Settle { .. }))
        );
    }
}

/// Catches: a result answered before its finish is written, an answer delivered under the
/// lock (before the hooks of the same feed are done), and a parked report that outlives its
/// answer.
#[test]
fn an_answer_is_settled_after_its_rows_and_delivered_last() {
    let trace = Trace::default();
    let (writer, _gate) = Gate::new(&trace);
    let core = gated(&trace, writer);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    let held = lease(1, 0);
    let report = Report {
        operation: admitted.operation,
        lease: held,
        outcome: done(),
    };
    let taken = block_on(core.report(at(20), &worker("node-a"), report, |rig| {
        rig.parked.insert(held);
    }));
    assert_eq!(taken, Taken::Accepted);
    assert_eq!(
        lines(&trace)[3..],
        ["write answer 1.0 Taken", "settle 1.0", "deliver 0"]
    );
    assert!(core.with(|_, rig| rig.parked.is_empty()));
}

/// Catches: a failed operation answered without the failure line, and a lease that
/// finishes failed being taken as accepted on the strength of an earlier result.
#[test]
fn a_failed_run_is_answered_as_failed() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 10, 1);
    let report = Report {
        operation: admitted.operation,
        lease: lease(1, 0),
        outcome: Outcome::Failed(Failure::Invalid),
    };
    let taken = block_on(core.report(at(20), &worker("node-a"), report, |_| {}));
    assert_eq!(taken, Taken::Accepted);
    assert_eq!(
        core.with(|_, rig| rig.delivered.lock().expect("delivered").clone()),
        [admitted.operation]
    );
}

/// Catches: a result taken on a core that was handed over (it must feed the scheduler
/// nothing), and a refusal that is not the stale one.
#[test]
fn a_result_on_a_fenced_core_is_refused_as_stale() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 1, 1);
    core.fence();
    let report = Report {
        operation: admitted.operation,
        lease: lease(1, 0),
        outcome: Outcome::Failed(Failure::Invalid),
    };
    let taken = block_on(core.report(at(5), &worker("node-a"), report, |_| {}));
    assert_eq!(taken, Taken::Refused(Reason::Stale));
    assert_eq!(
        core.standing(&worker("node-a"), lease(1, 0)),
        Ok(Standing::Current(admitted.operation))
    );
}

/// Catches: a host the admission refuses reporting a result, and a host it allows judged on
/// anything but its lease.
#[test]
fn a_result_is_heard_only_from_a_host_the_admission_allows() {
    let core = core().with_admission(Deny("node-x"));
    up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    assert_eq!(
        core.standing(&worker("node-x"), lease(1, 0)),
        Err(Denied::new("denied result"))
    );
    assert_eq!(
        core.standing(&worker("node-a"), lease(1, 0)),
        Ok(Standing::Current(OperationId(0)))
    );
}

/// Catches: the "lease finished" line naming the operation after the report has finished it
/// (with no finished retention the scheduler has dropped it by then, and its callers with it):
/// the name is asked for before the report is fed.
#[test]
fn the_line_of_a_finishing_report_names_the_operation_it_finished() {
    let config = Config::new(1).with_finished_retention(Duration::ZERO);
    let core = core_by(&config);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 1, 1);
    let report = Report {
        operation: admitted.operation,
        lease: lease(1, 0),
        outcome: done(),
    };
    let taken = block_on(core.report(at(2), &worker("node-a"), report, |_| {}));
    assert_eq!(taken, Taken::Accepted);
    let named = core.with(|_, rig| rig.named.lock().expect("named").clone());
    assert_eq!(named, ["operations/0"]);
}
