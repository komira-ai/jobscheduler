//! The scenario about work that is still queued when the process dies.

use super::CoreHarness;
use super::scenarios::{NODE_A, Notes, SECOND, answers_of, nth, operations, since};
use js_types::sched::{OperationId, WaiterId};

pub(super) fn queued_at_crash_runs_once<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    // No worker has ever registered, so the operation cannot be placed and is queued when
    // the process dies.
    let queued = harness.submit(1, 1);
    harness.advance(SECOND);
    notes.expect(
        harness.observed().released.is_empty(),
        "an operation with no worker to run on is not started",
    );

    harness.crash_and_restart();
    harness.worker_up(NODE_A);
    harness.advance(SECOND);
    let released = harness.observed().released;
    notes.expect(
        operations(&released) == [queued],
        "an operation queued at the crash is started once after the restart",
    );
    harness.advance(SECOND);
    notes.expect(
        harness.observed().released.len() == released.len(),
        "the recovered operation is not started a second time",
    );
    harness.report(queued, nth(&released, 0).1);
    harness.advance(SECOND);
    notes.expect(
        answers_of(harness, queued) == 1,
        "an operation queued at the crash is answered once",
    );
    notes.expect(
        waiters_of(harness, queued) == [WaiterId(1)],
        "an operation queued at the crash is answered for the caller that submitted it",
    );

    // Ids are not reissued: the next operation is numbered past the queued one.
    let next = harness.submit(3, 3);
    notes.expect(
        queued.0 < next.0,
        "an operation id is not reissued after a restart",
    );
}

/// The callers `operation` was answered for, in the order the answers left.
fn waiters_of<H: CoreHarness>(harness: &H, operation: OperationId) -> Vec<WaiterId> {
    let answered = harness.observed().answered;
    let mine = answered.iter().filter(|a| a.operation == operation);
    mine.flat_map(|a| a.waiters.clone()).collect()
}

pub(super) fn refused_grant_at_crash_runs_once<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    // A worker is up, but the store refuses the grant, so the operation is queued when
    // the process dies.
    harness.worker_up(NODE_A);
    harness.refuse_writes(u32::MAX);
    let refused = harness.submit(1, 1);
    harness.advance(SECOND);
    harness.advance(SECOND);

    harness.crash_and_restart();
    // Only what is started from here on counts: the restart is what is under test.
    let before = harness.observed().released.len();
    harness.worker_up(NODE_A);
    harness.advance(SECOND);
    let started = since(harness, before);
    notes.expect(
        operations(&started) == [refused],
        "an operation whose grant was refused at the crash is started once after the restart",
    );
    harness.advance(SECOND);
    notes.expect(
        since(harness, before).len() == started.len(),
        "the recovered operation is not started a second time",
    );
    harness.report(refused, nth(&started, 0).1);
    harness.advance(SECOND);
    notes.expect(
        answers_of(harness, refused) == 1,
        "an operation whose grant was refused at the crash is answered once",
    );
    notes.expect(
        waiters_of(harness, refused) == [WaiterId(1)],
        "the recovered operation is answered for the caller that submitted it",
    );
}
