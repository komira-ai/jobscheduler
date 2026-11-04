//! Times and ends as stored: every time is the store's clock at the step that set it, nothing
//! else in the record moves, and every end the design names is stored with its time and its
//! allocation's release.

use js_state::conformance::Backend;
use js_state::{Store, StoreTime};

use super::{Rig, host, rig, share, whole_host_spec};
use crate::{
    AllocationRecord, AllocationStep, Booked, EndReason, JobEnd, JobId, JobRecord, JobResult,
    JobState, JobTiming, ResultReason, Retention, StartFailure,
};

/// The store's clock, moved on first so this step's time differs from the last one's.
fn next_reading<S: Store>(rig: &Rig<S>) -> StoreTime {
    rig.tick();
    rig.store.now()
}

fn stored_job<S: Store>(rig: &Rig<S>, id: &JobId) -> JobRecord {
    rig.jobs.get_job(id).expect("reads").expect("present")
}

fn stored_allocation<S: Store>(rig: &Rig<S>, id: &JobId) -> AllocationRecord {
    rig.jobs
        .get_allocation(id)
        .expect("reads")
        .expect("present")
}

/// Through a job's whole life, each record (returned and stored) is the one before with only
/// what the step sets changed, and each time it sets is the store's clock at that step: the
/// submit, the placement and the allocation's creation, the start of running, the first cancel
/// (a second leaves it), the end (with its result's timing and its retention end, the end
/// plus the window), the free, and a requeue and placement after it. The retention end is
/// unset until the end.
pub fn every_time_is_the_store_clock_at_its_step<B: Backend>(backend: &B) {
    let name = "every_time_is_the_store_clock_at_its_step";
    let rig = rig(backend);
    let at = next_reading(&rig);
    let id = rig.queue("t");
    let mut want = stored_job(&rig, &id);
    assert_eq!(
        (
            want.submitted_at,
            want.placed_at,
            want.started_at,
            want.ended_at,
            want.cancel_requested_at,
            want.retention_ends,
        ),
        (at, None, None, None, None, None),
        "{name}: the submit"
    );

    let at = next_reading(&rig);
    let (placed, allocation) = rig
        .jobs
        .place(&id, host("host-a"), share())
        .expect("placed");
    want = JobRecord {
        state: JobState::Placed,
        host: Some(host("host-a")),
        placed_at: Some(at),
        ..want
    };
    assert_eq!(placed, want, "{name}: the placement");
    assert_eq!(stored_job(&rig, &id), want, "{name}: the stored placement");
    assert_eq!(
        (allocation.created_at, allocation.freed_at),
        (at, None),
        "{name}: the allocation's creation"
    );

    next_reading(&rig);
    let starting = rig
        .jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .expect("starting");
    want = JobRecord {
        state: JobState::Starting,
        ..want
    };
    assert_eq!(starting, want, "{name}: a move that sets no time");

    let at = next_reading(&rig);
    let running = rig
        .jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .expect("running");
    want = JobRecord {
        state: JobState::Running,
        started_at: Some(at),
        ..want
    };
    assert_eq!(running, want, "{name}: the start of running");

    let at = next_reading(&rig);
    let intent = rig.jobs.request_cancel(&id).expect("an intent");
    want = JobRecord {
        cancel_requested: true,
        cancel_requested_at: Some(at),
        ..want
    };
    assert_eq!(intent, want, "{name}: the first cancel");
    next_reading(&rig);
    let again = rig.jobs.request_cancel(&id).expect("an intent again");
    assert_eq!(
        again, want,
        "{name}: a second cancel moved the first one's time"
    );

    let at = next_reading(&rig);
    let ended = rig.jobs.end_job(&id, super::cancelled()).expect("ends");
    want = JobRecord {
        state: JobState::Cancelled,
        ended_at: Some(at),
        result: Some(JobResult {
            exit_code: None,
            reason: ResultReason::Cancelled,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            truncated: false,
            timing: JobTiming {
                queued_at: want.submitted_at,
                placed_at: want.placed_at,
                started_at: want.started_at,
                ended_at: at,
            },
            usage: None,
        }),
        retention_ends: Some(at.saturating_add(Retention::DEFAULT.window())),
        ..want
    };
    assert_eq!(ended, want, "{name}: the end");
    assert_eq!(stored_job(&rig, &id), want, "{name}: the stored end");

    let at = next_reading(&rig);
    let freed = rig.jobs.free(&id, EndReason::Cancelled).expect("frees");
    let want_allocation = AllocationRecord {
        step: AllocationStep::Freed,
        freed_at: Some(at),
        end_reason: Some(EndReason::Cancelled),
        ..allocation
    };
    assert_eq!(freed, want_allocation, "{name}: the free");
    assert_eq!(
        stored_allocation(&rig, &id),
        want_allocation,
        "{name}: the stored free"
    );

    let whole = rig.queue_spec("t-whole", whole_host_spec());
    let at = next_reading(&rig);
    let (placed, first) = rig
        .jobs
        .place(&whole, host("host-b"), Booked::WholeHost)
        .expect("placed");
    let at_requeue = next_reading(&rig);
    let (queued, freed) = rig.jobs.requeue_failed_prepare(&whole).expect("requeued");
    assert_eq!(
        queued,
        JobRecord {
            state: JobState::Queued,
            host: None,
            placed_at: None,
            ..placed.clone()
        },
        "{name}: the requeue"
    );
    assert_eq!(
        (freed.created_at, freed.freed_at),
        (at, Some(at_requeue)),
        "{name}: the requeue's free"
    );
    let at_again = next_reading(&rig);
    let (again, second) = rig
        .jobs
        .place(&whole, host("host-b"), Booked::WholeHost)
        .expect("placed again");
    assert_eq!(
        again,
        JobRecord {
            placed_at: Some(at_again),
            ..placed
        },
        "{name}: the placement after the requeue"
    );
    assert_eq!(
        (second.created_at, second.freed_at, second.end_reason),
        (at_again, None, None),
        "{name}: the new allocation"
    );
    assert_ne!(
        first.allocation_id, second.allocation_id,
        "{name}: the requeue and the placement after it share an allocation id"
    );
}

/// Every end the design names, from every state it may follow, through the store: the
/// answer is what is stored, its end time is the store's clock, it has a result exactly when
/// the job ran to an exit, a limit or a cancel, its retention end is its end plus the
/// window, a booked allocation is releasing (and otherwise unchanged), and the ended job then
/// refuses every further move.
pub fn every_supported_end_is_stored_with_its_time_and_release<B: Backend>(backend: &B) {
    use JobState::{Cancelled, Lost, Placed, Queued, Running, Starting};
    use StartFailure::{AgentRefused, ExclusiveNotReady, NoCapacity, StartTimeout};
    let name = "every_supported_end_is_stored_with_its_time_and_release";
    let rig = rig(backend);
    let ends = [
        (Queued, JobEnd::FailedToStart(NoCapacity), false),
        (Queued, super::cancelled(), true),
        (Queued, JobEnd::Lost, false),
        (Placed, JobEnd::FailedToStart(StartTimeout), false),
        (Placed, JobEnd::FailedToStart(AgentRefused), false),
        (Placed, JobEnd::FailedToStart(ExclusiveNotReady), false),
        (Placed, super::cancelled(), true),
        (Placed, JobEnd::Lost, false),
        (Starting, JobEnd::FailedToStart(StartTimeout), false),
        (Starting, JobEnd::FailedToStart(AgentRefused), false),
        (Starting, super::cancelled(), true),
        (Starting, JobEnd::Lost, false),
        (Running, super::exited(0), true),
        (Running, super::exited(1), true),
        (
            Running,
            JobEnd::WallLimit {
                exit_code: Some(137),
                output: super::output(),
            },
            true,
        ),
        (
            Running,
            JobEnd::Memory {
                exit_code: None,
                output: super::output(),
            },
            true,
        ),
        (Running, super::cancelled(), true),
        (Running, JobEnd::Lost, false),
    ];
    for (n, (from, end, has_result)) in ends.into_iter().enumerate() {
        let to = end.state();
        let text = format!("end-{n:02}");
        let id = match from {
            Queued => rig.queue(&text),
            Placed => rig.placed(&text),
            Starting => {
                let id = rig.placed(&text);
                rig.jobs
                    .transition(&id, Placed, Starting)
                    .expect("starting");
                id
            }
            _ => rig.running(&text),
        };
        let before = stored_job(&rig, &id);
        let allocation = rig.jobs.get_allocation(&id).expect("reads");
        let at = next_reading(&rig);
        let ended = rig
            .jobs
            .end_job(&id, end)
            .expect("every_supported_end_is_stored_with_its_time_and_release: the end");
        assert_eq!(
            ended.result.as_ref().map(|result| result.timing.ended_at),
            has_result.then_some(at),
            "{name}: {from:?} to {to:?} has a result it should not, or lacks one"
        );
        assert_eq!(
            ended,
            JobRecord {
                state: to,
                ended_at: Some(at),
                result: ended.result.clone(),
                retention_ends: Some(at.saturating_add(Retention::DEFAULT.window())),
                ..before
            },
            "{name}: {from:?} to {to:?} is not stored with its time, result and retention alone"
        );
        assert_eq!(stored_job(&rig, &id), ended, "{name}: {from:?} to {to:?}");
        assert_eq!(
            rig.jobs.get_allocation(&id).expect("reads"),
            allocation.map(|allocation| AllocationRecord {
                step: AllocationStep::Releasing,
                ..allocation
            }),
            "{name}: {from:?} to {to:?} did not release its allocation alone"
        );
        for next in [Cancelled, Lost, Running, Queued] {
            assert_eq!(
                rig.jobs.transition(&id, to, next).map(drop),
                Err(crate::JobStoreError::IllegalTransition {
                    id: id.clone(),
                    from: to,
                    to: next,
                }),
                "{name}: {to:?} moved again to {next:?}"
            );
        }
    }
}
