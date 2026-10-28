//! A core over a state store: what it takes up at a start, what it writes before it acts, and
//! what it can say of the store (rows moved from the farm core's tests of the same rules, with
//! the store's rows read from the store itself).

use js_server_core::{Config, Core, Held, NotQueued, Reason, Report, Stage, Standing, Taken};
use js_store::{EpochBump, EpochPolicy, Progress};
use js_types::sched::{LeaseId, OperationId, Outcome, ResultRef, WaiterId};

use crate::common::{
    Call, Rig, at, beat, block_on, calls, core, lease, reported, request, shown_to, started,
    submit, submit_request, up, up_after, worker,
};
use crate::stored::Setup;

fn done() -> Outcome {
    Outcome::Completed {
        action_result: ResultRef::new([7; 32], 1),
    }
}

/// The result of `operation`'s `lease`, reported by `node` at `now`.
pub fn finish(
    core: &Core<Rig>,
    now: u64,
    node: &str,
    operation: OperationId,
    lease: LeaseId,
) -> Taken {
    let report = Report {
        operation,
        lease,
        outcome: done(),
    };
    block_on(core.report(at(now), &worker(node), report, |_| {}))
}

/// The parcel `submit` keeps with the operation of caller `n`: the time it was submitted at.
fn parcel(n: u64) -> Option<Vec<u8>> {
    Some(n.to_be_bytes().to_vec())
}

/// Asserts that a core started over a store holding a leased and a queued operation takes up
/// the queued one for a caller of its own that is told of the parcel the first process kept,
/// takes the leased one up only when its node says `Hello` (then holding the lease for it, for
/// a caller of its own), and numbers new operations and leases past the blocks every earlier
/// start reserved. Catches: a restart that forgets an operation, one that numbers from zero
/// again (two operations of one id), one that loses the parcel, and one that takes up a lease
/// whose node may be connected to another frontend.
#[test]
fn a_start_takes_up_the_operations_the_store_holds_and_numbers_past_them() {
    let setup = Setup::new();
    let first = setup.core(1);
    up_after(&first, 0, "node-a", ("d1", ""), reported(1));
    let leased = submit(&first, 1, 1);
    let queued = submit(&first, 2, 2);
    assert_eq!(
        (leased.operation, queued.operation),
        (OperationId(0), OperationId(1))
    );
    drop(first);

    let second = setup.core_with(&Config::new(1), Rig::default(), 5_000);

    assert_eq!(
        calls(&second),
        [Call::Adopted(OperationId(1), WaiterId(0), parcel(2))]
    );
    assert_eq!(second.epoch(), Some(1));
    assert_eq!(second.queued_operations(), 1);
    let held = |core: &Core<Rig>| core.with(|view, _| view.sched().leases_on(&worker("node-a")));
    assert_eq!(held(&second), [], "the lease waits for its node");

    up_after(&second, 5_000, "node-a", ("d1", ""), reported(1));
    assert_eq!(
        calls(&second)[1],
        Call::Adopted(OperationId(0), WaiterId(1), parcel(1))
    );
    assert_eq!(held(&second), [lease(1, 0)]);
    // A new operation and a new lease are numbered in blocks past the first start's.
    up(&second, 6_000, "node-b", "d2");
    let admitted = submit(&second, 6_001, 3);
    assert_eq!(admitted.operation, OperationId(1024));
    assert_eq!(started(&second), [lease(1, 1024), lease(1, 1025)]);
}

/// Asserts that a result for a lease an earlier process granted is taken once the node
/// registers and lists the lease, and that the operation then reads as ended in the store,
/// with the value the adapter gave. Catches: a lease of an earlier process refused as unknown,
/// and a finish that leaves the operation's rows or loses its `done/` row.
#[test]
fn a_result_for_a_lease_an_earlier_process_granted_is_taken() {
    let setup = Setup::new();
    let first = setup.core(1);
    up(&first, 0, "node-a", "d1");
    let leased = submit(&first, 1, 1);
    assert_eq!(
        first.progress(leased.operation).expect("read"),
        Some(Progress::Running)
    );
    drop(first);

    let second = setup.core_with(&Config::new(1), Rig::default(), 5_000);
    let stream = up(&second, 5_000, "node-a", "d1");
    assert!(beat(&second, 5_001, ("node-a", stream), 1, &[lease(1, 0)]));
    let node_a = worker("node-a");
    assert_eq!(
        second.standing(&node_a, lease(1, 0)),
        Ok(Standing::Current(leased.operation))
    );
    let taken = finish(&second, 5_002, "node-a", leased.operation, lease(1, 0));
    assert_eq!(taken, Taken::Accepted);

    let ended = Some(b"done 1.0".to_vec());
    assert_eq!(second.ended(leased.operation).expect("read"), ended);
    assert_eq!(
        second.progress(leased.operation).expect("read"),
        Some(Progress::Ended(b"done 1.0".to_vec()))
    );
    assert!(setup.store.rows_of("lease/").is_empty());
    assert!(setup.store.rows_of("op/").is_empty());
    assert_eq!(setup.store.rows_of("done/").len(), 1);
    assert_eq!(
        second.standing(&node_a, lease(1, 0)),
        Ok(Standing::Refused(Reason::Duplicate))
    );
}

/// Asserts that a core says where an operation stands in the store: queued, running, or no
/// trace of it. Catches: a progress that reads the scheduler instead of the rows.
#[test]
fn the_store_says_where_an_operation_stands() {
    let setup = Setup::new();
    let core = setup.core(1);
    up_after(&core, 0, "node-a", ("d1", ""), reported(1));
    let running = submit(&core, 1, 1).operation;
    let queued = submit(&core, 2, 2).operation;

    let progress = |operation| core.progress(operation).expect("read");
    assert_eq!(progress(running), Some(Progress::Running));
    assert_eq!(progress(queued), Some(Progress::Queued));
    assert_eq!(progress(OperationId(99)), Some(Progress::Gone));
}

/// Asserts that a core with no store has no epoch, no progress to read and no epoch to
/// bump. Catches: a core that invents a store's answers.
#[test]
fn a_core_with_no_store_has_nothing_to_ask_of_one() {
    let core = core();
    assert_eq!(core.epoch(), None);
    assert!(core.progress(OperationId(0)).expect("no read").is_none());
    assert!(core.ended(OperationId(0)).expect("no read").is_none());
    assert!(core.bump_epoch().expect("no write").is_none());
    assert_eq!(core.write_behind(), 0);
    assert!(!core.store_down());
}

/// Asserts that an operator's bump raises the epoch the store holds and not the epoch of the
/// core that runs, and that a core started after it serves the new one: its leases carry
/// it. And that a start may take the next epoch itself. Catches: a bump that restarts the
/// running core's term, and a start that ignores the policy.
#[test]
fn a_bump_of_the_epoch_is_served_by_the_next_start() {
    let setup = Setup::new();
    let running = setup.core(1);
    assert_eq!(running.epoch(), Some(1));

    let bumped = running.bump_epoch().expect("bumped");

    assert_eq!(
        bumped,
        Some(EpochBump {
            previous: 1,
            epoch: 2
        })
    );
    assert_eq!(running.epoch(), Some(1));
    drop(running);
    let next = setup.core(1);
    assert_eq!(next.epoch(), Some(2));
    up(&next, 0, "node-a", "d1");
    submit(&next, 1, 1);
    assert_eq!(started(&next), [lease(2, 1024)]);
    drop(next);
    let storage = setup.storage().with_epoch(EpochPolicy::Bump);
    let third = Core::open(&Config::new(1), Rig::default(), storage, at(0)).expect("opened");
    assert_eq!(third.epoch(), Some(3));
}

/// Asserts that a request the store cannot take is not queued, and that the client is told
/// so: the scheduler holds nothing of it. Catches: a request acknowledged and lost.
#[test]
fn a_request_the_store_does_not_take_is_not_queued() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    setup.store.set_down(true);

    let refused = submit_request(&core, 1, request(1));

    assert_eq!(refused, Err(NotQueued::Unavailable));
    assert_eq!(core.queued_operations(), 0);
    assert_eq!(
        refused.expect_err("not queued").to_string(),
        "the scheduler's state store did not take the request"
    );
    setup.store.set_down(false);
    assert_eq!(submit(&core, 2, 1).operation, OperationId(0));
}

/// Asserts that a request for work whose operation the store holds, and this core does not,
/// joins that operation instead of queueing the work again, that the operation is taken up
/// for a caller of its own, and that the core grants it. Catches: a second operation of the
/// same work queued beside the first.
#[test]
fn a_request_joins_a_queued_operation_another_frontend_wrote() {
    let setup = Setup::new();
    let core = setup.core(1);
    let other = setup.core(2);
    let queued = submit(&other, 1, 1);

    let admitted = submit(&core, 5, 1);

    assert!(admitted.joined);
    assert_eq!(admitted.operation, queued.operation);
    let adopted = Call::Adopted(queued.operation, WaiterId(1), parcel(1));
    assert!(calls(&core).contains(&adopted), "{:?}", calls(&core));
    assert_eq!(
        setup.store.rows_of("op/").len(),
        1,
        "one operation of the work"
    );
    up(&core, 6, "node-a", "d1");
    assert_eq!(started(&core), [lease(1, 0)]);
}

/// Asserts that a start over a store another frontend still runs does not take up that
/// frontend's leases: the scheduler holds none of them, so none is given up after the lease
/// grace, the lease row and its operation's rows stand, and no loss is counted. A request for
/// the work follows the operation through the store. This replaces the test that pinned the
/// start taking every lease up, which ran an operation twice once the grace had passed.
/// Catches: a start that gives up the leases of a frontend that is still running them.
#[test]
fn a_start_over_a_store_another_frontend_still_runs_follows_its_leases() {
    let setup = Setup::new();
    let running = setup.core(1);
    up(&running, 0, "node-a", "d1");
    let leased = submit(&running, 1, 1);

    let second = setup.core_with(&Config::new(2), Rig::default(), 0);

    let held = second.with(|view, _| view.sched().leases_on(&worker("node-a")).len());
    assert_eq!(held, 0, "the other frontend's lease is not taken up");
    assert_eq!(calls(&second), [], "and no caller is adopted for it");
    block_on(second.tick(at(60_001)));
    second.intake(at(60_002));
    assert!(
        !calls(&second)
            .iter()
            .any(|call| matches!(call, Call::Requeued(_))),
        "nothing is given up after the grace: {:?}",
        calls(&second)
    );
    assert_eq!(setup.store.rows_of("loss/").len(), 0);
    assert_eq!(setup.store.rows_of("lease/").len(), 1, "the lease stands");

    let joined = submit(&second, 60_003, 1);
    assert_eq!(joined.held, Held::Elsewhere(leased.operation));
    finish(&running, 60_004, "node-a", leased.operation, lease(1, 0));
    second.intake(at(60_005));
    let ended = Some(Stage::Done(b"done 1.0".to_vec()));
    assert_eq!(shown_to(&second, joined.waiter), ended);
}
