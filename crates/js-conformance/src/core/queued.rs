//! The scenario about work that is still queued when the process dies.

use super::CoreHarness;
use super::scenarios::{NODE_A, Notes, SECOND, answers_of, nth, operations};

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

    // Ids are not reissued: the next operation is numbered past the queued one.
    let next = harness.submit(3, 3);
    notes.expect(
        queued.0 < next.0,
        "an operation id is not reissued after a restart",
    );
}
