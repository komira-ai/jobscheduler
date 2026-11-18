//! A result that names a lost tree (`tree-lost`) on the job tables alone: the job goes back to
//! queued while it has requeues left and no cancel was asked for, and ends as an input
//! failure otherwise.

use js_agent_wire::{
    Ack, HeartbeatReply, HeldJob, HeldState, JobResult, JobResultAck, NetworkCeiling, Outcome,
    refusal,
};
use js_jobs::{
    AllocationRecord, AllocationStep, EndReason, JobId, JobRecord, JobState, JobStoreError,
    ResultReason, TREE_LOST_REQUEUES,
};
use js_state::{Store, StoreError};
use js_types::{HostId, JobEventKind, QueueCode};

use super::{BeatContext, JobReply, commit_result, serve};
use crate::agent_api::job_rig::{BOOKED, placed, result, spec, starts, tables};
use crate::job_store::ServerJobs;
use crate::metrics::NoMetrics;

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

/// One heartbeat of `host-a` listing `listed`.
fn beat(jobs: &ServerJobs, listed: &[HeldJob]) -> Result<JobReply, JobStoreError> {
    let ceiling = NetworkCeiling::default();
    let context = BeatContext {
        host: &host(),
        ceiling: &ceiling,
        host_jobs: false,
        shape: None,
        logs: None,
        metrics: &NoMetrics,
        layered_rootfs: false,
        mount_providers: &[],
    };
    serve(jobs, &context, listed, &[], 8)
}

/// Records the job `id`, places it on `host-a` and runs it: its records.
fn running(jobs: &ServerJobs, id: &str) -> (JobRecord, AllocationRecord) {
    let (job, allocation) = placed(jobs, id, "host-a", spec(), BOOKED);
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    let job = jobs
        .transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    (job, allocation)
}

/// The listing of a job that ended with the input failure `code`.
fn failed_with(allocation: &AllocationRecord, code: &str) -> HeldJob {
    let mut ended: JobResult = result(
        "job-a",
        &allocation.allocation_id.to_string(),
        Outcome::Failed,
        Some(1),
    );
    ended.input_failure = Some(code.to_owned());
    HeldJob {
        job_id: "job-a".to_owned(),
        state: HeldState::ExitedUnacked,
        result: Some(ended),
    }
}

fn ack(reply: &JobReply) -> Vec<JobResultAck> {
    reply.job_acks.clone()
}

fn accepted() -> Vec<JobResultAck> {
    vec![JobResultAck {
        job_id: "job-a".to_owned(),
        ack: Ack::Accepted,
    }]
}

fn job_a() -> JobId {
    JobId::parse("job-a").unwrap()
}

/// A tree-lost result sends the running job back to queued: no host, one lost tree, a requeue
/// event naming the host, its allocation releasing, the result accepted. The same result sent
/// again is a duplicate and writes nothing. Once the host stops listing the job its allocation
/// is freed as exited, the job is placed again under a new allocation id, and the host is sent
/// its start. Catches a lost tree that ends the job, a resent result that requeues it twice, an
/// allocation never freed, and a second run started under the first run's allocation.
#[test]
fn a_tree_lost_result_requeues_the_job() {
    let (store, jobs) = tables();
    let (_, first) = running(&jobs, "job-a");
    let lost = failed_with(&first, refusal::TREE_LOST);

    let reply = beat(&jobs, std::slice::from_ref(&lost)).unwrap();
    assert_eq!(ack(&reply), accepted());
    assert!(reply.commands.is_empty() & reply.clean);
    let job = jobs.get_job(&job_a()).unwrap().unwrap();
    assert_eq!(
        (
            job.state,
            job.host.clone(),
            job.lost_trees,
            job.failed_prepares
        ),
        (JobState::Queued, None, 1, 0)
    );
    assert_eq!(job.queue.code(), QueueCode::WaitingForCapacity);
    let event = job.queue.events().last().unwrap();
    assert_eq!(
        (event.kind, event.host.clone()),
        (JobEventKind::Requeued, Some(host()))
    );
    let releasing = jobs.get_allocation(&job.id).unwrap().unwrap();
    assert_eq!(releasing.step, AllocationStep::Releasing);

    let commits = store.commit_stats().txns;
    let again = beat(&jobs, std::slice::from_ref(&lost)).unwrap();
    assert_eq!(
        ack(&again),
        [JobResultAck {
            job_id: "job-a".to_owned(),
            ack: Ack::Duplicate
        }]
    );
    assert_eq!(store.commit_stats().txns, commits, "a resent result wrote");
    assert_eq!(jobs.get_job(&job.id).unwrap(), Some(job.clone()));

    beat(&jobs, &[]).unwrap();
    let freed = jobs.get_allocation(&job.id).unwrap().unwrap();
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::Exited))
    );

    let (placed_again, second) = jobs.place(&job.id, host(), BOOKED).unwrap();
    assert_eq!(placed_again.lost_trees, 1);
    assert_ne!(second.allocation_id, first.allocation_id);
    let reply = beat(&jobs, &[]).unwrap();
    let sent = starts(&HeartbeatReply {
        commands: reply.commands,
        ..HeartbeatReply::default()
    });
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].allocation_id, second.allocation_id.to_string());
}

/// A job is queued again for a lost tree at most [`TREE_LOST_REQUEUES`] times: the next
/// tree-lost result ends it failed, with the reason input-failed and the code `tree-lost`, and
/// its allocation releasing. Catches a job that is requeued without end while its tree is
/// lost, and a count that is not kept across the runs.
#[test]
fn the_third_tree_lost_result_ends_the_job_as_an_input_failure() {
    let (_store, jobs) = tables();
    let (_, mut allocation) = running(&jobs, "job-a");
    for round in 1..=TREE_LOST_REQUEUES {
        let reply = beat(&jobs, &[failed_with(&allocation, refusal::TREE_LOST)]).unwrap();
        assert_eq!(ack(&reply), accepted(), "round {round}");
        beat(&jobs, &[]).unwrap();
        let (_, next) = jobs.place(&job_a(), host(), BOOKED).unwrap();
        jobs.transition(&job_a(), JobState::Placed, JobState::Starting)
            .unwrap();
        jobs.transition(&job_a(), JobState::Starting, JobState::Running)
            .unwrap();
        allocation = next;
    }
    let reply = beat(&jobs, &[failed_with(&allocation, refusal::TREE_LOST)]).unwrap();
    assert_eq!(ack(&reply), accepted());
    let job = jobs.get_job(&job_a()).unwrap().unwrap();
    assert_eq!(
        (job.state, job.lost_trees),
        (JobState::Failed, TREE_LOST_REQUEUES)
    );
    let ended = job.result.unwrap();
    assert_eq!(
        (ended.reason, ended.input_failure.as_deref()),
        (ResultReason::InputFailed, Some("tree-lost"))
    );
    let releasing = jobs.get_allocation(&job_a()).unwrap().unwrap();
    assert_eq!(releasing.step, AllocationStep::Releasing);
}

/// A tree-lost result for a job a cancel was asked for ends it as it ends without the code (an
/// input failure), and an input failure of another code ends a job the same way; neither is
/// queued again. Catches a cancelled job that runs again, and a requeue for any input failure.
#[test]
fn a_cancelled_job_or_another_input_failure_ends_the_job() {
    let (_store, jobs) = tables();
    let (job, allocation) = running(&jobs, "job-a");
    jobs.request_cancel(&job.id).unwrap();
    let reply = beat(&jobs, &[failed_with(&allocation, refusal::TREE_LOST)]).unwrap();
    assert_eq!(ack(&reply), accepted());
    let ended = jobs.get_job(&job.id).unwrap().unwrap();
    assert_eq!((ended.state, ended.lost_trees), (JobState::Failed, 0));

    let (other, allocation) = running(&jobs, "job-a-2");
    let mut unreadable = failed_with(&allocation, refusal::INPUT_UNREADABLE);
    unreadable.job_id = other.id.to_string();
    unreadable.result.as_mut().unwrap().job_id = other.id.to_string();
    let reply = beat(&jobs, &[unreadable]).unwrap();
    assert_eq!(reply.job_acks[0].ack, Ack::Accepted);
    let ended = jobs.get_job(&other.id).unwrap().unwrap();
    assert_eq!((ended.state, ended.lost_trees), (JobState::Failed, 0));
}

/// A cancel asked for between the read of a job and the requeue stops the requeue: the result
/// is answered stale, the job stays running with its cancel, and its allocation booked, so the
/// host's resent result ends it. Catches a requeue over a cancel that overwrites the intent.
#[test]
fn a_cancel_that_arrives_with_the_lost_tree_is_answered_stale() {
    let (_store, jobs) = tables();
    let (read, allocation) = running(&jobs, "job-a");
    jobs.request_cancel(&read.id).unwrap();
    let ceiling = NetworkCeiling::default();
    let context = BeatContext {
        host: &host(),
        ceiling: &ceiling,
        host_jobs: false,
        shape: None,
        logs: None,
        metrics: &NoMetrics,
        layered_rootfs: false,
        mount_providers: &[],
    };
    let lost = failed_with(&allocation, refusal::TREE_LOST).result.unwrap();
    let answer = commit_result(&jobs, &context, read.clone(), &lost).unwrap();
    assert_eq!(answer, Some(Ack::Stale));
    let held = jobs.get_job(&read.id).unwrap().unwrap();
    assert_eq!(
        (held.state, held.cancel_requested),
        (JobState::Running, true)
    );
    let booked = jobs.get_allocation(&read.id).unwrap().unwrap();
    assert_eq!(booked.step, AllocationStep::Booked);
}

/// A requeue whose commit fails is the error the heartbeat answers 500 for, and the job is
/// left running with its allocation booked, so the host reports the result again. Catches a
/// requeue acknowledged though nothing was written.
#[test]
fn a_failed_requeue_commit_is_an_error_and_leaves_the_job_running() {
    let (store, jobs) = tables();
    let (job, allocation) = running(&jobs, "job-a");
    store.fail_next();
    let answer = beat(&jobs, &[failed_with(&allocation, refusal::TREE_LOST)]);
    assert_eq!(
        answer.map(drop),
        Err(JobStoreError::Store(StoreError::Failed(
            js_jobs::interfere::INJECTED.to_owned()
        )))
    );
    assert_eq!(
        jobs.get_job(&job.id).unwrap().unwrap().state,
        JobState::Running
    );
}
