//! A state store that fails: the rows the core holds back, what it tells the scheduler, what
//! it writes again, and what it gives up on (moved from the farm core's tests of the same
//! rules, with the store's failures played by a store that fails on request).

use std::time::Duration;

use js_sched::fence::LEASE_GRACE;
use js_server_core::{Config, Core, NotQueued, Report, Reported, Taken};
use js_types::sched::{Failure, Outcome, Resources};

use crate::common::{
    Call, GIB, Rig, at, beat, block_on, calls, caps, lease, request, started, submit,
    submit_request, up, up_after, worker,
};
use crate::recover::finish;
use crate::stored::Setup;

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("small")
}

/// A serving host with `cpus` cores and 2 TiB.
fn host(cpus: u64) -> Reported {
    Reported::serving(Resources::new(cpus * 1_000, 2_048 * GIB), caps())
}

/// Asserts that a store that cannot be reached when a worker registers is noticed by the next
/// input: the scheduler is told, nothing is placed while the store is gone, and the claim of
/// the node, the session row and the grant are made once a write takes again. Catches: work
/// placed with no way to write it down, and a node whose claim was lost to an outage.
#[test]
fn a_store_that_is_gone_holds_the_placement_until_a_write_takes_again() {
    let setup = Setup::new();
    let core = setup.core(1);
    submit(&core, 0, 1);
    setup.store.set_down(true);

    let stream = up(&core, 1, "node-a", "d1");
    assert_eq!(started(&core), []);
    block_on(core.tick(at(2)));
    assert!(core.store_down(), "the scheduler is told");
    assert_eq!(started(&core), []);
    assert!(setup.store.rows_of("owner/").is_empty());

    setup.store.set_down(false);
    block_on(core.tick(at(3)));
    assert!(!core.store_down());
    assert_eq!(started(&core), [lease(1, 0)]);
    assert_eq!(
        setup.store.rows_of("owner/").len(),
        1,
        "the node is claimed"
    );
    assert_eq!(setup.store.rows_of("lease/").len(), 1);
    // The session row the registration could not write is put by the next heartbeat.
    assert!(setup.store.rows_of("session/").is_empty());
    assert!(beat(&core, 4, ("node-a", stream), 1, &[lease(1, 0)]));
    assert_eq!(setup.store.rows_of("session/").len(), 1);
}

/// Asserts that a grant the store refuses is held, with the effects behind it, and written
/// by the next input that finds the store taking it; and that a refusal that does not hide
/// the store (it takes the ping) does not leave the scheduler told it is gone. Catches: a
/// `Start` before its grant, and a held grant lost.
#[test]
fn a_grant_the_store_refuses_is_written_when_it_takes_it_again() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    setup.store.fail_writes(Some("lease/"));

    submit(&core, 1, 1);
    assert_eq!((core.write_behind(), started(&core)), (1, vec![]));
    block_on(core.tick(at(2)));
    assert_eq!(core.write_behind(), 1, "refused again");
    assert!(!core.store_down(), "the ping took");

    setup.store.fail_writes(None);
    block_on(core.tick(at(3)));
    assert_eq!(core.write_behind(), 0);
    assert_eq!(started(&core), [lease(1, 0)]);
}

/// Asserts that a core reserves its next block of lease sequences before the last is used,
/// numbers from the new block, and tries again at the next input when the store refuses the
/// reservation. Catches: a core that runs out of sequences (it places nothing past its
/// block), and one that numbers two leases alike across the blocks.
#[test]
fn the_next_block_of_lease_sequences_is_reserved_before_the_last_is_used() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    // Room for every lease: a node of 2000 cores.
    up_after(&core, 0, "node-big", ("d2", ""), host(2_000));
    // More than half of the block of 1024 is used when the 514th request comes.
    for n in 1..=513 {
        submit(&core, 1, n);
    }
    assert_eq!(started(&core).last(), Some(&lease(1, 512)));

    setup.store.fail_writes(Some("meta/next_seq"));
    submit(&core, 2, 514);
    assert_eq!(
        started(&core).last(),
        Some(&lease(1, 513)),
        "the old block serves while the store refuses"
    );
    setup.store.fail_writes(None);
    submit(&core, 3, 515);

    let all = started(&core);
    assert_eq!(
        all.last(),
        Some(&lease(1, 1024)),
        "numbered from the new block"
    );
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, all, "no lease id twice, in order");
}

/// Asserts that a lease given up for silence is written down in the same transaction as the
/// operation's return to the queue, with the count of its lost leases. Catches: a give-up
/// the store never hears of (the operation would run on after a restart).
#[test]
fn a_lease_given_up_for_silence_is_queued_again_in_the_store_with_its_loss() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    assert_eq!(setup.store.rows_of("lease/").len(), 1);

    block_on(core.tick(at(millis(LEASE_GRACE) + 1)));

    let losses = setup.store.rows_of("loss/");
    assert_eq!(losses.len(), 1);
    assert_eq!(losses[0].1.value, 1u64.to_be_bytes());
    assert!(
        calls(&core).contains(&Call::Requeued(lease(1, 0))),
        "{:?}",
        calls(&core)
    );
}

/// Asserts that a kill for memory writes the floor it raised and the operation's fate with
/// its return to the queue, that a restart takes the raised booking up again, and that the
/// kill is not counted as a lost lease. Catches: a memory ladder that starts over at a
/// restart, and a kill counted against the cap on lost leases.
#[test]
fn a_kill_for_memory_writes_its_floor_and_fate_and_a_restart_keeps_the_booking() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 1, 1);
    let report = Report {
        operation: admitted.operation,
        lease: lease(1, 0),
        outcome: Outcome::Failed(Failure::OutOfMemory),
    };

    let taken = block_on(core.report(at(5), &worker("node-a"), report, |_| {}));

    assert_eq!(taken, Taken::Accepted);
    assert_eq!(setup.store.rows_of("floor/").len(), 1);
    assert_eq!(setup.store.rows_of("fate/").len(), 1);
    assert!(setup.store.rows_of("loss/").is_empty(), "not a lost lease");
    drop(core);
    let restarted = setup.core_with(&Config::new(1), Rig::default(), 10);
    let booked = restarted.with(|view, _| {
        view.sched()
            .request(admitted.operation)
            .map(|request| request.resources.memory_bytes)
    });
    assert!(
        booked.is_some_and(|bytes| bytes > GIB),
        "{booked:?}: the raised booking"
    );
}

/// Asserts that a give-up the store cannot take stays in line and is written, in order, before
/// the grant that follows it, and that a grant which meets no queue row for want of it is not
/// forgotten: the operation stays queued. Catches: a requeue lost to an outage, and an
/// operation dropped because its queue row had not been put back yet.
#[test]
fn a_give_up_the_store_missed_is_written_before_the_grant_that_follows_it() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    submit(&core, 1, 1);
    let node_b = up(&core, 2, "node-b", "d2");
    setup.store.fail_writes(Some("loss/"));
    beat(&core, 59_000, ("node-b", node_b), 1, &[]);

    block_on(core.tick(at(millis(LEASE_GRACE) + 1)));

    // The give-up is refused; the re-grant meets no queue row, is stale, and is undone.
    assert_eq!(core.dropped_writes(), 1);
    assert_eq!(
        core.queued_operations(),
        1,
        "kept: its give-up is not written"
    );
    assert_eq!(started(&core), [lease(1, 0)]);
    assert!(
        !calls(&core)
            .iter()
            .any(|call| matches!(call, Call::Followed(..)))
    );

    setup.store.fail_writes(None);
    block_on(core.tick(at(millis(LEASE_GRACE) + 2)));
    assert_eq!(started(&core), [lease(1, 0), lease(1, 2)]);
    assert_eq!(setup.store.rows_of("loss/").len(), 1);
}

/// Asserts that a request for work whose previous run has finished, with the finish not
/// written yet, is not queued: it asks again once the finish is. Catches: the same work run
/// twice because its rows were taken up again.
#[test]
fn a_request_for_work_that_is_finishing_asks_again() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 1, 1);
    setup.store.fail_writes(Some("done/"));
    let taken = finish(&core, 2, "node-a", admitted.operation, lease(1, 0));
    assert_eq!(taken, Taken::Held);

    let refused = submit_request(&core, 3, request(1));

    assert_eq!(refused, Err(NotQueued::Finishing));
    setup.store.fail_writes(None);
    block_on(core.tick(at(4)));
    assert_eq!(core.write_behind(), 0, "the finish is written");
    assert_eq!(setup.store.rows_of("done/").len(), 1);
    assert!(submit_request(&core, 5, request(1)).is_ok());
}

/// Asserts that a core that cannot read or write the rows a start needs does not start: each
/// failure is the error of the start, and no core is made. Catches: a start over a store it
/// could not read, which would number operations from zero again.
#[test]
fn a_start_over_a_store_that_fails_is_an_error() {
    for (what, reads, writes) in [
        ("the epoch", Some("meta/epoch"), None),
        ("the operations", Some("op/"), None),
        ("the operation ids", None, Some("meta/next_op")),
        ("the lease sequences", None, Some("meta/next_seq")),
    ] {
        let setup = Setup::new();
        setup.store.fail_reads(reads);
        setup.store.fail_writes(writes);
        let opened = Core::open(&Config::new(1), Rig::default(), setup.storage(), at(0));
        assert!(opened.is_err(), "{what}");
    }
}

/// Asserts that a record the store refuses for good (its lease row is not one the scheduler
/// wrote) is dropped with the rest of its record, and that the result's sender is not
/// acknowledged. Catches: a finish retried for ever against a row that cannot be read.
#[test]
fn a_result_the_store_refuses_for_good_is_dropped() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 1, 1);
    let rows = setup.store.rows_of("lease/");
    let key = rows[0].0.clone();
    setup
        .store
        .write(js_state::Txn::new().put(key, b"not a lease".to_vec(), js_state::Cond::Any));

    let taken = finish(&core, 2, "node-a", admitted.operation, lease(1, 0));

    assert_eq!(taken, Taken::Dropped);
    assert_eq!(core.dropped_writes(), 1);
    assert!(calls(&core).contains(&Call::ForgotParcel(lease(1, 0))));
    let waiter = admitted.waiter;
    assert!(calls(&core).contains(&Call::Followed(admitted.operation, vec![waiter])));
}

/// Asserts that a result whose rows another writer removed (the operation is neither held
/// under the lease nor queued, and has no result) is refused as stale, and its callers are
/// left to the store. Catches: a result written over rows that are not the operation's.
#[test]
fn a_result_for_rows_another_writer_removed_is_stale_and_followed() {
    let setup = Setup::new();
    let core = setup.core(1);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 1, 1);
    let gone = [
        setup.store.rows_of("lease/")[0].0.clone(),
        setup.store.rows_of("op/")[0].0.clone(),
    ];
    let removal = gone.into_iter().fold(js_state::Txn::new(), |txn, key| {
        txn.delete(key, js_state::Cond::Any)
    });
    setup.store.write(removal);

    let taken = finish(&core, 2, "node-a", admitted.operation, lease(1, 0));

    assert_eq!(taken, Taken::Dropped);
    assert_eq!(core.dropped_writes(), 1);
    let followed = Call::Followed(admitted.operation, vec![admitted.waiter]);
    assert!(calls(&core).contains(&followed));
}

/// Asserts that an effect the adapter cannot say the end of is dropped and its
/// callers are left to the store: an answer, then a refusal. Catches: a finish written with
/// no value, and callers left waiting for a core that will never answer them.
#[test]
fn a_finish_the_adapter_cannot_describe_is_dropped_and_its_callers_followed() {
    let setup = Setup::new();
    let config = Config::new(1).with_unservable_wait(Duration::from_secs(1));
    let core = setup.core_with(&config, Rig::default(), 0);
    up(&core, 0, "node-a", "d1");
    let admitted = submit(&core, 1, 1);
    core.with(|_, rig| rig.no_ending = true);

    let taken = finish(&core, 2, "node-a", admitted.operation, lease(1, 0));

    assert_eq!(taken, Taken::Dropped);
    let followed = Call::Followed(admitted.operation, vec![admitted.waiter]);
    assert!(calls(&core).contains(&followed));

    let mut huge = request(2);
    huge.resources = Resources::new(1_000_000, GIB);
    let refused = submit_request(&core, 3, huge).expect("queued");
    block_on(core.tick(at(5_000)));
    let followed = Call::Followed(refused.operation, vec![refused.waiter]);
    assert!(calls(&core).contains(&followed), "{:?}", calls(&core));
    assert_eq!(core.dropped_writes(), 2);
}
