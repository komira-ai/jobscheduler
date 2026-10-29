//! The longest an operation may run (its action's timeout), given with a submission: the
//! scheduler's whole-node drain reads it for the deadline of a whole-machine request's hold,
//! the state store keeps it in an `opwall/` row, and a restart or another frontend takes it up
//! again. A submission without one must leave the store and the calls as they were.

use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::Request;
use js_server_core::{Admitted, Config, Core, Reported, SubmitOptions, WholeNodeConfig};
use js_store::keys;
use js_types::sched::{FarmTime, LeaseKind, OperationId, Resources};

use crate::common::{
    Call, GIB, Rig, at, block_on, calls, core_by, enrol, lease, request, submit, up_after,
};
use crate::recover::finish;
use crate::stored::Setup;

/// The wall time of the operations that have one.
const WALL: Duration = Duration::from_secs(600);

/// A core that holds and drains workers for whole-machine requests, the hold's deadline being
/// the operation's own wall time (the setting names no deadline of its own).
fn whole_config(term: u64) -> Config {
    Config::new(term).with_whole_node(WholeNodeConfig::dedicated(4))
}

/// A node that serves whole-machine leases.
fn whole_node() -> Reported {
    let caps = NodeCaps::from_report([("arch", "arm64")])
        .expect("a node report")
        .with_drivers(["container", "native-whole-machine"]);
    Reported::serving(Resources::new(8_000, 64 * GIB), caps)
}

/// Registers node-a, which serves whole-machine leases, at `now`.
fn node_up(core: &Core<Rig>, now: u64) {
    up_after(core, now, "node-a", ("d1", ""), whole_node());
}

/// The whole-machine request of caller `n`.
fn whole(n: u64) -> Request {
    let mut whole = request(n);
    whole.kind = LeaseKind::WholeMachine;
    whole
}

/// Submits `request` with the longest run `wall`, enrolling the caller in the rig.
fn walled(core: &Core<Rig>, now: u64, request: Request, wall: Option<Duration>) -> Admitted {
    let options = SubmitOptions::new().with_max_wall(wall);
    let parcel = now.to_be_bytes();
    let admitted = core.submit_with_options(at(now), request, &parcel, options, enrol);
    admitted.expect("submitted")
}

/// How long the hold `operation` has on its node lasts before it is given up, from the time the
/// hold was made: `None` for an operation that holds no node, `Some(None)` for a hold that is
/// never given up.
fn hold_window(core: &Core<Rig>, operation: OperationId) -> Option<Option<Duration>> {
    core.with(|view, _| {
        let mark = view.sched().whole_mark(operation)?;
        let never = FarmTime::from_millis(u64::MAX);
        let window =
            (mark.deadline != never).then(|| mark.deadline.saturating_duration_since(mark.made));
        Some(window)
    })
}

/// The wall times the store holds, as another frontend's handle reads them: for each
/// unfinished operation, in id order.
fn stored_walls(setup: &Setup) -> Vec<Option<Duration>> {
    let stored = setup.rows(9).load().expect("loaded");
    stored.operations.iter().map(|op| op.max_wall).collect()
}

/// The keys of the `opwall/` rows of the store.
fn wall_rows(setup: &Setup) -> Vec<String> {
    let rows = setup.store.rows_of("opwall/");
    rows.into_iter().map(|(key, _)| key).collect()
}

/// Asserts that a whole-machine request waiting for a busy node holds it for as long as the
/// wall time it was submitted with, and for ever if it was submitted with none. Catches: a wall
/// time dropped on the way to the scheduler (the hold of every whole-machine request would
/// never be given up, and the cheapest node to drain could not be told), and one given to the
/// wrong operation.
#[test]
fn a_whole_machine_requests_hold_lasts_as_long_as_its_wall_time() {
    for wall in [Some(WALL), Some(WALL * 6), None] {
        let core = core_by(&whole_config(1));
        node_up(&core, 0);
        submit(&core, 0, 1);

        let admitted = walled(&core, 1, whole(2), wall);

        assert_eq!(
            hold_window(&core, admitted.operation),
            Some(wall),
            "{wall:?}"
        );
    }
}

/// Asserts that the wall time a submission is given with is one `opwall/` row, in the bytes
/// the farm server writes, beside the operation's other rows, under the id of the operation
/// the caller is given, and that an operation given none has none. Catches: a wall time that
/// needs no store to take effect but is lost at a restart, and a row written under another
/// operation's id.
#[test]
fn a_wall_time_is_written_with_the_submission_under_the_operations_id() {
    let setup = Setup::new();
    let core = setup.core_with(&whole_config(1), Rig::default(), 0);

    let plain = walled(&core, 1, request(1), None);
    let long = walled(
        &core,
        2,
        request(2),
        Some(Duration::new(3_600, 250_000_000)),
    );

    assert_eq!(wall_rows(&setup), [keys::opwall(long.operation)]);
    assert_ne!(plain.operation, long.operation);
    let rows = setup.store.rows_of("opwall/");
    assert_eq!(
        rows[0].1.value,
        [1, 0, 0, 0, 0, 0, 0, 14, 16, 14, 230, 178, 128]
    );
    assert_eq!(
        stored_walls(&setup),
        [None, Some(Duration::new(3_600, 250_000_000))]
    );
}

/// Asserts that a request that joins an operation neither writes a wall time of its own nor
/// replaces the operation's: the row and the hold keep the first request's. Catches: a late
/// caller with another timeout changing how long the operation's hold lasts.
#[test]
fn a_request_that_joins_an_operation_leaves_its_wall_time() {
    let setup = Setup::new();
    let core = setup.core_with(&whole_config(1), Rig::default(), 0);
    node_up(&core, 0);
    submit(&core, 0, 1);
    let first = walled(&core, 1, whole(2), Some(WALL));

    let second = walled(&core, 2, whole(2), Some(WALL * 3));

    assert!(second.joined);
    assert_eq!(second.operation, first.operation);
    assert_eq!(stored_walls(&setup), [None, Some(WALL)]);
    assert_eq!(hold_window(&core, first.operation), Some(Some(WALL)));
}

/// Asserts that a core started over a store that holds a queued whole-machine request takes up
/// the wall time its row names: once the node is busy, the hold lasts as long as the time the
/// first process was given. Catches: a restart that forgets the wall time (the hold would never
/// be given up).
#[test]
fn a_restart_takes_up_the_wall_time_of_a_queued_operation() {
    let setup = Setup::new();
    let first = setup.core_with(&whole_config(1), Rig::default(), 0);
    node_up(&first, 0);
    submit(&first, 1, 1);
    let queued = walled(&first, 2, whole(2), Some(WALL));
    drop(first);

    let second = setup.core_with(&whole_config(1), Rig::default(), 5_000);
    node_up(&second, 5_000);
    block_on(second.tick(at(5_001)));

    assert_eq!(
        hold_window(&second, queued.operation),
        Some(Some(WALL)),
        "calls: {:?}",
        calls(&second)
    );
}

/// Asserts that a frontend which takes in an operation another frontend queued takes up the
/// wall time its row names, so the hold it makes lasts as long as the time the first frontend
/// was given. Catches: a handover that loses the wall time of the operations it is handed (the
/// hold would never be given up).
#[test]
fn another_frontend_takes_up_the_wall_time_of_an_operation_it_takes_in() {
    let setup = Setup::new();
    let a = setup.core_with(&whole_config(1), Rig::default(), 0);
    let b = setup.core_with(&whole_config(2), Rig::default(), 0);
    node_up(&b, 0);
    submit(&b, 1, 1);
    let queued = walled(&a, 1, whole(2), Some(WALL));

    b.intake(at(2));
    block_on(b.tick(at(3)));

    assert_eq!(hold_window(&b, queued.operation), Some(Some(WALL)));
}

/// Asserts that a request another frontend took in, which joins through the store and finds
/// the operation queued there, is held for the wall time of the first request's row. Catches: a
/// join that takes the operation up without the row (the other frontend queued it with a wall
/// time).
#[test]
fn a_request_that_joins_through_the_store_takes_up_the_wall_time_of_the_row() {
    let setup = Setup::new();
    let a = setup.core_with(&whole_config(1), Rig::default(), 0);
    let b = setup.core_with(&whole_config(2), Rig::default(), 0);
    node_up(&b, 0);
    submit(&b, 1, 1);
    let queued = walled(&a, 1, whole(2), Some(WALL));

    let joined = walled(&b, 2, whole(2), Some(WALL * 3));

    assert!(joined.joined);
    assert_eq!(joined.operation, queued.operation);
    assert_eq!(hold_window(&b, joined.operation), Some(Some(WALL)));
}

/// Asserts that the result of an operation takes its `opwall/` row out of the store with its
/// other rows. Catches: a row left behind by every operation that was given a wall time.
#[test]
fn ending_an_operation_removes_its_wall_row() {
    let setup = Setup::new();
    let core = setup.core_with(&whole_config(1), Rig::default(), 0);
    node_up(&core, 0);
    let admitted = walled(&core, 1, request(1), Some(WALL));
    assert_eq!(wall_rows(&setup), [keys::opwall(admitted.operation)]);

    finish(&core, 2, "node-a", admitted.operation, lease(1, 0));

    assert_eq!(wall_rows(&setup), Vec::<String>::new());
}

/// What a script leaves in a store and tells its rig: the store's rows (key and value) and the
/// hooks called.
fn trace_of(options: Option<SubmitOptions>) -> (Vec<(String, Vec<u8>)>, Vec<Call>) {
    let setup = Setup::new();
    let core = setup.core_with(&whole_config(1), Rig::default(), 0);
    node_up(&core, 0);
    let first = submit(&core, 1, 1);
    let parcel = 2u64.to_be_bytes();
    let second = match options {
        Some(options) => core.submit_with_options(at(2), request(2), &parcel, options, enrol),
        None => core.submit(at(2), request(2), &parcel, enrol),
    };
    second.expect("submitted");
    finish(&core, 3, "node-a", first.operation, lease(1, 0));
    block_on(core.tick(at(4)));
    let rows = setup.store.rows_of("");
    let rows = rows.into_iter().map(|(name, row)| (name, row.value));
    (rows.collect(), calls(&core))
}

/// Asserts that a submission through the entry point that takes options, given none, leaves
/// the rows and the calls the plain submit leaves, and writes no `opwall/` row. Catches: a
/// change to anything a core does for a caller that has no wall time (a store written by it
/// would differ from the store of a core without the feature).
#[test]
fn a_submission_without_a_wall_time_is_byte_identical() {
    let plain = trace_of(None);
    let empty = trace_of(Some(SubmitOptions::new()));
    let none = trace_of(Some(SubmitOptions::default().with_max_wall(None)));

    assert_eq!(empty, plain);
    assert_eq!(none, plain);
    assert!(plain.0.iter().all(|(name, _)| !name.starts_with("opwall/")));
}

/// Asserts that each part of the options is set by its own builder and by no other: a size
/// key, a prediction and a wall time stay as given when the others are set after them.
/// Catches: a builder that writes another part's field.
#[test]
fn the_builders_of_the_options_each_set_their_own_part() {
    let prediction = js_sched::Prediction::cold(GIB);
    let built = SubmitOptions::new()
        .with_size_key(Some(7))
        .with_prediction(Some(prediction))
        .with_max_wall(Some(WALL));

    assert_eq!(built.size_key, Some(7));
    assert_eq!(built.prediction, Some(prediction));
    assert_eq!(built.max_wall, Some(WALL));
    assert_eq!(SubmitOptions::new(), SubmitOptions::default());
    assert_eq!(SubmitOptions::new().size_key, None);
    assert_eq!(SubmitOptions::new().prediction, None);
    assert_eq!(SubmitOptions::new().max_wall, None);
    assert_ne!(built, SubmitOptions::new());
}
