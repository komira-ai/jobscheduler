//! Several frontends over one state store: a write that finds another frontend's work in its
//! way, a request that joins an operation another frontend holds, the daemons of the other
//! frontends, and an operator's action on a node another frontend holds (the farm halves of the
//! farm core's results and placement tests; the poll of the store is in `intake.rs`).

use std::time::Duration;

use js_sched::Cordon;
use js_server_core::{
    Held, NodeAction, NodeHeld, NodeUnread, PlaceError, Stage, Standing, Taken, UnknownNode,
};
use js_store::Progress;
use js_types::sched::usage::LeaseCost;
use js_types::sched::{LeaseGrant, OperationId, Resources};

use crate::common::{
    Call, GIB, Rig, at, beat, block_on, calls, core, lease, reported, request, shown_to, started,
    submit, submit_request, up, up_after, worker,
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
/// undone: the scheduler forgets the operation, its callers are shown what the store says of
/// it (the result the frontend that won wrote), and the frontend that won runs it. Catches:
/// two frontends that run one operation, a scheduler that keeps a lease the store does not
/// hold, and callers left waiting for a core that will never answer them.
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
    assert_eq!(shown_to(&a, queued.waiter), None, "nothing is known yet");

    a.intake(at(5));
    assert_eq!(
        shown_to(&a, queued.waiter),
        Some(Stage::Executing),
        "the other frontend runs it"
    );
    finish(&b, 6, "node-b", joined.operation, lease(1, 1024));
    a.intake(at(7));
    let ended = Stage::Done(b"done 1.1024".to_vec());
    assert_eq!(shown_to(&a, queued.waiter), Some(ended));
    let followed = Call::Followed(queued.operation, b"done 1.1024".to_vec());
    assert!(calls(&a).contains(&followed), "{:?}", calls(&a));
}

/// Asserts that a grant lost to another frontend is judged by the queue row read after the
/// loss: another frontend took the operation and queued it again meanwhile, so the operation
/// stays queued here with its caller, and is not handed off as if it were gone. Catches: a
/// caller moved to the store's follower while this core still holds the operation.
#[test]
fn a_lost_grant_whose_operation_is_queued_again_keeps_its_caller() {
    let setup = Setup::new();
    let core = setup.core(1);
    let queued = submit(&core, 1, 1);
    let key = js_store::keys::queue(queued.operation);
    let other = setup.rows(7);
    let store = setup.store.clone();
    let taken = LeaseGrant {
        lease: lease(5, 0),
        operation: queued.operation,
        worker: worker("node-b"),
        cost: LeaseCost::default(),
    };
    // The other frontend takes the operation before this core's grant reads its queue row,
    // and queues it again before the look that follows the loss.
    setup.store.before_read("queue/", move || {
        other.granted(&taken).expect("granted by another frontend");
        let again = store.clone();
        store.before_read("queue/", move || again.put(&key, Vec::new()));
    });

    up(&core, 2, "node-a", "d1");

    assert_eq!(core.dropped_writes(), 1, "the grant was not refused");
    assert_eq!(core.queued_operations(), 1);
    let waiters = core.with(|view, _| view.sched().waiters(queued.operation).map(<[_]>::to_vec));
    assert_eq!(waiters, Some(vec![queued.waiter]));
    assert!(
        !calls(&core)
            .iter()
            .any(|call| matches!(call, Call::KeptFinished(..) | Call::Followed(..))),
        "handed off"
    );
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
/// callers are shown the result the node's new frontend writes. Catches: a result written over
/// the new owner's lease, and callers left waiting for it.
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
    assert_eq!(setup.store.rows_of("lease/").len(), 1, "the lease stands");

    finish(&b, 4, "node-a", admitted.operation, lease(1, 0));
    a.intake(at(5));
    let ended = Stage::Done(b"done 1.0".to_vec());
    assert_eq!(shown_to(&a, admitted.waiter), Some(ended));
    let followed = Call::Followed(admitted.operation, b"done 1.0".to_vec());
    assert!(calls(&a).contains(&followed));
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

/// Two frontends over one store, with the first running request 1 on `node-c`, which has room
/// for one, and requests 2 and 3 on `node-a`.
fn running_on_first() -> (Setup, js_server_core::Core<Rig>, js_server_core::Core<Rig>) {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    submit(&a, 1, 1);
    up_after(&a, 2, "node-c", ("d3", ""), reported(1));
    submit(&a, 3, 2);
    submit(&a, 3, 3);
    up(&a, 4, "node-a", "d1");
    assert_eq!(leases_on(&a, "node-c").len(), 1);
    assert_eq!(leases_on(&a, "node-a").len(), 2);
    (setup, a, b)
}

/// The leases the rig was told to start on `name`, in the order started.
fn leases_on(core: &js_server_core::Core<Rig>, name: &str) -> Vec<js_types::sched::LeaseId> {
    let who = worker(name);
    calls(core)
        .into_iter()
        .filter_map(|call| match call {
            Call::Start { lease, worker, .. } if worker == who => Some(lease),
            _ => None,
        })
        .collect()
}

/// Asserts that a frontend asked about a node another frontend owns says so and names the
/// leases the store holds on it, in order, and that the owner calls it its own. Catches: a
/// node view that shows a node of another frontend as absent, or as this one's, or without
/// the work running on it.
#[test]
fn a_node_another_frontend_owns_is_elsewhere_with_its_stored_leases() {
    let (_setup, a, b) = running_on_first();
    let on_a = leases_on(&a, "node-a");
    let on_c = leases_on(&a, "node-c");
    assert!(on_a.is_sorted(), "the leases were granted in order");

    assert_eq!(
        b.node_held(at(5), &worker("node-a")),
        NodeHeld::Elsewhere { leases: on_a },
        "only the leases of the node asked about"
    );
    assert_eq!(
        b.node_held(at(5), &worker("node-c")),
        NodeHeld::Elsewhere { leases: on_c }
    );
    assert_eq!(a.node_held(at(5), &worker("node-a")), NodeHeld::Here);
}

/// Asserts that a node linked here whose claim the store refused, while the store names
/// another frontend as its owner, reads as held elsewhere with that frontend's leases. Catches:
/// a view that calls a node ours because a link is open, while the store says it is not.
#[test]
fn a_link_here_whose_claim_was_refused_reads_as_the_stores_owner_has_it() {
    let (setup, a, b) = running_on_first();
    setup.store.fail_writes(Some("owner/"));
    up(&b, 5, "node-a", "d2");

    assert_eq!(
        b.node_held(at(6), &worker("node-a")),
        NodeHeld::Elsewhere {
            leases: leases_on(&a, "node-a")
        }
    );
}

/// Asserts that a heartbeat is not acknowledged while the owner row cannot be read, and is
/// again once it can. Catches: an owner check that takes an unreadable store for ownership.
#[test]
fn an_unreadable_owner_row_is_not_ownership() {
    let setup = Setup::new();
    let a = setup.core(1);
    let stream = up(&a, 0, "node-a", "d1");
    assert!(beat(&a, 1, ("node-a", stream), 1, &[]));

    setup.store.fail_reads(Some("owner/"));
    assert!(!beat(&a, 60_000, ("node-a", stream), 2, &[]));

    setup.store.fail_reads(None);
    assert!(beat(&a, 60_001, ("node-a", stream), 3, &[]));
}

/// Asserts that a node another frontend owns that holds no unfinished work is elsewhere with
/// no leases. Catches: a node with nothing running shown as unknown.
#[test]
fn an_idle_node_another_frontend_owns_is_elsewhere_without_leases() {
    let (_setup, through, _own) = pair();

    assert_eq!(
        through.node_held(at(1), &worker("node-b")),
        NodeHeld::Elsewhere { leases: Vec::new() }
    );
}

/// Asserts that a node connected here is here, and a name no frontend has held is unknown.
/// Catches: a node of this core looked up as another's, and a store error or a made-up node
/// for a name nobody has registered.
#[test]
fn a_node_connected_here_is_here_and_an_unregistered_one_is_unknown() {
    let (_setup, through, own) = pair();

    assert_eq!(own.node_held(at(1), &worker("node-b")), NodeHeld::Here);
    assert_eq!(
        through.node_held(at(1), &worker("nobody")),
        NodeHeld::Unknown
    );
}

/// Asserts that a node connected here whose claim the store has not taken yet is here, not
/// unknown. Catches: a node view that loses a daemon between its `Hello` and the store coming
/// back.
#[test]
fn a_connected_node_whose_claim_is_pending_is_here() {
    let setup = Setup::new();
    let core = setup.core(1);
    setup.store.fail_writes(Some("owner/"));
    up(&core, 0, "node-a", "d1");

    assert_eq!(core.node_held(at(1), &worker("node-a")), NodeHeld::Here);
}

/// Asserts that a store that cannot be read gives `Unread` at each of the three looks (the
/// owner row of a node this core owns once its cache is old, the owner row of one it does
/// not, the lease rows), not a guess. Catches: an unreadable store shown as an absent node
/// or as no leases.
#[test]
fn a_store_that_cannot_be_read_gives_unread() {
    for prefix in ["owner/", "lease/", ""] {
        let (setup, a, b) = running_on_first();
        setup.store.fail_reads(Some(prefix));
        let held = b.node_held(at(3), &worker("node-a"));
        assert_eq!(held, NodeHeld::Unread, "{prefix:?} from the other frontend");
        if prefix != "lease/" {
            let own = a.node_held(at(60_000), &worker("node-a"));
            assert_eq!(own, NodeHeld::Unread, "{prefix:?} from the owner");
        }
        setup.store.fail_reads(None);
        assert_ne!(b.node_held(at(3), &worker("node-a")), NodeHeld::Unread);
    }
}

/// Asserts that a core with no state store, and a fenced one, hold the nodes connected to
/// them and know no other. Catches: a lone server whose node view reports its own daemons
/// as unknown.
#[test]
fn a_core_without_a_store_holds_what_is_connected_to_it() {
    let lone = core();
    up(&lone, 0, "node-a", "d1");
    assert_eq!(lone.node_held(at(1), &worker("node-a")), NodeHeld::Here);
    assert_eq!(lone.node_held(at(1), &worker("node-b")), NodeHeld::Unknown);

    let (_setup, a, _b) = running_on_first();
    a.fence();
    assert_eq!(a.node_held(at(3), &worker("node-a")), NodeHeld::Here);
    assert_eq!(a.node_held(at(3), &worker("node-z")), NodeHeld::Unknown);
}
