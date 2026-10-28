use std::collections::BTreeSet;
use std::time::Duration;

use js_sched::fence::LEASE_GRACE;
use js_types::sched::{LeaseId, OperationId, WaiterId};

use super::queued;
use super::write_path;
use super::{CoreHarness, Released, Stream};
use crate::Verdict;

pub(super) const NODE_A: &str = "node-a";
pub(super) const NODE_B: &str = "node-b";

/// The step of the injected clock in the scenarios that wait out a grace.
pub(super) const STEP: Duration = Duration::from_secs(5);

/// One thing the loop around the scheduler must do, named in the suite's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scenario {
    /// A `Start`, an `Answer` and a refusal leave the core only after the record they
    /// depend on is in the store, for the first lease and for later ones: while the store
    /// refuses a write, nothing that depends on it is released, and every `Start` released
    /// has its lease in the store.
    StartWaitsForCommit,
    /// An operation is answered exactly once across two crashes and restarts: one
    /// finished before a crash is not run or answered again, one queued at a crash runs
    /// and is answered once afterwards, and work submitted twice while in flight is one
    /// operation.
    AnsweredOnceAcrossRestart,
    /// A lease id `(term, seq)` is never handed out twice, across restarts, though the
    /// earlier leases have finished and left the store, and though only the lower of two
    /// leases in flight is left in it.
    LeaseIdsNeverRepeat,
    /// A heartbeat that arrives on a replaced stream is not fed, so it cannot requeue a
    /// lease the newest stream is about to list; and a `Start` goes to the stream that is
    /// the worker's newest when it leaves.
    StaleHeartbeatIsNotFed,
    /// A write the store refused is not lost, refused up to six times in a row: the held
    /// records are written again, in the order the scheduler asked, and each `Start` is
    /// released once.
    RefusedWriteIsRetriedInOrder,
    /// After a restart a lease whose worker has not registered is kept until the lease
    /// grace has passed and then given up, and one whose worker registers and lists it is
    /// kept.
    RecoveredLeaseWaitsForItsWorker,
    /// While the store is down nothing is given up for silence, and for the grace after it
    /// returns a silent worker's lease is kept; once the grace has passed it is given up.
    StoreOutageIsWaitedOut,
    /// An operation still queued when the process dies, for want of any worker to place it
    /// on, is started once after the restart, answered once, and
    /// its id is not handed out again.
    QueuedAtCrashRunsOnce,
}

impl Scenario {
    /// Every scenario, in the order [`super::run`] runs them.
    pub const ALL: [Scenario; 8] = [
        Scenario::StartWaitsForCommit,
        Scenario::AnsweredOnceAcrossRestart,
        Scenario::LeaseIdsNeverRepeat,
        Scenario::StaleHeartbeatIsNotFed,
        Scenario::RefusedWriteIsRetriedInOrder,
        Scenario::RecoveredLeaseWaitsForItsWorker,
        Scenario::StoreOutageIsWaitedOut,
        Scenario::QueuedAtCrashRunsOnce,
    ];

    /// Runs this scenario on a fresh harness.
    pub(crate) fn run<H: CoreHarness>(&self, harness: &mut H) -> Verdict {
        let mut notes = Notes(Vec::new());
        match self {
            Self::StartWaitsForCommit => write_path::start_waits_for_commit(harness, &mut notes),
            Self::AnsweredOnceAcrossRestart => answered_once_across_restart(harness, &mut notes),
            Self::LeaseIdsNeverRepeat => lease_ids_never_repeat(harness, &mut notes),
            Self::StaleHeartbeatIsNotFed => stale_heartbeat_is_not_fed(harness, &mut notes),
            Self::RefusedWriteIsRetriedInOrder => {
                write_path::refused_write_is_retried(harness, &mut notes);
            }
            Self::RecoveredLeaseWaitsForItsWorker => {
                recovered_lease_waits(harness, &mut notes);
            }
            Self::StoreOutageIsWaitedOut => {
                write_path::store_outage_is_waited_out(harness, &mut notes);
            }
            Self::QueuedAtCrashRunsOnce => queued::queued_at_crash_runs_once(harness, &mut notes),
        }
        notes.verdict()
    }
}

/// The observations that did not hold, in the order they were made. A scenario makes
/// every observation, so the verdict names everything that was wrong, not the first.
pub(super) struct Notes(Vec<String>);

impl Notes {
    pub(super) fn expect(&mut self, held: bool, what: &str) {
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

pub(super) const SECOND: Duration = Duration::from_secs(1);

/// The operation and lease of the `index`th start, or a pair no core knows if there is none.
pub(super) fn nth(released: &[Released], index: usize) -> (OperationId, LeaseId) {
    let none = (OperationId(u64::MAX), LeaseId::new(0, u64::MAX));
    released.get(index).map_or(none, |s| (s.operation, s.lease))
}

/// The operations of `released`, in order.
pub(super) fn operations(released: &[Released]) -> Vec<OperationId> {
    released.iter().map(|s| s.operation).collect()
}

/// The operation and worker of each start in `released`, in order.
fn placements(released: &[Released]) -> Vec<(OperationId, &str)> {
    released
        .iter()
        .map(|s| (s.operation, s.worker.as_str()))
        .collect()
}

/// The starts released since `seen` of them were.
pub(super) fn since<H: CoreHarness>(harness: &H, seen: usize) -> Vec<Released> {
    harness.observed().released.split_off(seen)
}

/// How many times `operation` was answered.
pub(super) fn answers_of<H: CoreHarness>(harness: &H, operation: OperationId) -> usize {
    let answered = harness.observed().answered;
    answered.iter().filter(|a| a.operation == operation).count()
}

/// The leases the core has released to `worker`, in order.
pub(super) fn leases_on<H: CoreHarness>(harness: &H, worker: &str) -> Vec<LeaseId> {
    let released = harness.observed().released;
    let there = released.iter().filter(|s| s.worker == worker);
    there.map(|s| s.lease).collect()
}

/// Advances the clock by `total` in steps, `worker` heartbeating on `stream` at each step
/// with the leases it has been sent.
pub(super) fn soak<H: CoreHarness>(harness: &mut H, worker: &str, stream: Stream, total: Duration) {
    let mut spent = Duration::ZERO;
    while spent < total {
        harness.advance(STEP);
        let running = leases_on(harness, worker);
        harness.heartbeat(worker, stream, &running);
        spent += STEP;
    }
}

/// Reports every start released since `seen` of them were.
fn report_since<H: CoreHarness>(harness: &mut H, seen: usize) {
    for start in since(harness, seen) {
        harness.report(start.operation, start.lease);
    }
}

fn answered_once_across_restart<H: CoreHarness>(harness: &mut H, notes: &mut Notes) {
    harness.worker_up(NODE_A);
    let first = harness.submit(1, 1);
    harness.advance(SECOND);
    let started = harness.observed().released;
    harness.report(first, nth(&started, 0).1);
    // Unanswered when the process dies, queued or already started (a core that places work
    // at a tick has not placed it; one that places it as it is submitted has). Its twin
    // joins it, and the next distinct job is numbered right after it: a join burns no id.
    let second = harness.submit(2, 2);
    let joined = harness.submit(3, 2);
    notes.expect(
        joined == second,
        "work already in flight is joined, not added",
    );
    let fourth = harness.submit(5, 5);
    notes.expect(
        fourth.0 == second.0 + 1,
        "a join does not use up an operation id: the next operation is numbered next",
    );

    harness.crash_and_restart();
    harness.worker_up(NODE_A);
    harness.advance(SECOND);
    report_since(harness, started.len());
    // Counted before the submit: a core that places work as it is submitted has started the
    // third operation by the time the crash comes, and the report after it is still due.
    let second_boot = harness.observed().released.len();
    let third = harness.submit(4, 3);

    harness.crash_and_restart();
    harness.worker_up(NODE_A);
    harness.advance(SECOND);
    report_since(harness, second_boot);
    harness.advance(SECOND);

    notes.expect(
        operations(&harness.observed().released) == [first, second, fourth, third],
        "each operation is started once across both restarts, a joined one once",
    );
    let answered = harness.observed().answered;
    let waiters: Vec<WaiterId> = answered
        .iter()
        .filter(|a| a.operation == second)
        .flat_map(|a| a.waiters.clone())
        .collect();
    notes.expect(
        waiters == [WaiterId(2)],
        "a joined operation is recovered for its first caller, not the joiner",
    );
    // Past, not next, across a restart: a core that reserves its ids in blocks begins a new
    // block at one.
    let ids = [first, second, fourth, third];
    notes.expect(
        ids.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "operation ids grow across restarts: no id is handed out twice",
    );
    for operation in [first, second, fourth, third] {
        notes.expect(
            answers_of(harness, operation) == 1,
            "each operation is answered once across both restarts",
        );
    }
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
    // Two leases in flight, the higher one finishes, the process dies: the lower one is
    // all the store holds of them, and the next lease must still not take the higher id.
    harness.worker_up(NODE_A);
    harness.submit(4, 4);
    harness.submit(5, 5);
    harness.advance(SECOND);
    let released = harness.observed().released;
    let (operation, lease) = nth(&released, released.len().saturating_sub(1));
    harness.report(operation, lease);
    harness.crash_and_restart();
    harness.worker_up(NODE_A);
    harness.submit(6, 6);
    harness.advance(SECOND);

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
    let lease = nth(&first, 0).1;
    harness.heartbeat(NODE_A, new, &[lease]);

    // A second replacement: heartbeats of both earlier streams, one and two generations
    // old, arrive late and list nothing.
    let newest = harness.worker_up(NODE_A);
    harness.heartbeat(NODE_A, old, &[]);
    harness.heartbeat(NODE_A, new, &[]);
    harness.advance(SECOND);
    notes.expect(
        harness.observed().released.len() == 1,
        "heartbeats from streams replaced twice over did not requeue the lease",
    );
    harness.heartbeat(NODE_A, newest, &[lease]);
    harness.submit(2, 2);
    harness.advance(SECOND);

    let all = harness.observed().released;
    notes.expect(
        all.iter().map(|s| s.stream).collect::<Vec<_>>() == [old, newest],
        "each Start went to the worker's newest stream when it left",
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
        placements(&before) == [(kept, NODE_A), (lost, NODE_B)],
        "the operations were started before the restart, one on each worker",
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
        operations(&after) == [lost],
        "after the grace the unclaimed lease is given up and its operation runs again",
    );
    notes.expect(
        after.iter().all(|s| s.lease != lost_lease),
        "the operation runs again under a new lease",
    );
    harness.report(kept, kept_lease);
    notes.expect(
        answers_of(harness, kept) == 1,
        "the lease its worker listed is kept: its result is accepted",
    );
}
