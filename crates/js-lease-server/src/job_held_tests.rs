//! A start a held host refused, served through the per-job half of a heartbeat on the job
//! tables alone: the job goes back to the queue with no failure counted, the bound on such
//! requeues ends it naming every host, and a cancelled or running job, or another code, ends
//! as before.

use std::sync::Mutex;
use std::time::Duration;

use js_agent_wire::{Ack, Command, HeldJob, HeldState, JobResult, NetworkCeiling, Outcome};
use js_jobs::{
    AllocationRecord, AllocationStep, Booked, EndReason, Exclusion, JobId, JobRecord, JobSpec,
    JobState, JobStoreError, Needs, StartFailure,
};
use js_state::{Store, StoreError};
use js_types::{HostId, JobEventKind, QueueCode};

use super::{HELD_BAR, MAX_HELD_REQUEUES, named};
use crate::agent_api::job_rig::{BOOKED, placed, result, spec, tables};
use crate::job_store::ServerJobs;
use crate::metrics::MetricsSink;
use crate::per_job::{BeatContext, JobReply, serve};

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A sink that records the start failures it is told of, and counts nothing else.
#[derive(Debug, Default)]
struct Failures(Mutex<Vec<StartFailure>>);

impl MetricsSink for Failures {
    fn job_placed(&self, _: Duration) {}
    fn start_failed(&self, failure: StartFailure) {
        self.0.lock().unwrap().push(failure);
    }
    fn job_lost(&self) {}
    fn store_write_failed(&self) {}
    fn job_pass_took(&self, _: Duration) {}
    fn job_tables(&self, _: js_sched_placer::engine::TablesFrom) {}
    fn acquire_answered(&self, _: &str, _: &js_elastic::Acquired) {}
}

impl Failures {
    fn told(&self) -> Vec<StartFailure> {
        self.0.lock().unwrap().clone()
    }
}

/// The result an agent lists for a start of `allocation` it refused with `code`.
fn refusal(allocation: &AllocationRecord, code: &str) -> JobResult {
    let mut refused = result(
        allocation.job.as_str(),
        &allocation.allocation_id.to_string(),
        Outcome::FailedToStart,
        None,
    );
    refused.detail = Some(code.to_owned());
    refused.log_tail = None;
    refused
}

/// Serves one heartbeat of the host of `allocation` listing `listed` as its result for the
/// allocation's job, reporting to `metrics`.
fn beat(
    jobs: &ServerJobs,
    allocation: &AllocationRecord,
    listed: JobResult,
    metrics: &Failures,
) -> Result<JobReply, JobStoreError> {
    let ceiling = NetworkCeiling::default();
    let context = BeatContext {
        host: &allocation.host,
        ceiling: &ceiling,
        host_jobs: false,
        shape: None,
        logs: None,
        polls_dispatch: false,
        metrics,
        layered_rootfs: false,
        mount_providers: &[],
        xcodes: &[],
    };
    let listing = [HeldJob {
        job_id: allocation.job.to_string(),
        state: HeldState::ExitedUnacked,
        result: Some(listed),
    }];
    serve(jobs, &context, &listing, &[], 8, None)
}

/// The verdict on the one result a heartbeat of the host of `allocation` lists, refused with
/// `code`.
fn refused(
    jobs: &ServerJobs,
    allocation: &AllocationRecord,
    code: &str,
    metrics: &Failures,
) -> Ack {
    let reply = beat(jobs, allocation, refusal(allocation, code), metrics).unwrap();
    assert_eq!(reply.job_acks.len(), 1, "one verdict: {reply:?}");
    reply.job_acks[0].ack
}

fn job(jobs: &ServerJobs, id: &JobId) -> JobRecord {
    jobs.get_job(id).unwrap().unwrap()
}

fn allocation(jobs: &ServerJobs, id: &JobId) -> AllocationRecord {
    jobs.get_allocation(id).unwrap().unwrap()
}

/// A job that books the whole host, as [`spec`] otherwise.
fn whole_spec() -> JobSpec {
    JobSpec {
        needs: Needs::WholeHost,
        ..spec()
    }
}

/// A start a held host refused, of a job placed on a share of the host, of the same job once
/// it is starting, and of a whole-host job once its host was ready, is answered `Accepted`
/// and sends the job back to the queue in one commit: no host and no placement time, the host
/// in its held refusals and barred for [`HELD_BAR`] from the store's time, a `REQUEUED` event
/// naming the host and the job waiting again as it waited before it was placed; its allocation
/// is freed with `Refused`. No start failure is told, no failed prepare counted and no start
/// failure detail stored, and the reply sends the host neither a stop nor a start of the job.
/// Catches a held refusal that fails the job (as before), that counts it as a failure, that
/// leaves its allocation booking the held host, and that does not keep the job off the host.
#[test]
fn a_start_refused_as_held_sends_the_job_back_to_the_queue_and_counts_no_failure() {
    let cases = [
        (
            "share-placed",
            spec(),
            BOOKED,
            false,
            QueueCode::WaitingForCapacity,
        ),
        (
            "share-starting",
            spec(),
            BOOKED,
            true,
            QueueCode::WaitingForCapacity,
        ),
        (
            "whole-starting",
            whole_spec(),
            Booked::WholeHost,
            true,
            QueueCode::WaitingForWholeHost,
        ),
    ];
    for (name, spec, booked, starting, waits) in cases {
        let (store, jobs) = tables();
        let (placed_job, booking) = placed(&jobs, name, "host-a", spec, booked);
        if starting {
            jobs.transition(&placed_job.id, JobState::Placed, JobState::Starting)
                .unwrap();
        }
        let now = jobs.now().unwrap();
        let failures = Failures::default();
        let commits = store.commit_stats().txns;
        let reply = beat(&jobs, &booking, refusal(&booking, "host-held"), &failures).unwrap();
        assert_eq!(
            store.commit_stats().txns,
            commits + 1,
            "{name}: the requeue took other than one commit"
        );
        assert_eq!(reply.job_acks[0].ack, Ack::Accepted, "{name}");
        assert_eq!(
            reply.commands,
            Vec::<Command>::new(),
            "{name}: a stop or a start"
        );
        assert!(reply.clean, "{name}: the listing was not clean");
        let requeued = job(&jobs, &placed_job.id);
        assert_eq!(
            (requeued.state, requeued.host.clone(), requeued.placed_at),
            (JobState::Queued, None, None),
            "{name}: the job did not go back to the queue"
        );
        assert_eq!(requeued.held_refusals, [host("host-a")], "{name}");
        assert_eq!(
            requeued.prepare_bars.get(&host("host-a")),
            Some(&Exclusion::Until(now.saturating_add(HELD_BAR))),
            "{name}: the host is not barred for as long as a hold's cordon"
        );
        assert_eq!(
            (
                requeued.failed_prepares,
                requeued.start_failure_detail.clone()
            ),
            (0, None),
            "{name}: a failure was counted or a detail stored"
        );
        let event = requeued.queue.events().last().cloned().unwrap();
        assert_eq!(
            (event.kind, event.host, requeued.queue.code()),
            (JobEventKind::Requeued, Some(host("host-a")), waits),
            "{name}: the queue status does not say why the job waits again"
        );
        let freed = allocation(&jobs, &placed_job.id);
        assert_eq!(
            (freed.step, freed.end_reason),
            (AllocationStep::Freed, Some(EndReason::Refused)),
            "{name}: the allocation was not freed as refused"
        );
        assert_eq!(failures.told(), [], "{name}: a start failure was told");
    }
}

/// A requeued job's refusal listed again (its verdict was lost) is answered `Stale` with a
/// stop, as for any job the host holds no live allocation of: it is not requeued twice and
/// counts nothing. Catches a second requeue, or a refusal counted, for one refusal.
#[test]
fn a_held_refusal_listed_again_after_the_requeue_is_stale_and_requeues_nothing() {
    let (_store, jobs) = tables();
    let (placed_job, booking) = placed(&jobs, "again", "host-a", spec(), BOOKED);
    let failures = Failures::default();
    assert_eq!(
        refused(&jobs, &booking, "host-held", &failures),
        Ack::Accepted
    );
    let reply = beat(&jobs, &booking, refusal(&booking, "host-held"), &failures).unwrap();
    assert_eq!(reply.job_acks[0].ack, Ack::Stale);
    assert_eq!(
        reply.commands,
        [Command::JobStop {
            job_id: "again".to_owned()
        }]
    );
    let requeued = job(&jobs, &placed_job.id);
    assert_eq!(
        (requeued.state, requeued.held_refusals),
        (JobState::Queued, vec![host("host-a")])
    );
    assert_eq!(failures.told(), []);
}

/// The refusal after [`MAX_HELD_REQUEUES`] requeues ends the job as a refused start ends,
/// failed to start with `AgentRefused` and told once, with a detail that names each host that
/// refused it once, in the order each first did; every refusal before it requeued the job.
/// Catches a bound that is never reached (a job a fault keeps refusing stays queued for ever),
/// one reached a refusal early, and a detail that does not say which hosts refused.
#[test]
fn the_refusal_after_the_last_held_requeue_ends_the_job_naming_every_host() {
    let (_store, jobs) = tables();
    let (placed_job, mut booking) = placed(&jobs, "bounded", "host-a", spec(), BOOKED);
    let failures = Failures::default();
    let hosts = ["host-b", "host-a", "host-c"];
    assert_eq!(hosts.len(), MAX_HELD_REQUEUES);
    for next in hosts {
        assert_eq!(
            refused(&jobs, &booking, "host-held", &failures),
            Ack::Accepted
        );
        assert_eq!(job(&jobs, &placed_job.id).state, JobState::Queued);
        (_, booking) = jobs.place(&placed_job.id, host(next), BOOKED).unwrap();
    }
    assert_eq!(
        refused(&jobs, &booking, "host-held", &failures),
        Ack::Accepted
    );
    let ended = job(&jobs, &placed_job.id);
    assert_eq!(
        ended.state,
        JobState::FailedToStart(StartFailure::AgentRefused)
    );
    assert_eq!(
        ended.start_failure_detail.as_deref(),
        Some("host-held: host-a, host-b, host-c")
    );
    assert_eq!(
        ended.held_refusals,
        [host("host-a"), host("host-b"), host("host-a")]
    );
    assert_eq!(failures.told(), [StartFailure::AgentRefused]);
    assert_eq!(
        allocation(&jobs, &placed_job.id).step,
        AllocationStep::Releasing
    );
}

/// A start refused as held of a job whose cancel was asked for ends it as before (failed to
/// start, with the code), as does a refusal with another code; a refusal listed for a job
/// that is running is answered `Stale` and moves nothing. None of them requeues the job.
/// Catches a cancelled job sent back to the queue, a refusal of another code taken for a hold,
/// and a running job requeued.
#[test]
fn a_held_refusal_of_a_cancelled_or_running_job_and_another_code_are_not_requeued() {
    let (_store, jobs) = tables();
    let failures = Failures::default();
    let (cancelled, booking) = placed(&jobs, "cancelled", "host-a", spec(), BOOKED);
    jobs.request_cancel(&cancelled.id).unwrap();
    assert_eq!(
        refused(&jobs, &booking, "host-held", &failures),
        Ack::Accepted
    );
    let (other, other_booking) = placed(&jobs, "other", "host-a", spec(), BOOKED);
    assert_eq!(
        refused(&jobs, &other_booking, "input-not-allowed", &failures),
        Ack::Accepted
    );
    for (id, detail) in [
        (&cancelled.id, "host-held"),
        (&other.id, "input-not-allowed"),
    ] {
        let ended = job(&jobs, id);
        assert_eq!(
            (ended.state, ended.start_failure_detail.as_deref()),
            (
                JobState::FailedToStart(StartFailure::AgentRefused),
                Some(detail)
            ),
            "{id}"
        );
        assert_eq!(ended.held_refusals, Vec::<HostId>::new(), "{id}");
    }

    let (running, running_booking) = placed(&jobs, "running", "host-a", spec(), BOOKED);
    jobs.transition(&running.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&running.id, JobState::Starting, JobState::Running)
        .unwrap();
    assert_eq!(
        refused(&jobs, &running_booking, "host-held", &failures),
        Ack::Stale
    );
    assert_eq!(job(&jobs, &running.id).state, JobState::Running);
    assert_eq!(allocation(&jobs, &running.id).step, AllocationStep::Booked);
    assert_eq!(
        failures.told(),
        [StartFailure::AgentRefused, StartFailure::AgentRefused]
    );
}

/// A requeue whose commit fails is the heartbeat's error, and the job stays placed on its
/// booked allocation, so the agent's next report is judged again. Catches a failed requeue
/// answered as if it had landed, which would leave the job placed on a held host with its
/// refusal dropped.
#[test]
fn a_held_requeue_whose_commit_fails_is_the_heartbeats_error_and_moves_nothing() {
    let (store, jobs) = tables();
    let (placed_job, booking) = placed(&jobs, "failing", "host-a", spec(), BOOKED);
    store.fail_next();
    let failures = Failures::default();
    let answer = beat(&jobs, &booking, refusal(&booking, "host-held"), &failures);
    assert!(
        matches!(answer, Err(JobStoreError::Store(StoreError::Failed(_)))),
        "{answer:?}"
    );
    assert_eq!(job(&jobs, &placed_job.id).state, JobState::Placed);
    assert_eq!(
        allocation(&jobs, &placed_job.id).step,
        AllocationStep::Booked
    );
    assert_eq!(failures.told(), []);
}

/// The detail names each host once, the latest refusal's host last unless it refused before,
/// and the code first. Catches a list with repeats, out of order or without the code.
#[test]
fn the_detail_names_each_host_once_in_the_order_each_first_refused() {
    assert_eq!(named(&[], &host("host-a")), "host-held: host-a");
    let earlier = [host("host-b"), host("host-a"), host("host-b")];
    assert_eq!(
        named(&earlier, &host("host-c")),
        "host-held: host-b, host-a, host-c"
    );
    assert_eq!(
        named(&earlier, &host("host-a")),
        "host-held: host-b, host-a"
    );
}
