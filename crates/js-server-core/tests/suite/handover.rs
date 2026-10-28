//! A core handing its work to the other frontends of its state store: the writes a fenced core
//! still owes ([`Core::flush_unsettled`]), a store that says a newer writer holds it
//! ([`Core::superseded`]), the callers a fenced core answers from the store ([`Core::follow`]),
//! and two frontends over one store from the first request to the takeover.

use std::future::ready;
use std::time::Duration;

use js_server_core::{Core, Held, NotQueued, Reason, Stage, Standing, Taken};
use js_types::sched::{LeaseId, OperationId};

use crate::common::{
    Call, Rig, at, beat, block_on, calls, close, lease, logged, reported, request, shown_to,
    started, submit, submit_request, up, up_after, worker,
};
use crate::recover::finish;
use crate::stored::Setup;

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("small")
}

/// Drives [`Core::flush_unsettled`] with a script that says, each time the store refused
/// again, whether to try again; returns what is left and how many times it was asked.
fn flush(core: &Core<Rig>, script: &mut dyn FnMut() -> bool) -> (Vec<OperationId>, usize) {
    let mut asked = 0;
    let left = block_on(core.flush_unsettled(|| {
        asked += 1;
        ready(script())
    }));
    (left, asked)
}

/// A core with a daemon, its lease given up for silence, and the store refusing the write of
/// the give-up: the operation it is for.
fn unsettled(setup: &Setup) -> (Core<Rig>, OperationId) {
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 1, 1);
    setup.store.fail_writes(Some("loss/"));
    block_on(core.tick(at(millis(js_sched::fence::LEASE_GRACE) + 1)));
    (core, admitted.operation)
}

/// Asserts that a core with nothing left unwritten flushes at once without waiting. Catches: a
/// handover that waits out its deadline with nothing to write.
#[test]
fn a_flush_with_nothing_unwritten_does_not_wait() {
    let setup = Setup::new();
    let core = setup.core(1);

    let (left, asked) = flush(&core, &mut || panic!("nothing to wait for"));

    assert_eq!((left, asked), (Vec::new(), 0));
}

/// Asserts that a fenced core writes the requeues the store refused once the store takes them,
/// trying again each time it is told to, so the successor recovers each operation queued and
/// not under the lease this core gave up. Catches: a handover that leaves an operation under a
/// lease nobody runs.
#[test]
fn a_fenced_core_writes_the_requeues_the_store_refused_when_it_takes_them() {
    let setup = Setup::new();
    let (core, operation) = unsettled(&setup);
    assert_eq!(
        setup.store.rows_of("lease/").len(),
        1,
        "the give-up is not written"
    );
    core.fence();

    let (left, asked) = flush(&core, &mut || {
        setup.store.fail_writes(None);
        true
    });

    assert_eq!((left, asked), (Vec::new(), 1));
    assert!(setup.store.rows_of("lease/").is_empty());
    assert_eq!(
        setup.store.rows_of("queue/").len(),
        1,
        "queued for the successor"
    );
    assert_eq!(setup.store.rows_of("loss/").len(), 1);
    assert_eq!(unfinished_of(&core), [operation]);
}

fn unfinished_of(core: &Core<Rig>) -> Vec<OperationId> {
    core.with(|view, _| view.sched().unfinished().collect())
}

/// Asserts that a flush stops at its deadline when the store keeps refusing, and returns the
/// operations whose requeues are not written. Catches: a handover that waits for ever on a
/// store that is gone.
#[test]
fn a_flush_gives_up_at_its_deadline_and_names_what_it_could_not_write() {
    let setup = Setup::new();
    let (core, operation) = unsettled(&setup);

    let mut tries = 0;
    let (left, asked) = flush(&core, &mut || {
        tries += 1;
        tries < 3
    });

    assert_eq!(left, [operation]);
    assert_eq!(asked, 3);
    assert_eq!(setup.store.rows_of("lease/").len(), 1);
}

/// Asserts that a flush stops at once when the store says a newer writer holds it: nothing may
/// be written then, and it does not wait. Catches: a retired core that goes on writing over its
/// successor's rows.
#[test]
fn a_flush_stops_at_once_when_a_newer_writer_holds_the_store() {
    let setup = Setup::new();
    let (core, operation) = unsettled(&setup);
    setup.store.fail_writes(None);
    setup.store.set_fenced(true);
    // The next feed's write of the give-up finds the store held by a newer writer.
    block_on(core.tick(at(millis(js_sched::fence::LEASE_GRACE) + 2)));
    assert!(core.superseded());
    // The store takes writes again, but this core was told it is not the writer.
    setup.store.set_fenced(false);

    let (left, asked) = flush(&core, &mut || panic!("a superseded core does not wait"));

    assert_eq!((left, asked), (vec![operation], 0));
    assert_eq!(
        setup.store.rows_of("lease/").len(),
        1,
        "nothing was written"
    );
}

/// Asserts that a store that says a newer writer holds it supersedes the core: a request is
/// refused for that reason, the scheduler is told the store is unavailable and stays so when
/// the store takes writes again, and nothing is granted. Catches: two writers that both place
/// work, and a superseded core that recovers by itself.
#[test]
fn a_store_a_newer_writer_holds_stops_a_core_for_good() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    assert!(!core.superseded());
    setup.store.set_fenced(true);

    let refused = submit_request(&core, 1, request(1));

    assert_eq!(refused, Err(NotQueued::Superseded));
    assert_eq!(
        refused.expect_err("refused").to_string(),
        "this scheduler was superseded by a newer writer of the state store"
    );
    assert!(core.superseded());
    block_on(core.tick(at(2)));
    assert!(core.store_down(), "the scheduler is told");
    setup.store.set_fenced(false);
    block_on(core.tick(at(3)));
    assert!(core.store_down(), "a superseded core never recovers");
    block_on(core.tick(at(4)));
    assert_eq!(started(&core), []);
}

/// Asserts that once a newer writer holds the store every effect held for it is dropped and
/// counted, not retried: the first refused record, and the records behind it. Catches: a
/// superseded core that holds records for ever, or writes them later over its successor's.
#[test]
fn a_superseded_core_drops_every_record_it_held() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    setup.store.fail_writes(Some("lease/"));
    submit(&core, 1, 1);
    submit(&core, 2, 2);
    assert_eq!(core.write_behind(), 2, "two grants are held");
    setup.store.fail_writes(None);
    setup.store.set_fenced(true);

    block_on(core.tick(at(3)));

    assert!(core.superseded());
    assert_eq!(core.write_behind(), 0);
    assert_eq!(
        core.dropped_writes(),
        2,
        "the refused record and the one behind it"
    );
    assert_eq!(started(&core), []);
}

/// Asserts that a fenced core answers, from the state store, the callers it still holds: an
/// operation another frontend leased is shown executing, one it finished its result, and the
/// count of callers still waiting follows; a caller that gave up is not counted. Catches:
/// callers of a retired frontend that wait for ever.
#[test]
fn a_fenced_core_answers_the_callers_it_holds_from_the_store() {
    let setup = Setup::new();
    let stateless = crate::common::core();
    assert_eq!(stateless.follow().expect("no store to read"), 0);
    let core = setup.core(1);
    assert_eq!(core.follow().expect("read"), 0, "nothing is held");
    let queued = submit(&core, 1, 1);
    let second = submit(&core, 2, 2);
    core.fence();
    assert_eq!(core.follow().expect("read"), 2, "queued: nothing to show");
    assert_eq!(shown_to(&core, queued.waiter), None);
    setup.store.fail_reads(Some("done/"));
    let unread = core.follow();
    assert!(unread.is_err(), "a store that cannot be read is an error");
    setup.store.fail_reads(None);

    let other = setup.rows(7);
    other
        .granted(&grant(queued.operation, 0, "node-b"))
        .expect("leased by the successor");
    assert_eq!(core.follow().expect("read"), 2);
    assert_eq!(shown_to(&core, queued.waiter), Some(Stage::Executing));

    other
        .finished(
            queued.operation,
            lease(5, 0),
            &ending("finished by the successor"),
        )
        .expect("finished by the successor");
    assert_eq!(core.follow().expect("read"), 1, "one caller is answered");
    let done = Stage::Done(b"finished by the successor".to_vec());
    assert_eq!(shown_to(&core, queued.waiter), Some(done));

    core.with(|_, rig| rig.gone.insert(second.waiter));
    assert_eq!(
        core.follow().expect("read"),
        0,
        "a caller that left is not counted"
    );
}

fn grant(operation: OperationId, seq: u64, node: &str) -> js_types::sched::LeaseGrant {
    js_types::sched::LeaseGrant {
        lease: lease(5, seq),
        operation,
        worker: worker(node),
        cost: js_types::sched::usage::LeaseCost::default(),
    }
}

fn ending(text: &str) -> js_store::Ending {
    js_store::Ending::new(text.as_bytes().to_vec(), Duration::from_secs(600))
}

/// Asserts that an operation the store holds no trace of keeps its callers waiting, and that
/// they are not counted as answered: the operation's rows are neither there nor ended, which
/// only a store a release wrote in two transactions leaves. Catches: callers answered, or
/// dropped, on a guess.
#[test]
fn an_operation_the_store_has_no_trace_of_keeps_its_callers_waiting() {
    let setup = Setup::new();
    let core = setup.core(1);
    let queued = submit(&core, 1, 1);
    core.fence();
    setup.store.remove("op/0000000000000000");
    setup.store.remove("queue/0000000000000000");

    let log = logged(|| {
        for _ in 0..3 {
            assert_eq!(core.follow().expect("read"), 1, "the caller keeps waiting");
        }
    });
    assert_eq!(shown_to(&core, queued.waiter), None);
    let warning = "no trace in the state store: callers keep waiting";
    assert_eq!(
        log.matches(warning).count(),
        1,
        "once, at the second look: {log}"
    );

    // Its result, when it comes, answers them.
    setup.ended_elsewhere(queued.operation, b"late result");
    assert_eq!(core.follow().expect("read"), 0);
    let done = Stage::Done(b"late result".to_vec());
    assert_eq!(shown_to(&core, queued.waiter), Some(done));
}

/// Asserts that the operations clients ask for by name, which the core does not hold, are
/// shown what the store says of them at a follow and at an intake poll: nothing while queued,
/// executing while leased, the result once it is there, and not looked at again after. Catches:
/// a name that is never answered by the frontend a client asked, and one looked at for ever.
#[test]
fn the_operations_asked_for_by_name_are_followed_through_the_store() {
    let setup = Setup::new();
    let core = setup.core(1);
    let other = setup.rows(7);
    other
        .submit(OperationId(77), &request(1), &[], None)
        .expect("queued by another frontend");
    core.with(|_, rig| rig.watching.insert(OperationId(77), None));
    let shown = |core: &Core<Rig>| {
        calls(core)
            .into_iter()
            .filter(|call| matches!(call, Call::ShowWatched(..)))
            .collect::<Vec<_>>()
    };

    core.fence();
    core.follow().expect("read");
    assert_eq!(shown(&core), [], "queued: nothing to show");

    other
        .granted(&grant(OperationId(77), 0, "node-b"))
        .expect("leased");
    core.follow().expect("read");
    assert_eq!(
        shown(&core),
        [Call::ShowWatched(OperationId(77), Stage::Executing)]
    );

    setup.store.fail_reads(Some("done/"));
    core.follow()
        .expect("an unread watch is looked at again later");
    assert_eq!(shown(&core).len(), 1, "nothing was shown of it");
    setup.store.fail_reads(None);
    core.resume();
    other
        .finished(OperationId(77), lease(5, 0), &ending("named result"))
        .expect("finished");
    core.intake(at(1));
    let done = Call::ShowWatched(OperationId(77), Stage::Done(b"named result".to_vec()));
    assert_eq!(shown(&core).last(), Some(&done));
    let before = calls(&core).len();
    core.intake(at(2));
    assert_eq!(
        calls(&core).len(),
        before,
        "looked at again after its result"
    );
}

/// Two frontends over one store, from a request to a takeover. Asserts, in one story, what the
/// split of the work between them rests on: a request for work the other frontend runs joins it
/// and follows it through the store (`Held::Elsewhere`); queued work the first cannot place is
/// taken in by the second and run on its own node, while the first forgets it and follows; when
/// the first is handed over (fenced) its daemon reconnects to the second, which takes up the
/// lease the daemon was running and takes the result once; the fenced first answers its callers
/// from the store; no lease is granted twice by either; and every operation is answered once.
/// Catches: work run twice, a result written twice, a lease id reused across frontends, and
/// callers of a retired frontend left waiting.
#[test]
fn two_frontends_share_work_and_the_second_takes_over_after_the_first_is_fenced() {
    let setup = Setup::new();
    let (a, b) = (setup.core(1), setup.core(2));
    let stream_a = up_after(&a, 0, "node-a", ("d1", ""), reported(1));
    up_after(&b, 0, "node-b", ("d2", ""), reported(1));

    // A runs the first operation on its node; B's request for the same work joins it.
    let first = submit(&a, 1, 1);
    let joined = submit_request(&b, 2, request(1)).expect("joined");
    assert_eq!(joined.held, Held::Elsewhere(first.operation));
    assert_eq!(started(&a), [lease(1, 0)]);
    assert_eq!(started(&b), []);
    // A's node has no room for a second operation: it waits in the store for a frontend with room.
    let second = submit(&a, 3, 2);
    assert_eq!(a.queued_operations(), 1);

    // B takes the queued operation in and runs it on its own node; A forgets it and follows.
    b.intake(at(4));
    block_on(b.tick(at(5)));
    assert_eq!(started(&b), [lease(1, 1024)]);
    a.intake(at(6));
    assert_eq!(a.queued_operations(), 0, "A handed the operation off");
    assert_eq!(shown_to(&a, second.waiter), Some(Stage::Executing));

    // A is handed over: it takes nothing more, and its daemon reconnects to B.
    a.fence();
    assert!(beat(&a, 7, ("node-a", stream_a), 1, &[lease(1, 0)]));
    let stream_b = up_after(&b, 8, "node-a", ("d1", ""), reported(1));
    assert!(
        calls(&b)
            .iter()
            .any(|call| matches!(call, Call::Adopted(op, ..) if *op == first.operation))
    );
    assert!(beat(&b, 9, ("node-a", stream_b), 1, &[lease(1, 0)]));
    assert_eq!(
        b.standing(&worker("node-a"), lease(1, 0)),
        Ok(Standing::Current(first.operation))
    );
    // A report that reaches the fenced core is not taken.
    let late = finish(&a, 10, "node-a", first.operation, lease(1, 0));
    assert_ne!(late, Taken::Accepted);
    assert!(setup.store.rows_of("done/").is_empty());
    assert_eq!(
        a.follow().expect("read"),
        1,
        "A's caller waits: its operation runs"
    );
    assert_eq!(shown_to(&a, first.waiter), Some(Stage::Executing));

    // B takes the result of the lease A granted, once; the node's second report is a duplicate.
    assert_eq!(
        finish(&b, 11, "node-a", first.operation, lease(1, 0)),
        Taken::Accepted
    );
    assert_eq!(
        b.standing(&worker("node-a"), lease(1, 0)),
        Ok(Standing::Refused(Reason::Duplicate))
    );
    assert_eq!(
        finish(&b, 13, "node-b", second.operation, lease(1, 1024)),
        Taken::Accepted
    );

    // The callers of both frontends are answered from what B wrote.
    b.intake(at(14));
    let done = |text: &str| Some(Stage::Done(text.as_bytes().to_vec()));
    assert_eq!(shown_to(&b, joined.waiter), done("done 1.0"));
    assert_eq!(
        a.follow().expect("read"),
        0,
        "A's callers are answered from the store"
    );
    assert_eq!(shown_to(&a, first.waiter), done("done 1.0"));
    drop(a);

    // B goes on alone: new work is placed under a lease no earlier one used.
    let third = submit(&b, 15, 3);
    assert_ne!(third.operation, first.operation);
    let leases: Vec<LeaseId> = started(&b);
    assert_eq!(
        leases,
        [lease(1, 1024), lease(1, 1025)],
        "no lease id twice"
    );

    // Each operation was answered once, and only by the frontend that took the result.
    let settled = |core: &Core<Rig>| {
        calls(core)
            .into_iter()
            .filter_map(|call| match call {
                Call::Settle { operation, .. } => Some(operation),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(settled(&b), [first.operation, second.operation]);
    assert_eq!(setup.store.rows_of("done/").len(), 2);
    assert!(
        setup.store.rows_of("lease/").len() <= 1,
        "only the third operation holds a lease"
    );
    close(&b, "node-a", stream_b);
}
