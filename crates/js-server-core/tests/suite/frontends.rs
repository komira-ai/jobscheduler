//! Several frontends over one store, driven through real cores: the poll's memory of the
//! queue rows it read, two cores that take in one operation, a frontend that dies with its
//! leases, and the write a node's new frontend refuses.

use std::time::Duration;

use js_sched::fence::LEASE_GRACE;
use js_sched::{Requeue, RequeueReason};
use js_server_core::{Core, Stage, Taken};
use js_store::ORPHAN_WAIT;
use js_types::sched::usage::LeaseCost;
use js_types::sched::{LeaseGrant, OperationId};

use crate::common::{
    Call, Rig, at, block_on, calls, lease, reported, request, shown_to, started, submit, up,
    up_after, worker,
};
use crate::recover::finish;
use crate::stored::Setup;

/// The callers the scheduler holds `operation` for.
fn waiters(core: &Core<Rig>, operation: OperationId) -> Option<Vec<js_types::sched::WaiterId>> {
    core.with(|view, _| view.sched().waiters(operation).map(<[_]>::to_vec))
}

/// How many times the core was told of a caller it adopted for a stored operation.
fn adoptions(core: &Core<Rig>) -> usize {
    let adopted = |call: &Call| matches!(call, Call::Adopted(..));
    calls(core).iter().filter(|call| adopted(call)).count()
}

/// How many times the core settled `operation`, answering its callers itself.
fn settled(core: &Core<Rig>, operation: OperationId) -> usize {
    let own = |call: &Call| matches!(call, Call::Settle { operation: o, .. } if *o == operation);
    calls(core).iter().filter(|call| own(call)).count()
}

/// How many times the core was told another frontend's run of `operation` ended.
fn followed(core: &Core<Rig>, operation: OperationId) -> usize {
    let ended = |call: &Call| matches!(call, Call::Followed(o, _) if *o == operation);
    calls(core).iter().filter(|call| ended(call)).count()
}

/// Asserts that a poll does not mistake for another frontend's rewrite the queue row of an
/// operation it handed off and a request then joined. The other frontend granted the
/// operation (this core handed it off and forgot the version it read), then gave the lease
/// up, which wrote the queue row again; a request joined that operation through the store
/// before the next poll, so this core holds it for the request's caller. The next poll
/// finds a queue row it has not read before, and must keep the caller: nothing is handed
/// off, and the operation is not taken in a second time. Catches: a poll that remembers
/// the version of a queue row after the operation left the queue, and so moves the caller
/// of a request it just joined to the store's follower and adopts the operation again.
#[test]
fn a_request_joining_an_operation_queued_again_after_a_hand_off_keeps_its_caller() {
    let setup = Setup::new();
    let core = setup.core(1);
    let other = setup.rows(7);
    other
        .submit(OperationId(5000), &request(1), &[7], None)
        .expect("queued by another frontend");
    core.intake(at(1));
    assert_eq!(core.queued_operations(), 1, "taken in");

    // Another frontend grants it: the row is gone, and this core hands the operation off.
    let grant = LeaseGrant {
        lease: lease(5, 0),
        operation: OperationId(5000),
        worker: worker("node-z"),
        cost: LeaseCost::default(),
    };
    other.granted(&grant).expect("granted elsewhere");
    core.intake(at(2));
    assert_eq!(core.queued_operations(), 0, "handed off");

    // It gives the lease up: the queue row is a newer write than the one this core read.
    let gave_up = Requeue {
        operation: OperationId(5000),
        lease: lease(5, 0),
        worker: worker("node-z"),
        reason: RequeueReason::Silent,
    };
    let none = js_sched::FloorChange::default();
    other
        .requeued(&gave_up, Some(1), None, &none, None)
        .expect("given up elsewhere");

    // A request for the same work joins it through the store, before the next poll.
    let joined = submit(&core, 3, 1);
    assert!(joined.joined);
    assert_eq!(joined.operation, OperationId(5000));
    assert_eq!(core.queued_operations(), 1);
    let before = waiters(&core, joined.operation).expect("held");
    assert!(before.contains(&joined.waiter));
    let adopted = adoptions(&core);

    core.intake(at(4));

    assert_eq!(core.queued_operations(), 1);
    assert_eq!(waiters(&core, joined.operation), Some(before));
    assert_eq!(adoptions(&core), adopted, "taken in a second time");
    let moved = |call: &Call| matches!(call, Call::KeptFinished(..) | Call::Followed(..));
    assert!(!calls(&core).iter().any(moved), "handed off");
}

/// Asserts that two cores over one store, both of which took in one queued operation before
/// either placed it, run it once: the first to place it holds the lease, the second loses
/// the grant and follows the operation, a caller of each is answered once, and one result
/// row is written. Catches: an operation run on two nodes, a result taken twice, and the
/// caller of the core that lost the race left waiting.
#[test]
fn two_cores_that_both_take_in_one_operation_run_it_once_and_answer_each_caller_once() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    up(&a, 0, "node-a", "d1");
    up(&b, 0, "node-b", "d2");
    setup
        .rows(7)
        .submit(OperationId(5000), &request(1), &[7], None)
        .expect("queued by another frontend");

    // Both polls find the queued operation before either places it.
    a.intake(at(1));
    b.intake(at(1));
    assert_eq!(a.queued_operations(), 1);
    assert_eq!(b.queued_operations(), 1);
    let (caller_a, caller_b) = (submit(&a, 2, 1), submit(&b, 2, 1));
    assert!(caller_a.joined && caller_b.joined);

    block_on(a.tick(at(3)));
    block_on(b.tick(at(3)));

    assert_eq!(started(&a), [lease(1, 0)]);
    assert_eq!(started(&b), [], "the lease is one core's");
    assert_eq!(b.dropped_writes(), 1, "the second grant was refused");
    assert_eq!(a.dropped_writes(), 0);
    assert_eq!(b.queued_operations(), 0, "the loser follows");
    assert_eq!(setup.store.rows_of("lease/").len(), 1);
    b.intake(at(4));
    assert_eq!(shown_to(&b, caller_b.waiter), Some(Stage::Executing));

    let taken = finish(&a, 5, "node-a", OperationId(5000), lease(1, 0));
    assert_eq!(taken, Taken::Accepted);
    b.intake(at(6));

    let ended = Stage::Done(b"done 1.0".to_vec());
    assert_eq!(shown_to(&b, caller_b.waiter), Some(ended));
    assert_eq!(followed(&b, OperationId(5000)), 1);
    assert_eq!(settled(&b, OperationId(5000)), 0, "the loser answered");
    assert_eq!(settled(&a, OperationId(5000)), 1);
    assert_eq!(followed(&a, OperationId(5000)), 0);
    assert_eq!(setup.store.rows_of("done/").len(), 1);
    assert!(setup.store.rows_of("lease/").is_empty());
    b.intake(at(7));
    assert_eq!(followed(&b, OperationId(5000)), 1, "answered once");
}

/// Asserts, with two real cores, that when the frontend holding a lease dies (its daemon's
/// session row expires with nobody renewing it) the other frontend leaves the lease while the
/// row is there, and after it expires until the orphan wait has passed, then requeues it,
/// takes it in with the loss counted, and runs it on its own node. Catches: a lease taken
/// from a frontend whose session is alive, one taken before the wait, and one that is never
/// taken once its frontend is gone.
#[test]
fn a_frontend_that_dies_with_leases_has_them_run_by_the_other_after_the_orphan_wait() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    up_after(&a, 0, "node-a", ("d1", ""), reported(1));
    up_after(&b, 0, "node-b", ("d2", ""), reported(1));
    let leased = submit(&a, 1, 1);
    assert_eq!(started(&a), [lease(1, 0)]);
    let rows = |setup: &Setup| {
        (
            setup.store.rows_of("lease/").len(),
            setup.store.rows_of("queue/").len(),
        )
    };
    assert_eq!(rows(&setup), (1, 0));

    // While the session row stands, the lease is the other frontend's to leave alone.
    setup
        .store
        .advance(LEASE_GRACE - Duration::from_millis(100));
    b.intake(at(2));
    assert_eq!(setup.store.rows_of("session/").len(), 2, "A's row stands");
    drop(a);

    // The row expires; this poll is the first that finds the lease without a session.
    setup.store.advance(Duration::from_millis(100));
    b.intake(at(3));
    assert_eq!(setup.store.rows_of("session/").len(), 0, "the rows expired");
    setup.store.advance(ORPHAN_WAIT - Duration::from_millis(1));
    b.intake(at(4));
    block_on(b.tick(at(5)));
    assert_eq!(rows(&setup), (1, 0), "left until the wait has passed");
    assert_eq!(b.queued_operations(), 0);
    assert_eq!(started(&b), []);

    setup.store.advance(Duration::from_millis(1));
    b.intake(at(6));
    assert_eq!(rows(&setup), (0, 1), "requeued");
    b.intake(at(7));
    let losses = b.with(|view, _| view.sched().losses(leased.operation));
    assert_eq!(losses, Some(1), "counted as a lost lease");
    up_after(&b, 8, "node-b", ("d2", ""), reported(1));
    block_on(b.tick(at(9)));
    assert_eq!(started(&b).len(), 1, "run by the other frontend");
    let lease_b = started(&b)[0];
    assert_ne!(lease_b, lease(1, 0), "a lease id no earlier grant used");
    assert_eq!(rows(&setup), (1, 0));

    let taken = finish(&b, 10, "node-b", leased.operation, lease_b);
    assert_eq!(taken, Taken::Accepted);
    assert_eq!(setup.store.rows_of("done/").len(), 1);
    assert_eq!(rows(&setup), (0, 0));
}

/// Asserts that a grant for a node whose daemon moved to another frontend is dropped as one
/// for a node that frontend owns, not as a stale one: the scheduler keeps the lease it made,
/// since nothing in the store says the grant lost, and does not queue the operation again or
/// hand its caller to the store's follower. Catches: a refusal for a moved node taken for a
/// lost race, which takes the placement back while the node's new frontend may own the
/// operation, and answers the caller from the store.
#[test]
fn a_grant_for_a_node_that_moved_keeps_the_lease_the_scheduler_made() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    up(&a, 0, "node-a", "d1");
    up(&b, 1, "node-a", "d1");

    let admitted = submit(&a, 2, 1);

    assert_eq!(a.dropped_writes(), 1);
    assert_eq!(started(&a), [], "no start for a lease no row names");
    let node = worker("node-a");
    let held = a.with(|view, _| view.sched().leases_on(&node));
    assert_eq!(held, [lease(1, 0)], "the lease stays");
    assert_eq!(a.queued_operations(), 0, "not queued again");
    assert_eq!(waiters(&a, admitted.operation), Some(vec![admitted.waiter]));
    let moved = |call: &Call| matches!(call, Call::KeptFinished(..) | Call::Followed(..));
    assert!(!calls(&a).iter().any(moved), "handed to the follower");
    assert_eq!(
        setup.store.rows_of("queue/").len(),
        1,
        "the queue row stands"
    );
}
