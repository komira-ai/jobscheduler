use std::collections::BTreeSet;
use std::time::Duration;

use js_sched::fence::LEASE_GRACE;
use js_types::sched::{LeaseId, OperationId};

use super::{CoreHarness, Released};
use crate::Verdict;

const NODE_A: &str = "node-a";
const NODE_B: &str = "node-b";

/// The step of the injected clock in the scenarios that wait out a grace.
const STEP: Duration = Duration::from_secs(5);

/// One thing the loop around the scheduler must do, named in the suite's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scenario {
    /// A `Start` and an `Answer` leave the core only after the record they depend on is
    /// in the store: while the store refuses a write, nothing that depends on it is
    /// released, and every `Start` released has its lease in the store.
    StartWaitsForCommit,
    /// An operation is answered exactly once across a crash and restart: one finished
    /// before the crash is not run or answered again, and one queued at the crash runs
    /// and is answered once afterwards.
    AnsweredOnceAcrossRestart,
    /// A lease id `(term, seq)` is never handed out twice, across restarts, though the
    /// earlier leases have finished and left the store.
    LeaseIdsNeverRepeat,
    /// A heartbeat that arrives on a replaced stream is not fed, so it cannot requeue a
    /// lease the newest stream is about to list; and a `Start` goes to the stream that is
    /// the worker's newest when it leaves.
    StaleHeartbeatIsNotFed,
    /// A write the store refused is not lost: the held records are written again, in the
    /// order the scheduler asked, and each `Start` is released once.
    RefusedWriteIsRetriedInOrder,
    /// After a restart a lease whose worker has not registered is kept until the lease
    /// grace has passed and then given up, and one whose worker registers and lists it is
    /// kept.
    RecoveredLeaseWaitsForItsWorker,
}

impl Scenario {
    /// Every scenario, in the order [`super::run`] runs them.
    pub const ALL: [Scenario; 6] = [
        Scenario::StartWaitsForCommit,
        Scenario::AnsweredOnceAcrossRestart,
        Scenario::LeaseIdsNeverRepeat,
        Scenario::StaleHeartbeatIsNotFed,
        Scenario::RefusedWriteIsRetriedInOrder,
        Scenario::RecoveredLeaseWaitsForItsWorker,
    ];

    /// Runs this scenario on a fresh harness.
    pub(crate) fn run<H: CoreHarness>(&self, harness: &mut H) -> Verdict {
        let mut notes = Notes(Vec::new());
        match self {
            Self::StartWaitsForCommit => start_waits_for_commit(harness, &mut notes),
            Self::AnsweredOnceAcrossRestart => answered_once_across_restart(harness, &mut notes),
            Self::LeaseIdsNeverRepeat => lease_ids_never_repeat(harness, &mut notes),
            Self::StaleHeartbeatIsNotFed => stale_heartbeat_is_not_fed(harness, &mut notes),
            Self::RefusedWriteIsRetriedInOrder => refused_write_is_retried(harness, &mut notes),
            Self::RecoveredLeaseWaitsForItsWorker => {
                recovered_lease_waits(harness, &mut notes);
            }
        }
        notes.verdict()
    }
}

/// The observations that did not hold, in the order they were made. A scenario makes
/// every observation, so the verdict names everything that was wrong, not the first.
struct Notes(Vec<String>);

impl Notes {
    fn expect(&mut self, held: bool, what: &str) {
        if !held {
            self.0.push(what.to_string());
        }
    }

    fn verdict(self) -> Verdict {
        match self.0.is_empty() {
            true => Verdict::Passed,
            false => Verdict::Failed(self.0.join("; ")),
        }
    }
}

const SECOND: Duration = Duration::from_secs(1);

/// The operation and lease of the `index`th start, or a pair no core knows if there is none.
fn nth(released: &[Released], index: usize) -> (OperationId, LeaseId) {
    let none = (OperationId(u64::MAX), LeaseId::new(0, u64::MAX));
    released.get(index).map_or(none, |s| (s.operation, s.lease))
}

/// The starts released since `seen` of them were.
fn since<H: CoreHarness>(harness: &H, seen: usize) -> Vec<Released> {
    harness.observed().released.split_off(seen)
}

/// How many times `operation` was answered.
fn answers_of<H: CoreHarness>(harness: &H, operation: OperationId) -> usize {
    let answered = harness.observed().answered;
    answered.iter().filter(|a| a.operation == operation).count()
}

fn start_waits_for_commit<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
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
    let stored: BTreeSet<LeaseId> = seen.stored_leases.iter().copied().collect();
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
}

fn answered_once_across_restart<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    harness.worker_up(NODE_A);
    let first = harness.submit(1, 1);
    harness.advance(SECOND);
    let started = harness.observed().released;
    harness.report(first, nth(&started, 0).1);
    // Queued when the process dies: no tick has placed it.
    let queued = harness.submit(2, 2);

    harness.crash_and_restart();
    harness.worker_up(NODE_A);
    harness.advance(SECOND);
    let again = since(harness, started.len());
    for start in &again {
        harness.report(start.operation, start.lease);
    }
    harness.advance(SECOND);

    notes.expect(
        again.len() == 1 && again[0].operation == queued,
        "after the restart only the operation that was still queued is started",
    );
    notes.expect(
        answers_of(harness, first) == 1,
        "an operation finished before the crash is answered once, not again",
    );
    notes.expect(
        answers_of(harness, queued) == 1,
        "an operation queued at the crash is answered once",
    );
}

fn lease_ids_never_repeat<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    for job in 1..=3 {
        harness.worker_up(NODE_A);
        let operation = harness.submit(u64::from(job), job);
        harness.advance(SECOND);
        let released = harness.observed().released;
        harness.report(
            operation,
            nth(&released, released.len().saturating_sub(1)).1,
        );
        harness.crash_and_restart();
    }
    let leases: Vec<LeaseId> = harness
        .observed()
        .released
        .iter()
        .map(|s| s.lease)
        .collect();
    let distinct: BTreeSet<LeaseId> = leases.iter().copied().collect();
    notes.expect(
        distinct.len() == leases.len(),
        "no lease id is handed out twice across restarts",
    );
}

fn stale_heartbeat_is_not_fed<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    let old = harness.worker_up(NODE_A);
    harness.submit(1, 1);
    harness.advance(SECOND);
    let first = harness.observed().released;
    let new = harness.worker_up(NODE_A);

    // The old stream's last heartbeat arrives late, listing nothing.
    harness.heartbeat(NODE_A, old, &[]);
    harness.advance(SECOND);
    notes.expect(
        harness.observed().released.len() == 1,
        "a heartbeat from a replaced stream did not requeue the lease",
    );
    harness.heartbeat(NODE_A, new, &[nth(&first, 0).1]);
    harness.submit(2, 2);
    harness.advance(SECOND);

    let all = harness.observed().released;
    notes.expect(
        all.len() == 2 && all[0].stream == old && all[1].stream == new,
        "each Start went to the worker's newest stream when it left",
    );
}

fn refused_write_is_retried<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    harness.worker_up(NODE_A);
    let first = harness.submit(1, 1);
    let second = harness.submit(2, 2);
    // Refused twice: the retry at the next input is refused too, and the third succeeds.
    harness.refuse_writes(2);
    for _ in 0..4 {
        harness.advance(SECOND);
    }

    let seen = harness.observed();
    let started: Vec<OperationId> = seen.released.iter().map(|s| s.operation).collect();
    notes.expect(
        started == [first, second],
        "each operation is started once, in the order the scheduler placed them",
    );
    let leases: Vec<LeaseId> = seen.released.iter().map(|s| s.lease).collect();
    notes.expect(
        seen.stored_leases == leases,
        "the store took the lease records in the order the scheduler asked",
    );
}

fn recovered_lease_waits<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    harness.worker_up(NODE_A);
    let kept = harness.submit(1, 1);
    harness.advance(SECOND);
    harness.worker_up(NODE_B);
    let lost = harness.submit(2, 2);
    harness.advance(SECOND);
    let before = harness.observed().released;
    let kept_lease = nth(&before, 0).1;
    let lost_lease = nth(&before, 1).1;
    notes.expect(
        before.len() == 2 && before[0].operation == kept && before[1].operation == lost,
        "both operations were started before the restart",
    );
    notes.expect(
        before
            .iter()
            .any(|s| s.operation == lost && s.worker == NODE_B),
        "the second operation was placed on the second worker",
    );

    harness.crash_and_restart();
    // Only the first worker comes back and lists its lease.
    let stream = harness.worker_up(NODE_A);
    harness.heartbeat(NODE_A, stream, &[kept_lease]);
    let mut waited = Duration::ZERO;
    let mut wait = |harness: &mut H, total: Duration| {
        let until = waited + total;
        while waited < until {
            harness.advance(STEP);
            harness.heartbeat(NODE_A, stream, &[kept_lease]);
            waited += STEP;
        }
    };
    wait(harness, LEASE_GRACE - STEP);
    notes.expect(
        since(harness, before.len()).is_empty(),
        "a lease whose worker has not registered is kept until the grace has passed",
    );
    wait(harness, STEP * 3);
    let after = since(harness, before.len());
    notes.expect(
        after.len() == 1 && after[0].operation == lost && after[0].lease != lost_lease,
        "after the grace the unclaimed lease is given up and its operation runs again",
    );
    harness.report(kept, kept_lease);
    notes.expect(
        answers_of(harness, kept) == 1,
        "the lease its worker listed is kept: its result is accepted",
    );
}
