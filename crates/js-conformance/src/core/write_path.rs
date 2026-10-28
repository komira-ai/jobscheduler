//! The scenarios about the store write that every effect waits on: the order of release,
//! the retry of a refused write, and the outage rules.

use js_sched::UNSERVABLE_WAIT;
use js_sched::fence::LEASE_GRACE;
use js_types::sched::LeaseId;

use super::scenarios::{
    NODE_A, NODE_B, Notes, SECOND, STEP, answers_of, nth, operations, since, soak,
};
use super::{CoreHarness, UNPLACEABLE_JOB};

pub(super) fn start_waits_for_commit<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    harness.worker_up(NODE_A);
    harness.refuse_writes(1);
    harness.submit(1, 1);
    harness.advance(SECOND);
    notes.expect(
        harness.observed().released.is_empty(),
        "no Start is released while the store refuses its lease record",
    );
    harness.advance(SECOND);
    let seen = harness.observed();
    let stored: std::collections::BTreeSet<LeaseId> = seen.stored_leases.iter().copied().collect();
    notes.expect(
        seen.released
            .iter()
            .all(|start| stored.contains(&start.lease)),
        "every Start released has its lease in the store",
    );

    // With nothing started the report names ids the core does not know, and is ignored.
    let (operation, lease) = nth(&seen.released, 0);
    harness.refuse_writes(1);
    harness.report(operation, lease);
    notes.expect(
        harness.observed().answered.is_empty(),
        "no answer is released while the store refuses its result record",
    );
    harness.advance(SECOND);
    harness.advance(SECOND);

    // A later lease (not the first the core numbers) waits for its record too.
    let started = harness.observed().released.len();
    harness.refuse_writes(1);
    harness.submit(2, 2);
    harness.advance(SECOND);
    notes.expect(
        since(harness, started).is_empty(),
        "no Start of a later lease is released while the store refuses its record",
    );
    harness.advance(SECOND);
    harness.advance(SECOND);

    // A refusal is answered only once it is written, too.
    let doomed = harness.submit(3, UNPLACEABLE_JOB);
    harness.advance(SECOND);
    harness.refuse_writes(1);
    harness.advance(UNSERVABLE_WAIT + SECOND);
    notes.expect(
        answers_of(harness, doomed) == 0,
        "no refusal is answered while the store refuses its record",
    );
}

pub(super) fn refused_write_is_retried<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    harness.worker_up(NODE_A);
    // Refused once up to six times: a record held behind a refusal keeps its
    // place and is not given up however often the store says no.
    for depth in 1..=6u8 {
        let started = harness.observed().released.len();
        // Before the submits: a core that places work as it is submitted writes the grants
        // then, and must find the store refusing.
        harness.refuse_writes(u32::from(depth));
        let first = harness.submit(u64::from(2 * depth - 1), 2 * depth - 1);
        let second = harness.submit(u64::from(2 * depth), 2 * depth);
        for _ in 0..depth + 2 {
            harness.advance(SECOND);
        }

        let seen = harness.observed();
        let new = since(harness, started);
        notes.expect(
            operations(&new) == [first, second],
            "each operation is started once, in the order it was submitted and placed",
        );
        let leases: Vec<LeaseId> = new.iter().map(|s| s.lease).collect();
        notes.expect(
            leases.windows(2).all(|pair| pair[0] < pair[1]),
            "leases are numbered in the order they were placed",
        );
        notes.expect(
            seen.stored_leases == leases,
            "the store took the lease records in the order the scheduler asked",
        );
        for start in new {
            harness.report(start.operation, start.lease);
        }
        harness.advance(SECOND);
    }
}

pub(super) fn store_outage_is_waited_out<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    harness.worker_up(NODE_A);
    let silent = harness.submit(1, 1);
    harness.advance(SECOND);
    let stream = harness.worker_up(NODE_B);
    let busy = harness.submit(2, 2);
    harness.advance(SECOND);

    // The store goes down with a grant held; the first worker is never heard from again.
    harness.refuse_writes(u32::MAX);
    harness.submit(3, 3);
    harness.advance(SECOND);
    soak(harness, NODE_B, stream, LEASE_GRACE + STEP * 2);

    // The store is back when the next write lands: the second worker's job ends.
    harness.refuse_writes(0);
    let released = harness.observed().released;
    let started = released.len();
    harness.report(busy, nth(&released, 1).1);
    harness.advance(SECOND);
    notes.expect(
        !operations(&since(harness, started)).contains(&silent),
        "no lease is given up for silence while the store is down",
    );
    soak(harness, NODE_B, stream, LEASE_GRACE - STEP);
    notes.expect(
        !operations(&since(harness, started)).contains(&silent),
        "no lease is given up for silence in the grace after the store returns",
    );
    soak(harness, NODE_B, stream, STEP * 3);
    let again = operations(&since(harness, started));
    notes.expect(
        again.iter().filter(|op| **op == silent).count() == 1,
        "once the grace has passed a silent worker's lease is given up and runs again",
    );
}
