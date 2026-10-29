//! Size keys and key floors on the core: the key a submission is filed under, the floor a
//! memory kill raises under it, the rows both are kept in, what a start takes up of them,
//! and how a floor the adapter's estimator lowers is applied and written. A core with size
//! keys off (the default) must behave as it did before any of it existed.

use std::time::Duration;

use js_sched::{DaemonInstance, KeyFloor, NodePhase, OpState, Request, Requeue};
use js_server_core::{
    Adapter, Admitted, Config, Core, Delivery, Lowered, Report, Stage, Stream, View,
};
use js_store::{Ending, StateError, keys};
use js_types::sched::{
    Answer, Effect, Failure, LeaseGrant, LeaseId, OperationId, Outcome, Refusal, StartLease,
    WaiterId, WorkerId,
};

use crate::common::{
    Call, GIB, Rig, at, block_on, calls, core_by, enrol, reported, request, submit, submit_request,
    up, worker,
};
use crate::recover::finish;
use crate::stored::Setup;

/// A size key.
const K: u64 = 0x0123_4567_89ab_cdef;

/// The configuration of a core that reads and writes size keys.
fn on() -> Config {
    Config::new(1).with_size_keys(true)
}

/// A rig whose adapter names the summary of `K`'s floor `name`.
fn named(name: &str) -> Rig {
    let mut rig = Rig::default();
    rig.names.insert(K, name.to_owned());
    rig
}

/// Submits request `n` filed under `key`, at `now`.
fn keyed(core: &Core<Rig>, now: u64, n: u64, key: u64) -> Admitted {
    core.submit_keyed(at(now), request(n), &now.to_be_bytes(), Some(key), enrol)
        .expect("submitted")
}

/// The lease `operation` holds on its node.
fn lease_of(core: &Core<Rig>, operation: OperationId) -> LeaseId {
    core.with(|view, _| match view.sched().state(operation) {
        Some(OpState::Leased { lease, .. } | OpState::Running { lease, .. }) => *lease,
        other => panic!("{operation} holds no lease: {other:?}"),
    })
}

/// Reports that `operation`'s lease on node-a was killed for passing its memory limit.
fn kill(core: &Core<Rig>, now: u64, operation: OperationId) {
    let report = Report {
        operation,
        lease: lease_of(core, operation),
        outcome: Outcome::Failed(Failure::OutOfMemory),
    };
    block_on(core.report(at(now), &worker("node-a"), report, |_| {}));
}

/// Finishes `operation`, which holds a lease on node-a: a report, so the adapter is asked for
/// the floors to lower.
fn complete(core: &Core<Rig>, now: u64, operation: OperationId) {
    let lease = lease_of(core, operation);
    finish(core, now, "node-a", operation, lease);
}

/// The scheduler's floor of `key`.
fn floor(core: &Core<Rig>, key: u64) -> Option<u64> {
    core.with(|view, _| view.sched().key_floor(key))
}

/// The memory `operation` is booked, queued or leased.
fn booked(core: &Core<Rig>, operation: OperationId) -> Option<u64> {
    core.with(|view, _| {
        let request = view.sched().request(operation);
        request.map(|request| request.resources.memory_bytes)
    })
}

/// The size key the scheduler files `operation` under.
fn key_of(core: &Core<Rig>, operation: OperationId) -> Option<u64> {
    core.with(|view, _| view.sched().size_key(operation))
}

/// The size key each `adopted` call was told, in order.
fn told_keys(core: &Core<Rig>) -> Vec<Option<u64>> {
    core.with(|_, rig| rig.adoptions.iter().map(|a| a.size_key).collect())
}

/// The `(key, ask)` of every key floor row of the store, as a start reads them.
fn floor_rows(setup: &Setup) -> Vec<(u64, u64)> {
    let stored = setup.rows(9).load().expect("loaded");
    let rows = stored.key_floors.iter();
    rows.map(|floor| (floor.key, floor.ask)).collect()
}

/// The names of the `keyfloor/` rows of the store.
fn floor_names(setup: &Setup) -> Vec<String> {
    let rows = setup.store.rows_of("keyfloor/");
    rows.into_iter().map(|(name, _)| name).collect()
}

/// Asserts that a key given with a submission is written in the submission's transaction
/// under the id of the operation the caller is given, and that the scheduler files the
/// operation under it. Catches: a key dropped on the way to the rows (the row is absent and a
/// restart cannot book the floor), and a row written under another id than the operation's.
#[test]
fn submit_keyed_writes_the_opkey_row_in_the_submit_transaction() {
    let setup = Setup::new();
    let core = setup.core_with(&on(), Rig::default(), 0);
    up(&core, 0, "node-a", "d1");

    let admitted = keyed(&core, 1, 1, K);

    let written = setup.store.rows_of("opkey/");
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].0, keys::opkey(admitted.operation));
    let row = setup
        .rows(9)
        .stored_operation(admitted.operation)
        .expect("read")
        .expect("held");
    assert_eq!(row.size_key, Some(K));
    assert_eq!(key_of(&core, admitted.operation), Some(K));
}

/// Asserts that a core with no state store files a keyed operation under its key all the
/// same, and that a core with size keys off ignores the key: nothing is written, and the
/// scheduler files the operation under none. Catches: a key that needs a store to take
/// effect, and a key that books a floor on a core that has them off.
#[test]
fn a_key_is_set_without_a_store_and_ignored_with_size_keys_off() {
    let stored_nowhere = core_by(&on());
    let admitted = keyed(&stored_nowhere, 1, 1, K);
    assert_eq!(key_of(&stored_nowhere, admitted.operation), Some(K));

    let off = Setup::new();
    let core = off.core(1);
    let admitted = keyed(&core, 1, 1, K);
    assert_eq!(key_of(&core, admitted.operation), None);
    assert!(off.store.rows_of("opkey/").is_empty());
}

/// Asserts that a request joining an operation the scheduler holds keeps that operation's
/// key and writes no row, and that one joining an operation the store holds (queued by
/// another frontend) takes the key of that operation's own row, not the key it came with.
/// Catches: a joined caller's key written over the twin's, a second `opkey/` row, and a
/// store-joined operation that the scheduler files under no key or the newcomer's.
#[test]
fn a_twin_joined_keeps_its_key_and_writes_no_row() {
    let setup = Setup::new();
    let core = setup.core_with(&on(), Rig::default(), 0);
    let first = keyed(&core, 1, 1, K);
    let again = keyed(&core, 2, 1, K + 1);
    assert!(again.joined);
    assert_eq!(again.operation, first.operation);
    assert_eq!(key_of(&core, first.operation), Some(K));
    assert_eq!(setup.store.rows_of("opkey/").len(), 1);

    // Queued in the store by another frontend, under key K: this core joins it by request(2).
    let other = setup.rows(7);
    other
        .submit(OperationId(5000), &request(2), &[7], Some(K))
        .expect("queued by another frontend");
    let joined = keyed(&core, 3, 2, K + 2);
    assert!(joined.joined);
    assert_eq!(joined.operation, OperationId(5000));
    assert_eq!(key_of(&core, joined.operation), Some(K));
    assert_eq!(
        setup.store.rows_of("opkey/").len(),
        2,
        "no row for the newcomer"
    );
}

/// Asserts that an operation submitted under a key whose floor a kill raised is booked at
/// that floor in its first placement round, and that one under another key, or none, is
/// booked what it asked. Catches: a key set after the `Submit` is fed (the first round books
/// the request), a key set for the wrong operation id, and a floor that applies to every
/// operation.
#[test]
fn the_first_placement_round_books_the_key_floor() {
    let core = core_by(&on());
    up(&core, 0, "node-a", "d1");
    let killed = keyed(&core, 10, 1, K);
    kill(&core, 20, killed.operation);
    let raised = floor(&core, K).expect("the kill raised the floor");
    assert!(raised > GIB);

    let same_key = keyed(&core, 30, 2, K);
    let unkeyed = submit(&core, 31, 3);
    let other_key = keyed(&core, 32, 4, K + 1);

    assert_eq!(booked(&core, same_key.operation), Some(raised));
    assert_eq!(booked(&core, unkeyed.operation), Some(GIB));
    assert_eq!(booked(&core, other_key.operation), Some(GIB));
}

/// Asserts that a kill past the action's limit writes the key floor row with the name of
/// the size summary the adapter gives for the key, and none when it gives none. Catches: a
/// requeue written without the summary (the floor's decay is lost at a restart).
#[test]
fn a_kill_writes_the_keyfloor_row_with_the_adapters_summary_name() {
    for (rig, names) in [
        (named("est/k"), vec![(K, "est/k".to_owned())]),
        (Rig::default(), Vec::new()),
    ] {
        let setup = Setup::new();
        let core = setup.core_with(&on(), rig, 0);
        up(&core, 0, "node-a", "d1");
        let admitted = keyed(&core, 1, 1, K);

        kill(&core, 5, admitted.operation);

        let raised = floor(&core, K).expect("raised");
        assert_eq!(floor_rows(&setup), [(K, raised)]);
        assert_eq!(setup.rows(9).load().expect("loaded").key_names, names);
    }
}

/// A store holding one queued operation filed under `K`, and its id, with the core that
/// wrote it gone.
fn queued_under_key() -> (Setup, OperationId) {
    let setup = Setup::new();
    let first = setup.core_with(&on(), Rig::default(), 0);
    let admitted = keyed(&first, 1, 1, K);
    drop(first);
    (setup, admitted.operation)
}

/// Asserts that a start with size keys off is not stopped by an `opkey/` row that is not
/// one, and tells the adapter of no key. Catches: a core that reads the rows whatever the
/// setting (a rollback to a build without learned sizes would not start).
#[test]
fn a_restart_with_size_keys_off_ignores_a_bad_opkey_row() {
    let (setup, operation) = queued_under_key();
    setup.store.put(&keys::opkey(operation), vec![9, 9, 9]);

    let restarted = setup.core_with(&Config::new(1), Rig::default(), 5);

    assert_eq!(told_keys(&restarted), [None]);
    assert_eq!(key_of(&restarted, operation), None);
}

/// Asserts that a start with size keys on is refused over an `opkey/` row that is not one.
/// Catches: a bad key row read as no key, which would book the operation without its floor
/// and hide the damage.
#[test]
fn a_restart_with_size_keys_on_refuses_a_bad_opkey_row() {
    let (setup, operation) = queued_under_key();
    setup.store.put(&keys::opkey(operation), vec![9, 9, 9]);

    let opened = Core::open(&on(), Rig::default(), setup.storage(), at(5));

    assert!(matches!(opened.err(), Some(StateError::Corrupt { .. })));
}

/// Asserts that `adopted` is told the size key of a stored operation when size keys are on,
/// and the scheduler files the operation under it, and is told none with them off. Catches:
/// a key read but not handed on (the adapter cannot name the operation's estimator key), and
/// one handed on with size keys off.
#[test]
fn adopted_is_told_the_size_key_when_read() {
    let (setup, operation) = queued_under_key();

    let reading = setup.core_with(&on(), Rig::default(), 5);
    assert_eq!(told_keys(&reading), [Some(K)]);
    assert_eq!(key_of(&reading, operation), Some(K));
    drop(reading);

    let ignoring = setup.core_with(&Config::new(1), Rig::default(), 6);
    assert_eq!(told_keys(&ignoring), [None]);
    assert_eq!(key_of(&ignoring, operation), None);
}

/// Asserts that a start with size keys on restores the key floors and tells the adapter the
/// summary name of each (once), and that one with them off restores none and tells nothing.
/// Catches: floors lost at a restart (a killed action is killed again), summary names never
/// reaching the adapter (a key whose operations all ended never decays), and a start with
/// size keys off that books a floor.
#[test]
fn a_restart_restores_key_floors_and_names() {
    let setup = Setup::new();
    let first = setup.core_with(&on(), named("est/k"), 0);
    up(&first, 0, "node-a", "d1");
    let admitted = keyed(&first, 1, 1, K);
    kill(&first, 5, admitted.operation);
    let raised = floor(&first, K).expect("raised");
    drop(first);

    let reading = setup.core_with(&on(), Rig::default(), 10);
    assert_eq!(floor(&reading, K), Some(raised));
    let told = reading.with(|_, rig| rig.key_names_told.clone());
    assert_eq!(told, [vec![(K, "est/k".to_owned())]]);
    drop(reading);

    let ignoring = setup.core_with(&Config::new(1), Rig::default(), 20);
    assert_eq!(floor(&ignoring, K), None);
    assert!(ignoring.with(|_, rig| rig.key_names_told.is_empty()));
}

/// Asserts that a requeue the store refused, written again after the estimator lowered the
/// floor of its key, writes the floor as the scheduler holds it now and not the higher one it
/// was taken with. Catches: a retried requeue that puts a stale floor back over the lowered
/// row.
#[test]
fn a_requeue_retried_after_a_newer_kill_writes_the_floor_as_it_stands() {
    let setup = Setup::new();
    let core = setup.core_with(&on(), named("est/k"), 0);
    up(&core, 0, "node-a", "d1");
    let killed = keyed(&core, 1, 1, K);
    let other = submit(&core, 2, 2);

    // The first kill's requeue is refused; the floor it raised is lowered, and the next
    // give-up (another operation's) learns of it.
    setup
        .store
        .fail_writes(Some(&keys::queue(killed.operation)));
    kill(&core, 5, killed.operation);
    let raised = floor(&core, K).expect("raised");
    assert!(floor_rows(&setup).is_empty(), "the requeue was refused");
    core.with(|_, rig| rig.to_lower.push(Lowered::new(K, raised, GIB)));
    kill(&core, 6, other.operation);
    assert_eq!(floor(&core, K), Some(GIB));
    assert_eq!(floor_rows(&setup), [(K, GIB)]);

    setup.store.fail_writes(None);
    block_on(core.tick(at(7)));

    assert_eq!(
        floor_rows(&setup),
        [(K, GIB)],
        "the retry leaves the lowered row"
    );
}

/// Asserts that a requeue that forgot a key, retried after a newer one raised that key
/// again, neither deletes the live row nor writes the key it raised. Catches: a retried
/// eviction that deletes the row of a key raised since, and a retried raise that brings back
/// a key forgotten since.
#[test]
fn an_eviction_retried_after_a_reraise_keeps_the_row() {
    let (first_key, second_key) = (K, K + 1);
    let setup = Setup::new();
    let core = setup.core_with(&on().with_memory_floors(1), Rig::default(), 0);
    up(&core, 0, "node-a", "d1");
    let first = keyed(&core, 1, 1, first_key);
    kill(&core, 5, first.operation);
    assert_eq!(floor_names(&setup), [keys::key_floor(first_key)]);

    // A kill under another key forgets the first key; its requeue is refused.
    let second = keyed(&core, 6, 2, second_key);
    setup
        .store
        .fail_writes(Some(&keys::queue(second.operation)));
    kill(&core, 7, second.operation);
    assert_eq!(floor(&core, first_key), None, "forgotten");

    // A new request under the first key raises it again, forgetting the second.
    let third = keyed(&core, 8, 3, first_key);
    kill(&core, 9, third.operation);
    assert_eq!(floor(&core, second_key), None, "forgotten");
    let raised = floor(&core, first_key).expect("raised again");
    assert_eq!(floor_names(&setup), [keys::key_floor(first_key)]);

    setup.store.fail_writes(None);
    block_on(core.tick(at(10)));

    assert_eq!(floor_rows(&setup), [(first_key, raised)]);
}

/// Asserts that a floor the estimator lowers is lowered in the scheduler at once, kept for
/// the store when the store refuses it and written at a later tick, and that an ask of 0
/// forgets the floor and deletes the row. Catches: a lowered floor that never reaches the
/// scheduler or the store, one the store refused that is forgotten, and a forgotten floor
/// whose row stays.
#[test]
fn lowered_floor_is_applied_and_written_and_retried_at_tick() {
    let setup = Setup::new();
    let core = setup.core_with(&on(), named("est/k"), 0);
    up(&core, 0, "node-a", "d1");
    let killed = keyed(&core, 1, 1, K);
    let (first, second) = (submit(&core, 2, 2), submit(&core, 3, 3));
    let third = submit(&core, 4, 4);
    kill(&core, 5, killed.operation);
    let raised = floor(&core, K).expect("raised");
    assert_eq!(floor_rows(&setup), [(K, raised)]);

    setup.store.fail_writes(Some("keyfloor/"));
    core.with(|_, rig| rig.to_lower.push(Lowered::new(K, raised, 2 * GIB)));
    complete(&core, 6, first.operation);
    assert_eq!(floor(&core, K), Some(2 * GIB), "lowered at once");
    assert_eq!(
        floor_rows(&setup),
        [(K, raised)],
        "the store refused the row"
    );
    // Lowered again while the first row waits: the newer row replaces it.
    core.with(|_, rig| rig.to_lower.push(Lowered::new(K, 2 * GIB, GIB)));
    complete(&core, 7, second.operation);
    assert_eq!(floor(&core, K), Some(GIB));

    setup.store.fail_writes(None);
    block_on(core.tick(at(8)));
    assert_eq!(floor_rows(&setup), [(K, GIB)], "written at the tick");
    assert_eq!(
        setup.rows(9).load().expect("loaded").key_names,
        [(K, "est/k".to_owned())],
        "the row keeps its summary name"
    );

    core.with(|_, rig| rig.to_lower.push(Lowered::new(K, GIB, 0)));
    complete(&core, 9, third.operation);
    assert_eq!(floor(&core, K), None);
    assert!(floor_names(&setup).is_empty(), "the row is deleted");
}

/// Asserts that a lowered row still waiting for the store is dropped when a kill raises the
/// floor of its key again: the requeue writes the higher floor, and the late lower row does
/// not overwrite it. Catches: a pending row written over a floor raised since.
#[test]
fn a_pending_lowered_row_does_not_overwrite_a_floor_raised_since() {
    let setup = Setup::new();
    let core = setup.core_with(&on(), Rig::default(), 0);
    up(&core, 0, "node-a", "d1");
    let killed = keyed(&core, 1, 1, K);
    let first = submit(&core, 2, 2);
    kill(&core, 5, killed.operation);
    let raised = floor(&core, K).expect("raised");

    setup.store.fail_writes(Some("keyfloor/"));
    core.with(|_, rig| rig.to_lower.push(Lowered::new(K, raised, 2 * GIB)));
    complete(&core, 6, first.operation);
    assert_eq!(floor(&core, K), Some(2 * GIB));
    let again = keyed(&core, 7, 3, K);
    assert_eq!(booked(&core, again.operation), Some(2 * GIB));
    kill(&core, 8, again.operation);
    assert_eq!(
        floor(&core, K),
        Some(raised),
        "raised again past the lowered floor"
    );

    setup.store.fail_writes(None);
    block_on(core.tick(at(9)));

    assert_eq!(floor_rows(&setup), [(K, raised)]);
}

/// Asserts that a floor a kill raised after the estimator last saw the key is not lowered to
/// the estimator's older value, and that a key without a floor has nothing lowered or
/// written. Catches: a decay that undoes a kill the estimator never saw (the action is killed
/// again), and a row written for a key the scheduler holds no floor for.
#[test]
fn lower_skipped_when_a_newer_kill_raised_it() {
    let setup = Setup::new();
    let core = setup.core_with(&on(), Rig::default(), 0);
    up(&core, 0, "node-a", "d1");
    let killed = keyed(&core, 1, 1, K);
    let first = submit(&core, 2, 2);
    kill(&core, 5, killed.operation);
    let raised = floor(&core, K).expect("raised");

    core.with(|_, rig| {
        rig.to_lower.push(Lowered::new(K, raised - 1, GIB));
        rig.to_lower.push(Lowered::new(K + 1, 8 * GIB, GIB));
    });
    complete(&core, 6, first.operation);

    assert_eq!(floor(&core, K), Some(raised));
    assert_eq!(floor(&core, K + 1), None);
    assert_eq!(floor_rows(&setup), [(K, raised)]);
}

/// Asserts that a core a handover fenced lowers the floor in its scheduler and writes no row,
/// not at once and not at a later tick. Catches: a fenced core that writes to a store its
/// successor owns.
#[test]
fn fenced_core_lowers_and_writes_nothing() {
    let setup = Setup::new();
    let core = setup.core_with(&on(), Rig::default(), 0);
    up(&core, 0, "node-a", "d1");
    let killed = keyed(&core, 1, 1, K);
    let first = submit(&core, 2, 2);
    kill(&core, 5, killed.operation);
    let raised = floor(&core, K).expect("raised");
    let lease = lease_of(&core, first.operation);

    core.fence();
    core.with(|_, rig| rig.to_lower.push(Lowered::new(K, raised, GIB)));
    finish(&core, 6, "node-a", first.operation, lease);
    core.resume();
    block_on(core.tick(at(7)));

    assert_eq!(floor(&core, K), Some(GIB));
    assert_eq!(floor_rows(&setup), [(K, raised)]);
}

/// Asserts that the adapter is told each lease the scheduler gives up, on a core with and
/// without a store, and that a floor it lowers on a core without a store is applied.
/// Catches: a give-up the adapter never learns of (the estimator misses the kill), and a
/// lowering that needs a store.
#[test]
fn the_adapter_learns_from_each_give_up() {
    let core = core_by(&on());
    up(&core, 0, "node-a", "d1");
    let killed = keyed(&core, 1, 1, K);
    let other = submit(&core, 2, 2);
    let (lease_a, lease_b) = (
        lease_of(&core, killed.operation),
        lease_of(&core, other.operation),
    );

    kill(&core, 5, killed.operation);
    let raised = floor(&core, K).expect("raised");
    core.with(|_, rig| rig.to_lower.push(Lowered::new(K, raised, GIB)));
    kill(&core, 6, other.operation);

    assert_eq!(core.with(|_, rig| rig.learned.clone()), [lease_a, lease_b]);
    assert_eq!(floor(&core, K), Some(GIB));
}

/// What a script leaves in a store and tells its rig: the store's rows (key and value) and
/// the hooks called.
fn trace_of(config: &Config, key: Option<u64>) -> (Vec<(String, Vec<u8>)>, Vec<Call>) {
    let setup = Setup::new();
    let core = setup.core_with(config, named("est/k"), 0);
    up(&core, 0, "node-a", "d1");
    let admitted = core
        .submit_keyed(at(1), request(1), &1u64.to_be_bytes(), key, enrol)
        .expect("submitted");
    submit_request(&core, 2, request(2)).expect("submitted");
    kill(&core, 5, admitted.operation);
    block_on(core.tick(at(6)));
    let rows = setup.store.rows_of("");
    let rows = rows.into_iter().map(|(name, row)| (name, row.value));
    (rows.collect(), calls(&core))
}

/// Asserts that a core with size keys off, given keys with its submissions, leaves the rows
/// and calls a core given none leaves, and writes no `opkey/` or `keyfloor/` row. Catches: a
/// key that changes anything on a core that has them off (a rollback to off would behave
/// differently from a build that never had them).
#[test]
fn size_keys_off_core_is_byte_identical() {
    let off = Config::new(1).with_size_keys(false);
    let keyed_run = trace_of(&off, Some(K));
    let plain_run = trace_of(&Config::new(1), None);

    assert_eq!(keyed_run, plain_run);
    assert!(
        keyed_run
            .0
            .iter()
            .all(|(name, _)| !name.starts_with("opkey/") && !name.starts_with("keyfloor/"))
    );
}

/// Asserts that a lowered floor names its key, the floor the estimator saw and the floor
/// asked, and compares and prints by them. Catches: fields swapped in the constructor (a
/// decay would then be judged against the wrong floor).
#[test]
fn a_lowered_floor_names_its_key_what_was_seen_and_what_is_asked() {
    let lowered = Lowered::new(K, 8 * GIB, 2 * GIB);
    assert_eq!(lowered.clone(), lowered);
    assert_ne!(lowered, Lowered::new(K, 2 * GIB, 8 * GIB));
    let shown = format!("{lowered:?}");
    assert!(shown.contains(&format!("before: {}", 8 * GIB)), "{shown}");
    assert!(shown.contains(&format!("ask: {}", 2 * GIB)), "{shown}");
}

/// An adapter that does nothing and takes the default of every hook that has one.
struct Bare;

impl Adapter for Bare {
    fn stream_open(&self, _: &WorkerId, _: Stream) -> bool {
        true
    }

    fn name(&self, _: &View<'_>, operation: OperationId) -> String {
        operation.0.to_string()
    }

    fn phased(&mut self, _: &WorkerId, _: &NodePhase, _: &NodePhase) {}

    fn offer(&mut self, _: &View<'_>, _: &LeaseGrant) {}

    fn start(&mut self, _: &View<'_>, _: &StartLease) {}

    fn cancel(&mut self, _: &View<'_>, _: &WorkerId, _: LeaseId) {}

    fn show(&mut self, _: &View<'_>, _: OperationId, _: &Stage) {}

    fn refuse(&mut self, _: &View<'_>, _: &Refusal) {}

    fn requeued(&mut self, _: &Requeue) {}

    fn settle(&mut self, _: &View<'_>, _: &Answer, _: Option<&WorkerId>) -> Delivery {
        Box::pin(std::future::ready(()))
    }

    fn forget_dropped(&mut self, _: &dyn Fn(OperationId) -> bool) {}

    fn forget_parcel(&mut self, _: LeaseId) {}

    fn retain_parcels(&mut self, _: &dyn Fn(LeaseId) -> bool) {}

    fn ending(&self, _: &Effect, _: Duration) -> Option<Ending> {
        None
    }

    fn adopted(
        &mut self,
        _: OperationId,
        _: WaiterId,
        _: Option<&[u8]>,
        _: &Request,
        _: Option<&LeaseGrant>,
        _: Option<u64>,
    ) {
    }

    fn waiting_among(&self, _: &[WaiterId]) -> Vec<WaiterId> {
        Vec::new()
    }

    fn show_callers(&mut self, _: &[WaiterId], _: &Stage) {}

    fn keep_finished(&mut self, _: OperationId, _: Vec<WaiterId>) {}

    fn followed(&mut self, _: OperationId, _: &[u8]) {}

    fn watched(&mut self) -> Vec<OperationId> {
        Vec::new()
    }

    fn show_watched(&mut self, _: OperationId, _: &Stage) {}
}

/// Asserts that an adapter that names no summary and lowers nothing (the defaults) still has
/// its floors raised and written: the row is there and names no summary. Catches: a default
/// hook that blocks the floor's write, and a core that needs the adapter to implement the
/// size-key hooks.
#[test]
fn an_adapter_with_the_default_hooks_has_its_floors_written_without_a_name() {
    let setup = Setup::new();
    let core = Core::open(&on(), Bare, setup.storage(), at(0)).expect("opened");
    let node = worker("node-a");
    let (instance, predecessor) = (DaemonInstance::new("d1"), DaemonInstance::new(""));
    core.register(at(0), &node, instance, predecessor, reported(8), |_, _| {})
        .expect("registered");
    let admitted = core
        .submit_keyed(at(1), request(1), &[1], Some(K), |_, _| {})
        .expect("submitted");
    let report = Report {
        operation: admitted.operation,
        lease: LeaseId::new(1, 0),
        outcome: Outcome::Failed(Failure::OutOfMemory),
    };

    block_on(core.report(at(5), &node, report, |_| {}));

    let raised = core
        .with(|view, _| view.sched().key_floor(K))
        .expect("raised");
    assert_eq!(floor_rows(&setup), [(K, raised)]);
    assert!(setup.rows(9).load().expect("loaded").key_names.is_empty());
}

/// Asserts that a lowering does not overwrite a key floor row another frontend raised after
/// this one's kill: the scheduler here is lowered, and the row keeps the higher floor.
/// Catches: a lowered row written without regard to the row in the store (the other
/// frontend's raise is lost, and the next start books the lower floor and is killed again).
#[test]
fn a_lowering_does_not_overwrite_a_floor_another_frontend_raised() {
    let setup = Setup::new();
    let core = setup.core_with(&on(), Rig::default(), 0);
    up(&core, 0, "node-a", "d1");
    let killed = keyed(&core, 1, 1, K);
    let first = submit(&core, 2, 2);
    kill(&core, 5, killed.operation);
    let raised = floor(&core, K).expect("raised");
    let higher = KeyFloor {
        key: K,
        ask: 2 * raised,
        stamp: 1 << 40,
    };
    setup
        .rows(9)
        .put_key_floor(&higher, None)
        .expect("written by the other frontend");

    core.with(|_, rig| rig.to_lower.push(Lowered::new(K, raised, 2 * GIB)));
    complete(&core, 6, first.operation);

    assert_eq!(floor(&core, K), Some(2 * GIB), "lowered here");
    assert_eq!(floor_rows(&setup), [(K, 2 * raised)], "the higher row kept");
}

/// Asserts that a start with size keys on is refused over a `keyfloor/` row that is not one,
/// and that a start with them off ignores the row and books no floor. Catches: a bad floor
/// row read as none with size keys on (the damage hidden), and a start with size keys off
/// that is stopped by a row it never reads (a rollback would not start).
#[test]
fn a_bad_keyfloor_row_refuses_a_start_with_size_keys_on_and_is_ignored_with_them_off() {
    let setup = Setup::new();
    setup.store.put(&keys::key_floor(K), vec![9, 9, 9]);

    let opened = Core::open(&on(), Rig::default(), setup.storage(), at(5));
    assert!(matches!(opened.err(), Some(StateError::Corrupt { .. })));

    let ignoring = setup.core_with(&Config::new(1), Rig::default(), 6);
    assert_eq!(floor(&ignoring, K), None);
}

/// Asserts that a core with size keys off never calls `learned_requeue` or `lowered` on the
/// adapter, through a give-up and a report, while one with them on calls both. Catches: the
/// hooks called on every give-up and report of a core that has no key floors.
#[test]
fn size_keys_off_never_asks_the_adapter_to_learn_or_lower() {
    let counts = |config: &Config| {
        let setup = Setup::new();
        let core = setup.core_with(config, Rig::default(), 0);
        up(&core, 0, "node-a", "d1");
        let killed = keyed(&core, 1, 1, K);
        let first = submit(&core, 2, 2);
        kill(&core, 5, killed.operation);
        complete(&core, 6, first.operation);
        core.with(|_, rig| (rig.learned.len(), rig.lowered_asked))
    };

    assert_eq!(counts(&Config::new(1)), (0, 0));
    let (learned, asked) = counts(&on());
    assert_eq!(learned, 1);
    assert!(
        asked >= 2,
        "asked after the give-up and the report: {asked}"
    );
}
