//! The intake poll: what a core takes in from the other frontends of its state store, forgets
//! because they took it, answers its callers from, and applies of what they set. The other
//! frontend is a handle over the same store, or a write of the test.

use std::time::Duration;

use js_sched::{Cordon, Floor, FloorChange, Requeue, RequeueReason};
use js_server_core::{Config, Core, NodeAction, Stage};
use js_store::{Ending, Rows, StoredCordon};
use js_types::sched::usage::LeaseCost;
use js_types::sched::{LeaseGrant, LeaseId, OperationId, Resources, WorkKey};

use crate::common::{
    Call, GIB, Rig, at, block_on, calls, lease, logged, reported, request, shown_to, started,
    submit, submit_request, up, up_after, worker,
};
use crate::recover::finish;
use crate::stored::Setup;

/// The operations the scheduler holds unfinished.
fn unfinished(core: &Core<Rig>) -> Vec<OperationId> {
    core.with(|view, _| view.sched().unfinished().collect())
}

/// Queues operation `operation` through `rows`, as another frontend does.
fn queue_on(rows: &Rows, operation: u64) {
    let queued = rows.submit(OperationId(operation), &request(1), &[7], None);
    queued.expect("queued by another frontend");
}

/// The lease `(5, seq)` of `operation` on `node`.
fn grant_of(operation: u64, seq: u64, node: &str) -> LeaseGrant {
    LeaseGrant {
        lease: lease(5, seq),
        operation: OperationId(operation),
        worker: worker(node),
        cost: LeaseCost::default(),
    }
}

/// How the other frontend ends an operation: its result, kept for ten minutes.
fn ending(text: &str) -> Ending {
    Ending::new(text.as_bytes().to_vec(), Duration::from_secs(600))
}

fn lease_rows(setup: &Setup) -> usize {
    setup.store.rows_of("lease/").len()
}

fn queue_rows(setup: &Setup) -> usize {
    setup.store.rows_of("queue/").len()
}

fn placement(core: &Core<Rig>, node: &str) -> Option<Cordon> {
    core.with(|view, _| view.sched().cordon(&worker(node)).cloned())
}

/// The retention these tests use for a finished operation.
const RETENTION: Duration = Duration::from_secs(30);

fn retained() -> Config {
    Config::new(1).with_finished_retention(RETENTION)
}

/// Asserts that a core with no state store has nothing to poll: the poll does nothing, counts
/// nothing, and the stored epoch is not ahead. Catches: a poll that invents rows.
#[test]
fn a_core_without_a_state_store_polls_nothing() {
    let core = crate::common::core();

    core.intake(at(1));

    assert_eq!(core.epoch_ahead(), None);
    assert_eq!(core.intake_polls(), 0);
}

/// Asserts that a core a handover fenced takes nothing in (and its poll counts), and takes in
/// what the store holds once it is put back to work. Catches: a fenced core that queues work
/// its successor is to run.
#[test]
fn a_core_that_handed_its_work_over_takes_nothing_in() {
    let setup = Setup::new();
    let core = setup.core(1);
    queue_on(&setup.rows(7), 5000);

    core.fence();
    core.intake(at(1));
    assert!(unfinished(&core).is_empty(), "a fenced core took work in");
    assert_eq!(core.intake_polls(), 1);

    core.resume();
    core.intake(at(2));
    assert_eq!(unfinished(&core), [OperationId(5000)]);
    assert_eq!(core.intake_polls(), 2);
}

/// Asserts that a poll that cannot read a row of the store takes nothing in and does not
/// count, and takes in what is queued when the row is gone. Catches: work taken in on a guess.
#[test]
fn a_poll_that_cannot_read_the_queue_takes_nothing_in() {
    let setup = Setup::new();
    let core = setup.core(1);
    queue_on(&setup.rows(7), 5000);
    setup.store.put("queue/zz", Vec::new());

    core.intake(at(1));
    assert!(unfinished(&core).is_empty(), "work taken in on a guess");
    assert_eq!(
        core.intake_polls(),
        0,
        "a poll that read nothing was counted"
    );

    setup.store.remove("queue/zz");
    core.intake(at(2));
    assert_eq!(unfinished(&core), [OperationId(5000)]);
    assert_eq!(core.intake_polls(), 1);
}

/// Asserts that an operation another frontend queued is taken in once, for a caller of its
/// own that is told of the parcel, with the loss count the store holds of it, and that the
/// ids this core numbers with go past it. Catches: an operation another frontend queued that
/// never runs here, one taken in twice, and one that forgets its losses.
#[test]
fn a_queued_operation_is_taken_in_with_the_rows_the_store_holds() {
    let setup = Setup::new();
    let core = setup.core(1);
    queue_on(&setup.rows(7), 5000);
    setup
        .store
        .put("loss/0000000000001388", 2u64.to_be_bytes().to_vec());

    core.intake(at(1));
    core.intake(at(2));

    assert_eq!(unfinished(&core), [OperationId(5000)]);
    let adopted = Call::Adopted(
        OperationId(5000),
        js_types::sched::WaiterId(0),
        Some(vec![7]),
    );
    assert_eq!(calls(&core), [adopted]);
    let losses = core.with(|view, _| view.sched().losses(OperationId(5000)));
    assert_eq!(losses, Some(2));
}

/// Asserts that an operation another frontend granted leaves the scheduler (nothing is
/// recorded as given up), that its caller is shown it executing, that the caller gets the
/// result the other frontend recorded, kept for the finished retention like a result this
/// core finished, and forgotten after it. Catches: an operation run twice, a caller left
/// waiting for a core that will never answer, and callers kept for ever.
#[test]
fn an_operation_another_frontend_took_leaves_the_scheduler_and_its_callers_follow() {
    let setup = Setup::new();
    let core = setup.core_with(&retained(), Rig::default(), 0);
    let queued = submit(&core, 1, 1);
    let other = setup.rows(7);
    other
        .granted(&grant_of(queued.operation.0, 0, "node-b"))
        .expect("granted by another frontend");

    core.intake(at(2));

    assert!(
        unfinished(&core).is_empty(),
        "the scheduler kept the operation"
    );
    assert_eq!(shown_to(&core, queued.waiter), Some(Stage::Executing));
    assert!(!calls(&core).iter().any(|c| matches!(c, Call::Requeued(_))));
    let finished = ending("finished elsewhere");
    other
        .finished(queued.operation, lease(5, 0), &finished)
        .expect("finished by another frontend");

    core.intake(at(3));

    let done = Stage::Done(b"finished elsewhere".to_vec());
    assert_eq!(shown_to(&core, queued.waiter), Some(done));
    let ended = Call::Followed(queued.operation, b"finished elsewhere".to_vec());
    let kept = Call::KeptFinished(queued.operation, vec![queued.waiter]);
    let seen = calls(&core);
    assert!(seen.contains(&ended) && seen.contains(&kept), "{seen:?}");
    assert!(
        !seen.contains(&Call::Forgot(queued.operation)),
        "forgotten at once"
    );

    // Kept until the retention has run from the poll that saw the result, forgotten at the
    // poll that is that long after it.
    let until = 3 + u64::try_from(RETENTION.as_millis()).expect("small");
    core.intake(at(until - 1));
    assert!(!calls(&core).contains(&Call::Forgot(queued.operation)));
    core.intake(at(until));
    assert!(calls(&core).contains(&Call::Forgot(queued.operation)));
}

/// Asserts that a look at the store that fails shows the callers of an operation another
/// frontend took nothing and loses none of them, that they are shown it executing once the look
/// works, and that callers who all gave up are no longer followed. Catches: callers dropped on
/// a store hiccup, and callers followed for ever after they left.
#[test]
fn callers_follow_an_operation_through_a_failed_look_until_they_give_up() {
    let setup = Setup::new();
    let core = setup.core(1);
    let queued = submit(&core, 1, 1);
    let granted = grant_of(queued.operation.0, 0, "node-b");
    setup
        .rows(7)
        .granted(&granted)
        .expect("granted by another frontend");

    setup.store.fail_reads(Some("done/"));
    core.intake(at(2));
    assert_eq!(shown_to(&core, queued.waiter), None);

    setup.store.fail_reads(None);
    core.intake(at(3));
    assert_eq!(shown_to(&core, queued.waiter), Some(Stage::Executing));

    core.with(|_, rig| rig.gone.insert(queued.waiter));
    core.intake(at(4));
    let kept = Call::KeptFinished(queued.operation, vec![queued.waiter]);
    assert!(calls(&core).contains(&kept), "retired once nobody listens");
    let before = calls(&core).len();
    core.intake(at(5));
    assert_eq!(calls(&core).len(), before, "followed no more");
}

/// Asserts that an operation whose lease the scheduler gave up, and whose requeue the store
/// has not taken, stays in the scheduler although the store has no queue row for it, and is
/// not handed off once the row is written. Catches: an operation lost by a store hiccup.
#[test]
fn an_operation_whose_requeue_the_store_has_not_taken_is_not_handed_off() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    assert_eq!((lease_rows(&setup), queue_rows(&setup)), (1, 0));
    setup.store.fail_writes(Some("loss/"));
    let past = js_sched::fence::LEASE_GRACE.as_millis() as u64 + 1;
    block_on(core.tick(at(past)));
    setup.store.fail_writes(None);

    core.intake(at(past + 1));

    assert_eq!(unfinished(&core).len(), 1, "the operation was handed off");
    assert_eq!(core.queued_operations(), 1);
    block_on(core.tick(at(past + 2)));
    assert_eq!((lease_rows(&setup), queue_rows(&setup)), (0, 1));
    core.intake(at(past + 3));
    assert_eq!(unfinished(&core).len(), 1);
    assert_eq!(core.queued_operations(), 1);
}

/// Asserts that an operation this core queued, granted, and then gave the lease up of (which
/// writes its queue row again) is not mistaken by the next poll for one another frontend
/// queued again: its caller stays the scheduler's, and nothing is handed off. Catches: a
/// core that hands its own work to the store follower and takes it back under a new caller.
#[test]
fn an_operation_whose_lease_this_core_gave_up_is_not_handed_off_by_the_next_poll() {
    let setup = Setup::new();
    let core = setup.core(1);
    let queued = submit(&core, 1, 1);
    assert_eq!((lease_rows(&setup), queue_rows(&setup)), (0, 1));
    core.intake(at(2));
    up(&core, 3, "node-a", "d1");
    block_on(core.tick(at(4)));
    assert_eq!((lease_rows(&setup), queue_rows(&setup)), (1, 0));
    let past = js_sched::fence::LEASE_GRACE.as_millis() as u64 + 5;
    block_on(core.tick(at(past)));
    assert_eq!(queue_rows(&setup), 1, "the give-up wrote the queue row");

    core.intake(at(past + 1));

    let waiters = core.with(|view, _| view.sched().waiters(queued.operation).map(<[_]>::to_vec));
    assert_eq!(
        waiters,
        Some(vec![queued.waiter]),
        "the caller was detached"
    );
    assert!(
        !calls(&core)
            .iter()
            .any(|c| matches!(c, Call::KeptFinished(..) | Call::Followed(..))),
        "handed off"
    );
}

/// Asserts that an operation held queued whose queue row is a newer write than the one read
/// (another frontend granted it and queued it again, with a loss counted) is taken in again
/// with the rows of now. Catches: stale rows kept for an operation another frontend rewrote.
#[test]
fn an_operation_queued_again_by_another_frontend_is_taken_in_again() {
    let setup = Setup::new();
    let core = setup.core(1);
    queue_on(&setup.rows(7), 5000);
    core.intake(at(1));
    let losses = |core: &Core<Rig>| core.with(|view, _| view.sched().losses(OperationId(5000)));
    assert_eq!(losses(&core), Some(0));

    setup.store.put("queue/0000000000001388", Vec::new());
    setup
        .store
        .put("loss/0000000000001388", 1u64.to_be_bytes().to_vec());
    core.intake(at(2));

    assert_eq!(unfinished(&core), [OperationId(5000)]);
    assert_eq!(losses(&core), Some(1));
    core.intake(at(3));
    assert_eq!(losses(&core), Some(1), "taken in once more");
}

/// Asserts that a core whose intake is closed, which holds an operation queued whose queue row
/// is then a newer write than the one read, hands the operation off and does not take it in
/// again. Catches: a draining core that goes on taking work.
#[test]
fn a_closed_core_does_not_take_an_operation_queued_again_back_in() {
    let setup = Setup::new();
    let core = setup.core(1);
    queue_on(&setup.rows(7), 5000);
    core.intake(at(1));
    assert_eq!(unfinished(&core), [OperationId(5000)]);
    core.close_intake();

    setup.store.put("queue/0000000000001388", Vec::new());
    core.intake(at(2));

    assert!(unfinished(&core).is_empty(), "a closed core took it in");
    assert_eq!(queue_rows(&setup), 1, "the store lost the operation");
}

/// Asserts that a queued operation another frontend granted after the poll read the queue is
/// not taken in: its lease belongs to the frontend of its node, so the scheduler holds nothing
/// for it. Catches: a lease taken up that its owner still runs.
#[test]
fn a_queued_operation_granted_since_the_poll_read_the_queue_is_not_taken_in() {
    let setup = Setup::new();
    let core = setup.core(1);
    let other = setup.rows(7);
    queue_on(&other, 5000);
    // The grant lands between the poll's read of the queue and its look at the operation's
    // lease, which begins with its queue row.
    let racing = setup.rows(8);
    setup.store.before_read("queue/", move || {
        racing
            .granted(&grant_of(5000, 0, "node-b"))
            .expect("granted by another frontend");
    });

    core.intake(at(1));

    assert!(
        unfinished(&core).is_empty(),
        "a leased operation was taken in"
    );
    assert_eq!(lease_rows(&setup), 1);
}

/// Asserts that a refusal the store refuses as stale (the operation is running under another
/// frontend's lease) does not leave its callers waiting on this core: they are answered from
/// the `done/` row that stands. Catches: callers left waiting for ever for a refusal that was
/// never written.
#[test]
fn a_refusal_the_store_refuses_as_stale_is_answered_from_the_store() {
    let setup = Setup::new();
    let wait = Duration::from_secs(1);
    let config = Config::new(1).with_unservable_wait(wait);
    let core = setup.core_with(&config, Rig::default(), 0);
    let queued = submit(&core, 1, 1);
    let other = setup.rows(7);
    other
        .granted(&grant_of(queued.operation.0, 0, "node-b"))
        .expect("granted by another frontend");

    block_on(core.tick(at(5_000)));

    assert_eq!(core.dropped_writes(), 1, "the refusal was written");
    assert!(!matches!(
        shown_to(&core, queued.waiter),
        Some(Stage::Done(_))
    ));
    other
        .finished(queued.operation, lease(5, 0), &ending("finished elsewhere"))
        .expect("finished by another frontend");
    core.intake(at(6_000));
    let done = Stage::Done(b"finished elsewhere".to_vec());
    assert_eq!(shown_to(&core, queued.waiter), Some(done));
}

/// Asserts that the memory floors another frontend raised are raised here. Catches: an
/// action that this core books too little for, and kills again, after another frontend's kills
/// taught the store.
#[test]
fn the_floors_another_frontend_raised_are_raised_here() {
    let setup = Setup::new();
    let core = setup.core(1);
    let floor = Floor {
        key: request(1).key,
        ask: 16 << 30,
        stamp: 3,
    };
    let kill = Requeue {
        operation: OperationId(9),
        lease: LeaseId::new(5, 0),
        worker: worker("node-b"),
        reason: RequeueReason::OutOfMemory {
            booked: 4 << 30,
            raised: 16 << 30,
        },
    };
    let change = FloorChange {
        raised: Some(floor.clone()),
        ..FloorChange::default()
    };
    setup
        .rows(7)
        .requeued(&kill, None, None, &change, None)
        .expect("written by another frontend");
    let ask = |core: &Core<Rig>, key: &WorkKey| core.with(|v, _| v.sched().memory_floor(key));
    assert_eq!(ask(&core, &floor.key), None);

    core.intake(at(1));

    assert_eq!(ask(&core, &floor.key), Some(16 << 30));
}

/// Asserts that a floor row the scheduler did not write fails the poll (nothing is taken in on
/// a guess), and that the poll works once the row is gone. Catches: work taken in over a store
/// the core cannot read.
#[test]
fn a_poll_that_cannot_read_a_floor_takes_nothing_in() {
    let setup = Setup::new();
    let core = setup.core(1);
    queue_on(&setup.rows(7), 5000);

    setup.store.put("floor/zz", vec![9]);
    core.intake(at(1));
    assert!(unfinished(&core).is_empty(), "work taken in on a guess");
    setup.store.remove("floor/zz");

    core.intake(at(2));
    assert_eq!(unfinished(&core), [OperationId(5000)]);
}

/// Asserts that the poll reports a stored epoch past the one this core serves, and reports none
/// when they are the same. Catches: a bump nobody hears of.
#[test]
fn a_stored_epoch_past_the_served_one_is_reported() {
    let setup = Setup::new();
    let core = setup.core(1);
    core.intake(at(1));
    assert_eq!(core.epoch_ahead(), None);

    let bumped = core.bump_epoch().expect("a bump").expect("a store");
    core.intake(at(2));
    core.intake(at(3));

    assert_eq!(core.epoch_ahead(), Some(bumped.epoch));
    assert_eq!(core.epoch(), Some(bumped.previous));
}

/// Asserts that the lease of a node no frontend hears is requeued by a poll, only after the
/// wait, and runs on a daemon of the core that polls once it is taken in. Catches: an
/// operation that waits for ever for a daemon that went away.
#[test]
fn a_poll_requeues_the_lease_of_a_node_no_frontend_hears() {
    let setup = Setup::new();
    let wait = Duration::from_secs(30);
    let config = Config::new(1).with_orphan_wait(wait);
    let core = setup.core_with(&config, Rig::default(), 0);
    let other = setup.rows(7);
    queue_on(&other, 5000);
    other
        .granted(&grant_of(5000, 0, "node-z"))
        .expect("granted to a node of another frontend");

    core.intake(at(1));
    assert_eq!((lease_rows(&setup), queue_rows(&setup)), (1, 0));
    let short = wait - Duration::from_millis(1);
    setup.store.advance(short);
    core.intake(at(2));
    assert_eq!(
        (lease_rows(&setup), queue_rows(&setup)),
        (1, 0),
        "swept before the wait"
    );
    setup.store.advance(wait - short);
    core.intake(at(2));

    assert_eq!((lease_rows(&setup), queue_rows(&setup)), (0, 1));
    up(&core, 3, "node-a", "d1");
    core.intake(at(4));
    block_on(core.tick(at(5)));
    assert_eq!(
        started(&core).len(),
        1,
        "the requeued operation did not run"
    );
}

/// Asserts that the wait before an unheard node's lease is requeued is, with no configuration,
/// the store's orphan wait: a poll one millisecond short of it leaves the lease, the next
/// requeues it. Catches: a default of no wait, which requeues the lease of a daemon that
/// reconnects to another frontend within a renewal interval.
#[test]
fn the_default_orphan_wait_is_the_stores() {
    let setup = Setup::new();
    let core = setup.core(1);
    let other = setup.rows(7);
    queue_on(&other, 5000);
    other
        .granted(&grant_of(5000, 0, "node-z"))
        .expect("granted to a node of another frontend");
    core.intake(at(1));
    let short = js_store::ORPHAN_WAIT - Duration::from_millis(1);
    setup.store.advance(short);
    core.intake(at(2));
    assert_eq!((lease_rows(&setup), queue_rows(&setup)), (1, 0));

    setup.store.advance(Duration::from_millis(1));
    core.intake(at(3));

    assert_eq!((lease_rows(&setup), queue_rows(&setup)), (0, 1));
}

/// Asserts that a core with a daemon connected does not requeue the lease of that daemon
/// whatever the store says of its session, however short the wait. Catches: a lease taken from
/// a daemon that is running it.
#[test]
fn a_connected_daemons_lease_is_never_swept() {
    let setup = Setup::new();
    let config = Config::new(1).with_orphan_wait(Duration::ZERO);
    let core = setup.core_with(&config, Rig::default(), 0);
    up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    setup.store.remove("session/node-a");

    core.intake(at(2));
    core.intake(at(3));

    assert_eq!(lease_rows(&setup), 1);
}

/// Asserts that a core whose intake is closed takes in no operation another frontend queued,
/// and that the store still holds the operation for the others. Catches: a draining core that
/// takes new work.
#[test]
fn a_core_whose_intake_is_closed_takes_no_queued_operation_in() {
    let setup = Setup::new();
    let core = setup.core(1);
    queue_on(&setup.rows(7), 5000);
    core.close_intake();

    core.intake(at(1));

    assert!(unfinished(&core).is_empty(), "a closed core took work in");
    assert_eq!(queue_rows(&setup), 1, "the store lost the operation");
}

/// Asserts that a core whose intake is closed places nothing on its nodes (those it has and one
/// that registers later) and leaves the operation it holds queued, which another frontend takes
/// in and runs, and that it takes the result of the lease it holds, so that an execution that
/// is running finishes where it started. Catches: a drain that strands or doubles work.
#[test]
fn a_core_whose_intake_is_closed_places_nothing_and_finishes_what_it_holds() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    up_after(&a, 0, "node-a", ("d1", ""), reported(1));
    up_after(&b, 0, "node-b", ("d2", ""), reported(1));
    let running = submit(&a, 1, 1);
    assert_eq!(started(&a), [lease(1, 0)]);
    a.close_intake();
    up_after(&a, 2, "node-late", ("d3", ""), reported(1));
    let waiting = submit(&a, 3, 2);
    block_on(a.tick(at(4)));
    assert_eq!(started(&a), [lease(1, 0)], "a node was placed work");
    assert_eq!(a.queued_operations(), 1);

    b.intake(at(5));
    block_on(b.tick(at(6)));
    assert_eq!(
        started(&b),
        [lease(1, 1024)],
        "the other frontend ran nothing"
    );
    let taken = finish(&a, 7, "node-a", running.operation, lease(1, 0));
    assert_eq!(taken, js_server_core::Taken::Accepted);
    assert_ne!(running.operation, waiting.operation);
}

/// A request no node of these tests is large enough for.
fn too_big() -> js_sched::Request {
    let mut big = request(9);
    big.resources = Resources::new(64_000, GIB);
    big
}

/// Asserts what the intake poll's look at the other frontends' daemons does, while an operation
/// waits for a worker an operator's cordon holds back: a node this core holds is not listed as
/// another's, so the operation waits; one whose stream has closed is (its session row is
/// there), so the operation waits kept; a row that cannot be read leaves the list as it was.
/// Catches: callers told to give up on work another frontend would run, and a list that a
/// failed look empties.
#[test]
fn the_poll_lists_a_closed_stream_as_remote() {
    let setup = Setup::new();
    let core = setup.core(1);
    let stream = up(&core, 0, "node-a", "d1");
    core.place(at(1), &worker("node-a"), NodeAction::Cordon)
        .expect("placed");
    let waiting = submit(&core, 2, 1);
    let last = || {
        calls(&core)
            .into_iter()
            .filter_map(|call| match call {
                Call::Show(operation, stage) if operation == waiting.operation => Some(stage),
                _ => None,
            })
            .next_back()
    };
    assert!(matches!(last(), Some(Stage::Waiting(_))));
    core.intake(at(3));
    assert!(matches!(last(), Some(Stage::Waiting(_))), "a held node");

    crate::common::close(&core, "node-a", stream);
    core.intake(at(4));
    assert!(
        matches!(last(), Some(Stage::WaitingKept(_))),
        "a closed stream"
    );

    // A session row that cannot be read leaves the list as it was.
    setup.store.put("session/zz", vec![0xff]);
    core.intake(at(5));
    assert!(
        matches!(last(), Some(Stage::WaitingKept(_))),
        "a corrupt row"
    );
    setup.store.remove("session/zz");
}

/// Asserts that an operation the scheduler refused after the unservable wait is no longer
/// decided again by the poll. Catches: a refused operation shown a stage again at every poll.
#[test]
fn a_refused_operation_is_not_decided_again_by_the_poll() {
    let setup = Setup::new();
    let config = Config::new(1).with_unservable_wait(Duration::from_secs(5));
    let core = setup.core_with(&config, Rig::default(), 0);
    up_after(&core, 0, "node-a", ("d1", ""), reported(1));
    let waiting = submit_request(&core, 1, too_big()).expect("queued");
    core.intake(at(2));

    block_on(core.tick(at(6_000)));
    core.intake(at(6_001));
    assert!(calls(&core).contains(&Call::Refuse(waiting.operation)));
    let before = calls(&core).len();
    core.intake(at(6_002));
    assert_eq!(
        calls(&core).len(),
        before,
        "a refused operation is decided again"
    );
}

/// Asserts that an operation no worker of this core can run, while a daemon session of another
/// frontend could, waits as `WaitingKept` once the poll has read the sessions: a drain keeps its
/// streams, since that frontend takes the queued operation. Catches: callers told to give up
/// on work another frontend runs.
#[test]
fn an_operation_another_frontends_daemon_could_run_waits_kept() {
    let setup = Setup::new();
    let (here, there) = (setup.core(1), setup.core(2));
    up(&there, 0, "node-there", "d2");
    up_after(&here, 0, "node-here", ("d1", ""), reported(1));
    let first = submit_request(&here, 1, too_big()).expect("queued");
    here.intake(at(2));
    let second = submit_request(&here, 3, too_big_for_here()).expect("queued");
    let last = |operation| {
        calls(&here)
            .into_iter()
            .filter_map(|call| match call {
                Call::Show(shown, stage) if shown == operation => Some(stage),
                _ => None,
            })
            .next_back()
    };
    assert!(matches!(
        last(second.operation),
        Some(Stage::WaitingKept(_))
    ));
    assert!(
        matches!(last(first.operation), Some(Stage::Waiting(_))),
        "no one could run it"
    );
}

/// A request the node of `here` is too small for but the other frontend's is not.
fn too_big_for_here() -> js_sched::Request {
    let mut big = request(10);
    big.resources = Resources::new(4_000, GIB);
    big
}

/// Asserts that a poll reads the `session/` rows once, for the orphan sweep, while no operation
/// waits for a worker, and a second time for the other frontends' daemons while one does: the
/// second scan is only worth its load when something needs its answer.
#[test]
fn a_poll_with_nothing_waiting_reads_the_session_rows_only_for_the_sweep() {
    let setup = Setup::new();
    let core = setup.core(1);
    up_after(&core, 0, "node-a", ("d1", ""), reported(1));
    let before = setup.store.scans_of("session/");
    core.intake(at(1));
    assert_eq!(
        setup.store.scans_of("session/") - before,
        1,
        "nothing waits"
    );

    submit_request(&core, 2, too_big()).expect("queued");
    let before = setup.store.scans_of("session/");
    core.intake(at(3));
    assert_eq!(
        setup.store.scans_of("session/") - before,
        2,
        "an operation waits"
    );
}

/// Asserts that the cordons another frontend wrote are applied by a poll, a drain with its
/// deadline on the store's clock, and that a node whose row is gone returns to placement.
/// Catches: an operator's cordon through one frontend that the others ignore.
#[test]
fn a_cordon_another_frontend_wrote_is_applied_and_lifted_by_the_poll() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    let other = setup.rows(7);

    other
        .cordoned(&worker("node-a"), Some(StoredCordon::Cordoned))
        .expect("written");
    core.intake(at(1));
    assert_eq!(placement(&core, "node-a"), Some(Cordon::Cordoned));
    core.intake(at(2));
    assert_eq!(
        placement(&core, "node-a"),
        Some(Cordon::Cordoned),
        "applied once"
    );

    let drain = StoredCordon::Draining {
        deadline_unix_ms: 600_000,
    };
    other
        .cordoned(&worker("node-a"), Some(drain))
        .expect("written");
    core.intake(at(3));
    assert!(matches!(
        placement(&core, "node-a"),
        Some(Cordon::Draining { .. } | Cordon::Drained)
    ));

    other.cordoned(&worker("node-a"), None).expect("returned");
    core.intake(at(4));
    assert_eq!(placement(&core, "node-a"), None);
}

/// Asserts that a drain another frontend wrote is applied by a poll with its deadline as far
/// from the poll's farm time as it is from the store's clock then, not from the clock's start
/// or the farm time alone. Catches: frontends with clocks of their own that disagree on when a
/// drain ends.
#[test]
fn a_drain_another_frontend_wrote_ends_by_the_stores_clock_at_the_poll() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    let drain = StoredCordon::Draining {
        deadline_unix_ms: 600_000,
    };
    setup
        .rows(7)
        .cordoned(&worker("node-a"), Some(drain))
        .expect("written");
    setup.store.advance(Duration::from_secs(100));

    core.intake(at(1_000));

    let left = 600_000 - setup.rows(7).store_ms();
    assert!(left < 600_000 - 99_000, "the clock moved: {left}");
    let deadline = at(1_000 + left);
    assert_eq!(
        placement(&core, "node-a"),
        Some(Cordon::Draining { deadline })
    );
}

/// Asserts that a drain put back in force when its node claims its leases ends by the store's
/// clock at the claim, as far from the claim's farm time as the stored deadline is from the
/// store's clock then. Catches: a drain resumed with its whole length again, or with none left.
#[test]
fn a_resumed_drain_ends_by_the_stores_clock_at_the_claim() {
    let setup = Setup::new();
    let first = setup.core(1);
    up_after(&first, 0, "node-a", ("d1", ""), reported(1));
    submit(&first, 1, 1);
    first
        .place(
            at(2),
            &worker("node-a"),
            NodeAction::Drain(Duration::from_secs(600)),
        )
        .expect("placed");
    drop(first);
    let second = setup.core_with(&Config::new(1), Rig::default(), 5_000);
    setup.store.advance(Duration::from_secs(100));
    let rows = setup.rows(7);
    let [(_, StoredCordon::Draining { deadline_unix_ms })] = rows.cordons().expect("read")[..]
    else {
        panic!("one drain row");
    };
    // The store's clock moves a little with every write the claim makes, so the claim's
    // reading lies between the one before it and the one after.
    let before = rows.store_ms();

    up_after(&second, 5_001, "node-a", ("d1", ""), reported(1));

    let after = rows.store_ms();
    assert!(before > 99_000, "the clock moved: {before}");
    let Some(Cordon::Draining { deadline }) = placement(&second, "node-a") else {
        panic!("the drain was not resumed");
    };
    let ms = deadline.as_millis();
    let (soonest, latest) = (
        5_001 + deadline_unix_ms - after,
        5_001 + deadline_unix_ms - before,
    );
    assert!(
        (soonest..=latest).contains(&ms),
        "{ms} not in {soonest}..={latest}"
    );
}

/// Asserts that a read of the cordons applies what another frontend wrote at once, returns the
/// rows, and returns nothing, with the nodes as they were, when the core has no store or the
/// store cannot be read. Catches: an operator's view that shows a node as placed after another
/// frontend drained it.
#[test]
fn the_cordons_are_read_at_once_when_a_node_is_looked_at() {
    let none = crate::common::core();
    assert_eq!(none.cordons_now(at(1)), None);

    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    setup
        .rows(7)
        .cordoned(&worker("node-a"), Some(StoredCordon::Cordoned))
        .expect("written");

    setup.store.fail_reads(Some("cordon/"));
    assert_eq!(core.cordons_now(at(1)), None);
    assert_eq!(placement(&core, "node-a"), None);

    setup.store.fail_reads(None);
    let rows = core.cordons_now(at(2)).expect("read");
    assert_eq!(rows.get(&worker("node-a")), Some(&StoredCordon::Cordoned));
    assert_eq!(placement(&core, "node-a"), Some(Cordon::Cordoned));
}

/// Asserts that a drain that ended at the start, for want of the leases the start does not take
/// up, is put back in force when its node says `Hello` and its leases are taken up, and that a
/// node whose drain row cannot be read keeps the drain it had. Catches: a node that runs work
/// and is no longer drained after a restart.
#[test]
fn a_drain_resumes_when_its_node_claims_the_leases_it_ran() {
    let setup = Setup::new();
    let first = setup.core(1);
    up_after(&first, 0, "node-a", ("d1", ""), reported(1));
    submit(&first, 1, 1);
    first
        .place(
            at(2),
            &worker("node-a"),
            js_server_core::NodeAction::Drain(Duration::from_secs(600)),
        )
        .expect("placed");
    drop(first);

    let second = setup.core_with(&Config::new(1), Rig::default(), 5_000);
    assert_eq!(
        placement(&second, "node-a"),
        Some(Cordon::Drained),
        "no lease at the start"
    );

    up_after(&second, 5_001, "node-a", ("d1", ""), reported(1));
    assert!(matches!(
        placement(&second, "node-a"),
        Some(Cordon::Draining { .. })
    ));

    drop(second);
    let third = setup.core_with(&Config::new(1), Rig::default(), 6_000);
    setup.store.fail_reads(Some("cordon/"));
    up_after(&third, 6_001, "node-a", ("d1", ""), reported(1));
    assert_eq!(
        placement(&third, "node-a"),
        Some(Cordon::Drained),
        "the row was unread"
    );
}

/// Asserts that the cordons a start took up are the baseline of the polls: the first poll does
/// not apply them again as if another frontend had just placed the node. Catches: a drain whose
/// deadline moves later at every restart's first poll.
#[test]
fn the_cordons_a_start_took_up_are_not_applied_again_by_the_first_poll() {
    let setup = Setup::new();
    let first = setup.core(1);
    up(&first, 0, "node-a", "d1");
    first
        .place(at(1), &worker("node-a"), NodeAction::Cordon)
        .expect("placed");
    drop(first);
    let second = setup.core_with(&Config::new(1), Rig::default(), 2);
    assert_eq!(placement(&second, "node-a"), Some(Cordon::Cordoned));

    let log = logged(|| second.intake(at(3)));

    assert!(!log.contains("another frontend placed the node"), "{log}");
}
