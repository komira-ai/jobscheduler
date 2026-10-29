//! A cost prediction a server computes outside the lock: charged to the operation from its
//! first placement round, taken from the adapter for an operation the core takes up from the
//! store, and never written to the store.

use js_sched::fence::START_GRACE;
use js_sched::{FINISHED_RETENTION, Prediction};
use js_server_core::{Admitted, Config, Core, Held};
use js_types::sched::LeaseId;
use js_types::sched::usage::LeaseCost;

use crate::common::{
    GIB, Rig, at, beat, block_on, core, enrol, reported, request, started, submit, submit_request,
    up, up_after,
};
use crate::recover::finish;
use crate::stored::Setup;

/// A warm prediction: a quarter core and half a GiB, below what `request` asks for.
const WARM: Prediction = Prediction {
    cpu_millis: 250,
    memory_bytes: GIB / 2,
    cold: false,
    life_minutes: 1,
};

/// Another warm prediction.
const NEWER: Prediction = Prediction {
    cpu_millis: 400,
    memory_bytes: GIB / 4,
    cold: false,
    life_minutes: 2,
};

/// A cold prediction.
const COLD: Prediction = Prediction {
    cpu_millis: 100,
    memory_bytes: GIB / 8,
    cold: true,
    life_minutes: 1,
};

/// What `request` is charged with no prediction: its cores, no memory, not cold.
const BOOKED: (u64, u64, bool) = (1_000, 0, false);

/// What a lease was charged: cores, memory, cold.
fn charged(core: &Core<Rig>, lease: LeaseId) -> (u64, u64, bool) {
    let cost: LeaseCost = core
        .with(|view, _| view.sched().lease_cost(lease))
        .expect("the scheduler holds the lease");
    (cost.cpu_millis, cost.memory_bytes, cost.cold)
}

/// What `prediction` is charged as.
fn as_charged(prediction: Prediction) -> (u64, u64, bool) {
    (
        prediction.cpu_millis,
        prediction.memory_bytes,
        prediction.cold,
    )
}

/// The lease the last `Start` carried.
fn last_started(core: &Core<Rig>) -> LeaseId {
    *started(core).last().expect("a lease was started")
}

/// Submits request `n` with `prediction`.
fn submit_predicted(
    core: &Core<Rig>,
    now: u64,
    n: u64,
    prediction: Option<Prediction>,
) -> Admitted {
    core.submit_with(
        at(now),
        request(n),
        &now.to_be_bytes(),
        None,
        prediction,
        enrol,
    )
    .expect("submitted")
}

/// Asserts that a prediction given with a submit is what the operation is charged by the
/// placement the submit itself leads to, with a node up and no tick since. Catches: a cost
/// set after the `Submit` is fed (the first round would charge the booked cost), and a
/// prediction that never reaches the scheduler.
#[test]
fn a_submit_with_a_prediction_is_charged_it_in_its_first_round() {
    let core = core();
    up(&core, 0, "node-a", "d1");

    submit_predicted(&core, 1, 1, Some(WARM));

    assert_eq!(charged(&core, last_started(&core)), as_charged(WARM));
}

/// Asserts that a cold prediction is charged as cold. Catches: a prediction flattened to
/// its figures.
#[test]
fn a_cold_prediction_is_charged_cold() {
    let core = core();
    up(&core, 0, "node-a", "d1");

    submit_predicted(&core, 1, 1, Some(COLD));

    assert_eq!(charged(&core, last_started(&core)), as_charged(COLD));
}

/// Asserts that a submit with no prediction, by either entry point, is charged the booked
/// cost: the request's cores, no memory, not cold. Catches: a default prediction made up
/// for `None`.
#[test]
fn a_submit_without_a_prediction_is_charged_the_booked_cost() {
    let core = core();
    up(&core, 0, "node-a", "d1");

    submit(&core, 1, 1);
    assert_eq!(charged(&core, last_started(&core)), BOOKED);

    submit_predicted(&core, 2, 2, None);
    assert_eq!(charged(&core, last_started(&core)), BOOKED);
}

/// Asserts that a prediction is the operation's own: the next operation, submitted without
/// one, is charged the booked cost. Catches: a prediction that outlives its submit.
#[test]
fn a_prediction_is_not_carried_to_the_next_operation() {
    let core = core();
    up(&core, 0, "node-a", "d1");

    submit_predicted(&core, 1, 1, Some(WARM));
    submit(&core, 2, 2);

    assert_eq!(charged(&core, last_started(&core)), BOOKED);
}

/// Asserts that a request joining a queued operation (no node to place it on) replaces the
/// prediction the operation holds with its own, and that a join with none leaves the held
/// one. Catches: the first prediction kept against a newer one, and a join that clears the
/// held prediction.
#[test]
fn a_twin_that_waits_for_a_lease_takes_the_newer_prediction() {
    let core = core();
    let first = submit_predicted(&core, 1, 1, Some(WARM));
    let joined = submit_predicted(&core, 2, 1, Some(NEWER));
    assert_eq!(joined.operation, first.operation);
    assert!(joined.joined);

    up(&core, 4, "node-a", "d1");

    assert_eq!(charged(&core, last_started(&core)), as_charged(NEWER));
}

/// Asserts that a join with no prediction leaves the one the queued operation holds.
/// Catches: a join that clears the held prediction.
#[test]
fn a_twin_with_no_prediction_keeps_the_held_one() {
    let core = core();
    submit_predicted(&core, 1, 1, Some(WARM));
    submit_predicted(&core, 2, 1, None);

    up(&core, 3, "node-a", "d1");

    assert_eq!(charged(&core, last_started(&core)), as_charged(WARM));
}

/// Asserts that a request joining an operation that holds a lease does not change the cost
/// the operation is next placed at: once its lease is given up and it is placed again, it is
/// charged the prediction it had before the lease, not the joiner's. (The lease's own charge
/// is recorded when it is granted, so it is not what this checks.)
/// Catches: a prediction replacing the cost of an operation that has a lease.
#[test]
fn a_twin_of_a_leased_operation_does_not_change_its_cost() {
    let core = core();
    let stream = up(&core, 0, "node-a", "d1");
    submit_predicted(&core, 10, 1, Some(WARM));
    let first = last_started(&core);
    assert_eq!(charged(&core, first), as_charged(WARM));

    let joined = submit_predicted(&core, 11, 1, Some(NEWER));
    assert!(joined.joined);

    // The node never lists the lease: past the start grace it is given up, and the operation
    // goes to the other node.
    let other = up(&core, 12, "node-b", "d2");
    let grace = u64::try_from(START_GRACE.as_millis()).expect("millis");
    let late = 10 + grace + 1;
    beat(&core, late, ("node-b", other), 1, &[]);
    beat(&core, late, ("node-a", stream), 1, &[]);
    block_on(core.tick(at(late + 1)));
    let again = last_started(&core);
    assert_ne!(again, first, "the operation was placed again");
    assert_eq!(charged(&core, again), as_charged(WARM));
}

/// Asserts that a core opened over a store whose queued operations the adapter has
/// predictions for charges them those, and one the adapter has none for the booked cost.
/// Catches: a start that takes the operations up without asking the adapter, and a
/// prediction put on the wrong operation.
#[test]
fn an_operation_taken_up_at_the_start_is_charged_what_the_adapter_predicts() {
    let setup = Setup::new();
    let first = setup.core(1);
    let predicted = submit_request(&first, 1, request(1))
        .expect("queued")
        .operation;
    let plain = submit_request(&first, 2, request(2))
        .expect("queued")
        .operation;

    let mut rig = Rig::default();
    rig.predictions.insert(request(1).key, WARM);
    let second = setup.core_with(&Config::new(2), rig, 3);
    up(&second, 4, "node-a", "d1");

    let leases: Vec<LeaseId> = started(&second);
    assert_eq!(leases.len(), 2);
    let asked = second.with(|_, rig| rig.predicted_for.clone());
    assert_eq!(asked, [predicted, plain]);
    let costs: Vec<_> = leases.iter().map(|l| charged(&second, *l)).collect();
    assert!(costs.contains(&as_charged(WARM)));
    assert!(costs.contains(&BOOKED));
}

/// Asserts that an operation another frontend queued, taken in by an intake poll, is
/// charged what the adapter predicts, and that one the adapter has no prediction for is
/// charged the booked cost. Catches: a poll that takes an operation in without asking.
#[test]
fn an_operation_taken_in_by_the_intake_poll_is_charged_what_the_adapter_predicts() {
    let setup = Setup::new();
    let mut rig = Rig::default();
    rig.predictions.insert(request(1).key, WARM);
    let core = setup.core_with(&Config::new(1), rig, 0);
    let other = setup.core(7);
    let queued = submit_request(&other, 1, request(1))
        .expect("queued")
        .operation;

    core.intake(at(2));
    core.intake(at(3));
    up(&core, 4, "node-a", "d1");

    assert_eq!(core.with(|_, rig| rig.predicted_for.clone()), [queued]);
    assert_eq!(charged(&core, last_started(&core)), as_charged(WARM));
}

/// Asserts that the adapter is not asked about an operation that holds a lease and that
/// another frontend granted, taken up when its node says `Hello` here. Catches: a prediction
/// asked for, and set on, an operation whose cost its lease already holds.
#[test]
fn an_operation_taken_up_with_a_lease_is_not_asked_about() {
    let setup = Setup::new();
    let first = setup.core(1);
    up(&first, 0, "node-a", "d1");
    let leased = submit_request(&first, 1, request(1)).expect("queued");
    assert_eq!(started(&first).len(), 1);

    let mut rig = Rig::default();
    rig.predictions.insert(request(1).key, WARM);
    let second = setup.core_with(&Config::new(2), rig, 2);
    up_after(&second, 3, "node-a", ("d2", ""), reported(8));

    let held = second.with(|view, _| view.sched().state(leased.operation).is_some());
    assert!(held, "the leased operation was taken up here");
    assert_eq!(second.with(|_, rig| rig.predicted_for.clone()), []);
}

/// Asserts that the rows a submit writes are the same bytes with a prediction and without
/// one. Catches: a prediction kept in the store.
#[test]
fn the_stored_rows_are_the_same_with_and_without_a_prediction() {
    let rows_after = |prediction: Option<Prediction>| {
        let setup = Setup::new();
        let core = setup.core(1);
        submit_predicted(&core, 1, 1, prediction);
        setup
            .store
            .rows_of("")
            .into_iter()
            .map(|(key, row)| (key, row.value))
            .collect::<Vec<_>>()
    };

    let without = rows_after(None);
    assert!(!without.is_empty());
    assert_eq!(rows_after(Some(WARM)), without);
    assert_eq!(rows_after(Some(COLD)), without);
}

/// Asserts that a request for an operation another frontend holds sets no cost here: this
/// scheduler never places the operation, so it keeps no prediction for it. Catches: a
/// prediction put in the scheduler for an operation it does not hold.
#[test]
fn a_submit_for_an_operation_held_elsewhere_sets_no_cost() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    up(&a, 0, "node-a", "d1");
    let leased = submit_predicted(&a, 1, 1, None);

    let joined = submit_predicted(&b, 2, 1, Some(WARM));

    assert_eq!(joined.held, Held::Elsewhere(leased.operation));
    assert!(!b.with(|view, _| view.sched().has_cost(joined.operation)));
}

/// Asserts that the prediction of a finished operation is kept while the scheduler still
/// holds the operation and gone once it is retired. Catches: a prediction that outlives its
/// operation.
#[test]
fn a_finished_operations_prediction_is_gone_after_retire() {
    let core = core();
    up(&core, 0, "node-a", "d1");
    let admitted = submit_predicted(&core, 1, 1, Some(WARM));
    let lease = last_started(&core);
    assert!(core.with(|view, _| view.sched().has_cost(admitted.operation)));

    finish(&core, 3, "node-a", admitted.operation, lease);
    assert!(core.with(|view, _| view.sched().has_cost(admitted.operation)));

    let retired = 3 + u64::try_from(FINISHED_RETENTION.as_millis()).expect("millis");
    block_on(core.tick(at(retired)));
    assert!(!core.with(|view, _| view.sched().has_cost(admitted.operation)));
}
