//! Several frontends over one state store: a write that finds another frontend's work in its
//! way, a request that joins an operation another frontend holds, the daemons of the other
//! frontends, and an operator's action on a node another frontend holds (the farm halves of the
//! farm core's results and placement tests; the poll of the store is not in this core yet).

use std::time::Duration;

use js_sched::Cordon;
use js_server_core::{
    Held, NodeAction, NodeUnread, PlaceError, Stage, Standing, Taken, UnknownNode,
};
use js_store::Progress;
use js_types::sched::usage::LeaseCost;
use js_types::sched::{LeaseGrant, OperationId, Resources};

use crate::common::{
    Call, GIB, Rig, at, block_on, calls, lease, reported, request, started, submit, submit_request,
    up, up_after, worker,
};
use crate::recover::finish;
use crate::stored::Setup;

/// The stages shown to the callers, in order.
fn shown(core: &js_server_core::Core<Rig>) -> Vec<Stage> {
    calls(core)
        .into_iter()
        .filter_map(|call| match call {
            Call::Show(_, stage) => Some(stage),
            _ => None,
        })
        .collect()
}

/// Asserts that a grant which finds the operation taken by another frontend is dropped and
/// undone: the scheduler forgets the operation, its callers are left to the store, and the
/// frontend that won runs it. Catches: two frontends that run one operation, and a scheduler
/// that keeps a lease the store does not hold.
#[test]
fn a_grant_that_loses_to_another_frontend_is_undone_and_its_callers_followed() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    let queued = submit(&a, 1, 1);
    let joined = submit(&b, 2, 1);
    assert!(joined.joined);
    up(&b, 3, "node-b", "d2");
    assert_eq!(started(&b), [lease(1, 1024)]);

    up(&a, 4, "node-a", "d1");

    assert_eq!(started(&a), []);
    assert_eq!(a.queued_operations(), 0, "forgotten");
    assert_eq!(a.dropped_writes(), 1);
    let followed = Call::Followed(queued.operation, vec![queued.waiter]);
    assert!(calls(&a).contains(&followed), "{:?}", calls(&a));
}

/// Asserts that a grant for a node the daemon has left for another frontend is dropped, and
/// that the lease stays as the scheduler made it (nothing in the store says it lost). Catches:
/// a frontend that grants work to a daemon it no longer owns.
#[test]
fn a_grant_for_a_node_that_moved_is_dropped() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    up(&a, 0, "node-a", "d1");
    up(&b, 1, "node-a", "d1");

    submit(&a, 2, 1);

    assert_eq!(started(&a), []);
    assert_eq!(a.dropped_writes(), 1);
    assert_eq!(a.write_behind(), 0);
    assert!(setup.store.rows_of("lease/").is_empty());
}

/// Asserts that a result for a node the daemon has left is dropped, not acknowledged, and its
/// callers are left to the store. Catches: a result written over the new owner's lease.
#[test]
fn a_result_for_a_node_that_moved_is_dropped_and_followed() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    up(&a, 0, "node-a", "d1");
    let admitted = submit(&a, 1, 1);
    up(&b, 2, "node-a", "d1");

    let taken = finish(&a, 3, "node-a", admitted.operation, lease(1, 0));

    assert_eq!(taken, Taken::Dropped);
    assert_eq!(a.dropped_writes(), 1);
    let followed = Call::Followed(admitted.operation, vec![admitted.waiter]);
    assert!(calls(&a).contains(&followed));
    assert_eq!(setup.store.rows_of("lease/").len(), 1, "the lease stands");
}

/// Asserts that a request that joins an operation another frontend leased on its node follows
/// it through the store and never takes the lease up: this scheduler holds nothing, the lease
/// row stands, and the result is read from the store once the other frontend has recorded it.
/// Catches: a joiner that gives up a lease it does not own.
#[test]
fn a_request_joining_an_operation_another_frontend_leased_never_takes_its_lease() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    up(&a, 0, "node-a", "d1");
    let leased = submit(&a, 1, 1);

    let joined = submit(&b, 2, 1);

    assert!(joined.joined);
    assert_eq!(joined.held, Held::Elsewhere(leased.operation));
    assert_eq!(joined.operation, leased.operation);
    assert_eq!(b.queued_operations(), 0);
    assert!(b.with(|view, rig| {
        rig.callers.contains_key(&joined.waiter) && view.sched().state(joined.operation).is_none()
    }));
    assert_eq!(setup.store.rows_of("lease/").len(), 1);
    assert_eq!(
        b.progress(leased.operation).expect("read"),
        Some(Progress::Running)
    );

    finish(&a, 3, "node-a", leased.operation, lease(1, 0));

    assert_eq!(
        b.progress(leased.operation).expect("read"),
        Some(Progress::Ended(b"done 1.0".to_vec()))
    );
}

/// Asserts that a request that joins an operation leased on a node this frontend owns takes
/// the operation up with its lease, as one that joins a queued operation does. Catches: a
/// lease on our own node followed through the store, which no one would ever answer.
#[test]
fn a_request_joining_an_operation_leased_on_a_node_the_frontend_owns_takes_it_up() {
    let setup = Setup::new();
    let b = setup.core(1);
    up(&b, 0, "node-a", "d1");
    let other = setup.rows(7);
    other
        .submit(OperationId(5000), &request(1), &[], None)
        .expect("queued by another writer");
    let grant = LeaseGrant {
        lease: lease(5, 0),
        operation: OperationId(5000),
        worker: worker("node-a"),
        cost: LeaseCost::default(),
    };
    other.granted(&grant).expect("leased on the node");

    let joined = submit(&b, 1, 1);

    assert_eq!(joined.operation, OperationId(5000));
    assert_eq!(joined.held, Held::Here);
    assert_eq!(
        b.standing(&worker("node-a"), lease(5, 0)),
        Ok(Standing::Current(OperationId(5000)))
    );
}

/// The rig's host: a one-core node that cannot run the 4-core request, and a request for it.
fn four_cores(n: u64) -> js_sched::Request {
    let mut big = request(n);
    big.resources = Resources::new(4_000, GIB);
    big
}

/// Asserts that a refusal for want of a daemon is held back while a daemon of another frontend
/// could run the operation, also when the session rows cannot be read, and is made once none
/// could. Catches: work refused that another frontend's daemon would have run.
#[test]
fn a_refusal_waits_while_another_frontends_daemon_could_run_the_operation() {
    let setup = Setup::new();
    let config = js_server_core::Config::new(1).with_unservable_wait(Duration::from_secs(1));
    let a = setup.core_with(&config, Rig::default(), 0);
    let b = setup.core(2);
    up(&b, 0, "node-b", "d2");
    up_after(&a, 0, "node-a", ("d1", ""), reported(1));
    let waits = submit_request(&a, 1, four_cores(1)).expect("queued");

    block_on(a.tick(at(5_000)));
    assert!(
        !calls(&a).contains(&Call::Refuse(waits.operation)),
        "another could run it"
    );
    setup.store.fail_reads(Some("session/"));
    block_on(a.tick(at(20_000)));
    assert!(
        !calls(&a).contains(&Call::Refuse(waits.operation)),
        "the rows are unread"
    );
    setup.store.fail_reads(None);

    let mut huge = request(2);
    huge.resources = Resources::new(1_000_000, GIB);
    let refused = submit_request(&a, 21_000, huge).expect("queued");
    block_on(a.tick(at(30_000)));
    assert!(
        calls(&a).contains(&Call::Refuse(refused.operation)),
        "no one could"
    );
}

/// Asserts that the stage of an operation that waits for a worker is decided again once the
/// daemons of the other frontends are read: it is kept (a drain keeps its stream) while one
/// could run it, an operation that waits later is shown so at once, and a look that fails
/// changes nothing. Catches: callers told to give up on work another frontend will run.
#[test]
fn the_stage_of_a_waiting_operation_follows_the_daemons_of_the_other_frontends() {
    let setup = Setup::new();
    let a = setup.core(1);
    let b = setup.core(2);
    up_after(&a, 0, "node-a", ("d1", ""), reported(1));
    a.refresh_remote();
    let first = submit_request(&a, 1, four_cores(1)).expect("queued");
    let waiting = shown(&a);
    assert!(
        matches!(waiting.last(), Some(Stage::Waiting(_))),
        "{waiting:?}"
    );

    up(&b, 2, "node-b", "d2");
    a.refresh_remote();
    let kept = shown(&a);
    assert!(
        matches!(kept.last(), Some(Stage::WaitingKept(_))),
        "{kept:?}"
    );
    submit_request(&a, 3, four_cores(2)).expect("queued");
    let second = shown(&a);
    assert!(
        matches!(second.last(), Some(Stage::WaitingKept(_))),
        "{second:?}"
    );

    let before = calls(&a).len();
    setup.store.fail_reads(Some("session/"));
    a.refresh_remote();
    assert_eq!(calls(&a).len(), before, "a look that fails changes nothing");
    let _ = first;
}

fn placement(core: &js_server_core::Core<Rig>, node: &str) -> Option<Cordon> {
    core.with(|view, _| view.sched().cordon(&worker(node)).cloned())
}

/// Two frontends over one store, and a node connected to the second; actions are placed
/// through the first or the second.
fn pair() -> (Setup, js_server_core::Core<Rig>, js_server_core::Core<Rig>) {
    let setup = Setup::new();
    let (through, own) = (setup.core(1), setup.core(2));
    up(&own, 0, "node-b", "d2");
    (setup, through, own)
}

/// Asserts that an action on a node the store cannot place is a store error, not "no such
/// node", when the store cannot say who holds it. Catches: a host updater that takes the 404
/// for "no daemon here" and goes on to switch a node that is draining on another frontend.
#[test]
fn an_action_on_a_node_the_store_cannot_place_is_a_store_error() {
    for prefix in ["owner/", ""] {
        let (setup, through, own) = pair();
        setup.store.fail_reads(Some(prefix));
        for action in [
            NodeAction::Cordon,
            NodeAction::Drain(Duration::from_secs(600)),
            NodeAction::Uncordon,
        ] {
            let placed = through.place(at(1), &worker("node-b"), action);
            let refused = placed.expect_err("the store cannot say who holds the node");
            assert_eq!(
                refused,
                PlaceError::Unread(NodeUnread("node-b".to_owned())),
                "{prefix:?} {action:?}"
            );
        }
        setup.store.fail_reads(None);
        assert_eq!(placement(&own, "node-b"), None);
    }
}

/// Asserts that a node no frontend has held is still "no such node" when the store can be
/// read. Catches: a store error for every name.
#[test]
fn an_action_on_a_node_no_frontend_holds_is_still_unknown() {
    let (_setup, through, _own) = pair();

    let refused = through
        .place(at(1), &worker("nobody"), NodeAction::Cordon)
        .expect_err("no frontend holds the node");

    assert_eq!(
        refused,
        PlaceError::Unknown(UnknownNode("nobody".to_owned()))
    );
    assert_eq!(refused.to_string(), "no node nobody has registered");
}

/// Asserts that an action on a node another frontend holds, whose `cordon/` row the store
/// refuses, is a store error and applied nowhere, and that one the store takes is written for
/// the node's frontend to apply (a restart of it finds the node placed). Catches: a drain the
/// store refused answered as placed.
#[test]
fn an_action_on_a_node_another_frontend_holds_goes_through_its_cordon_row() {
    for action in [
        NodeAction::Cordon,
        NodeAction::Drain(Duration::from_secs(600)),
    ] {
        let (setup, through, own) = pair();
        setup.store.fail_writes(Some("cordon/"));
        let refused = through
            .place(at(1), &worker("node-b"), action)
            .expect_err("the store refused the cordon row");
        assert_eq!(
            refused.to_string(),
            "the state store could not be written for node node-b; the action was not applied"
        );
        assert_eq!(placement(&through, "node-b"), None);
        assert_eq!(placement(&own, "node-b"), None);

        setup.store.fail_writes(None);
        through
            .place(at(2), &worker("node-b"), action)
            .expect("placed");
        assert_eq!(setup.store.rows_of("cordon/").len(), 1);
        drop(own);
        let restarted = setup.core_with(&js_server_core::Config::new(2), Rig::default(), 3);
        let placed = placement(&restarted, "node-b");
        match action {
            NodeAction::Cordon => assert_eq!(placed, Some(Cordon::Cordoned)),
            // A drain of a node that holds no lease has ended: nothing is left to wait for.
            _ => assert_eq!(placed, Some(Cordon::Drained)),
        }
    }
}

/// Asserts that an action on a node connected here, whose `cordon/` row the store refuses, is a
/// store error and leaves the scheduler as it was; and that an uncordon whose row delete the
/// store refuses leaves a cordoned node cordoned, while one it takes returns the node to
/// placement and removes the row. Catches: a node returned here while its row still cordons it.
#[test]
fn an_action_on_a_node_connected_here_follows_the_store() {
    let (setup, _through, own) = pair();
    setup.store.fail_writes(Some("cordon/"));
    let refused = own.place(at(1), &worker("node-b"), NodeAction::Cordon);
    assert_eq!(refused, Err(PlaceError::Unwritten("node-b".to_owned())));
    assert_eq!(placement(&own, "node-b"), None);

    setup.store.fail_writes(None);
    own.place(at(2), &worker("node-b"), NodeAction::Cordon)
        .expect("cordoned");
    assert_eq!(placement(&own, "node-b"), Some(Cordon::Cordoned));
    setup.store.fail_writes(Some("cordon/"));
    let kept = own.place(at(3), &worker("node-b"), NodeAction::Uncordon);
    assert_eq!(kept, Err(PlaceError::Unwritten("node-b".to_owned())));
    assert_eq!(placement(&own, "node-b"), Some(Cordon::Cordoned));

    setup.store.fail_writes(None);
    own.place(at(4), &worker("node-b"), NodeAction::Uncordon)
        .expect("returned");
    assert_eq!(placement(&own, "node-b"), None);
    assert!(setup.store.rows_of("cordon/").is_empty());
}
