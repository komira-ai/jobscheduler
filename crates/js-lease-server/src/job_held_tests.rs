//! A start a held host or a host with no room refused, served through the per-job half of a
//! heartbeat on the job tables alone: the job goes back to the queue with no failure counted,
//! the bound on such requeues of each code ends it naming every host, the two bounds are kept
//! apart, and a cancelled or running job, or another code, ends as before. A start refused
//! while its host fetches the job's bundle is in [`bundle`].

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
use crate::job_expiry::JobLimits;
use crate::job_store::ServerJobs;
use crate::metrics::MetricsSink;
use crate::per_job::{BeatContext, JobReply, serve};

#[path = "job_held_bundle_tests.rs"]
mod bundle;

/// The capacity requeue count and bar a pool that sets none has.
const MAX_CAPACITY_REQUEUES: usize = JobLimits::DEFAULT.capacity_requeues;
const CAPACITY_BAR: Duration = JobLimits::DEFAULT.capacity_refusal_bar;

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A sink that records the start failures and the refused starts it is told of, and counts
/// nothing else.
#[derive(Debug, Default)]
struct Failures(Mutex<Vec<StartFailure>>, Mutex<Vec<(String, String)>>);

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
    fn start_refused(&self, host: &HostId, code: &str) {
        self.1
            .lock()
            .unwrap()
            .push((host.to_string(), code.to_owned()));
    }
}

impl Failures {
    fn told(&self) -> Vec<StartFailure> {
        self.0.lock().unwrap().clone()
    }

    /// The refused starts told of, each as its host and code.
    fn refused_told(&self) -> Vec<(String, String)> {
        self.1.lock().unwrap().clone()
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
    beat_under(JobLimits::DEFAULT, jobs, allocation, listed, metrics)
}

/// [`beat`] for a pool whose limits are `limits`.
fn beat_under(
    limits: JobLimits,
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
        limits,
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

/// A requeue whose commit fails, after a held or a capacity refusal, is the heartbeat's error,
/// and the job stays placed on its booked allocation, so the agent's next report is judged
/// again; the refusal is not counted until that report's requeue commits. Catches a failed
/// requeue answered as if it had landed, which would leave the job placed on a refusing host
/// with its refusal dropped, and a refusal counted twice when the agent lists it again.
#[test]
fn a_held_requeue_whose_commit_fails_is_the_heartbeats_error_and_moves_nothing() {
    for code in ["host-held", "no-capacity"] {
        let (store, jobs) = tables();
        let (placed_job, booking) = placed(&jobs, "failing", "host-a", spec(), BOOKED);
        store.fail_next();
        let failures = Failures::default();
        let answer = beat(&jobs, &booking, refusal(&booking, code), &failures);
        assert!(
            matches!(answer, Err(JobStoreError::Store(StoreError::Failed(_)))),
            "{code}: {answer:?}"
        );
        assert_eq!(job(&jobs, &placed_job.id).state, JobState::Placed, "{code}");
        assert_eq!(
            allocation(&jobs, &placed_job.id).step,
            AllocationStep::Booked,
            "{code}"
        );
        assert_eq!(failures.told(), [], "{code}");
        assert_eq!(
            failures.refused_told(),
            Vec::<(String, String)>::new(),
            "{code}: a refusal whose requeue did not commit was counted"
        );
    }
}

/// A capacity refusal on a host the job is still barred from for longer (a held host's bar,
/// which outlasts [`CAPACITY_BAR`]) keeps the longer bar. Catches the short capacity bar
/// replacing a longer one, which would put the job back on a held host early.
#[test]
fn a_capacity_refusal_keeps_a_longer_bar_on_its_host() {
    let failures = Failures::default();
    let (_store, jobs) = tables();
    let (barred_job, booking) = placed(&jobs, "barred", "host-a", spec(), BOOKED);
    let held_at = jobs.now().unwrap();
    // Held on host-a, then placed on host-a again (as a placement past the bar would) and
    // refused there for capacity.
    let booking = refused_then_placed(&jobs, &booking, "host-held", "host-a", &failures);
    assert_eq!(
        refused(&jobs, &booking, "no-capacity", &failures),
        Ack::Accepted
    );
    let requeued = job(&jobs, &barred_job.id);
    assert_eq!(requeued.capacity_refusals, [host("host-a")]);
    let Some(Exclusion::Until(until)) = requeued.prepare_bars.get(&host("host-a")).cloned() else {
        panic!("no bar on host-a: {:?}", requeued.prepare_bars);
    };
    assert!(
        until >= held_at.saturating_add(HELD_BAR),
        "the capacity bar replaced the held one"
    );
    assert!(HELD_BAR > CAPACITY_BAR);
}

/// The detail names each host once, the latest refusal's host last unless it refused before,
/// and the code first. Catches a list with repeats, out of order or without the code.
#[test]
fn the_detail_names_each_host_once_in_the_order_each_first_refused() {
    assert_eq!(
        named("host-held", &[], &host("host-a")),
        "host-held: host-a"
    );
    let earlier = [host("host-b"), host("host-a"), host("host-b")];
    assert_eq!(
        named("host-held", &earlier, &host("host-c")),
        "host-held: host-b, host-a, host-c"
    );
    assert_eq!(
        named("host-held", &earlier, &host("host-a")),
        "host-held: host-b, host-a"
    );
    assert_eq!(
        named("no-capacity", &earlier, &host("host-c")),
        "no-capacity: host-b, host-a, host-c"
    );
}

/// Sends the job `id` back to the queue with a refusal `code` of its start on `booking`'s host
/// and places it again on `next`, as the placement pass does, returning the new booking.
fn refused_then_placed(
    jobs: &ServerJobs,
    booking: &AllocationRecord,
    code: &str,
    next: &str,
    metrics: &Failures,
) -> AllocationRecord {
    assert_eq!(refused(jobs, booking, code, metrics), Ack::Accepted);
    assert_eq!(job(jobs, &booking.job).state, JobState::Queued);
    jobs.place(&booking.job, host(next), BOOKED).unwrap().1
}

/// A start a host refused for capacity, of a job placed on a share of the host and of the same
/// job once it is starting, is answered `Accepted` and sends the job back to the queue in one
/// commit: no host and no placement time, the host in its capacity refusals (and not in its held
/// ones) and barred for [`CAPACITY_BAR`] from the store's time, a `REQUEUED` event naming the
/// host and the job waiting for capacity with the reason in its detail; its allocation is freed
/// with `Refused`. No start failure is told, no failed prepare counted and no start failure
/// detail stored, and the reply sends the host neither a stop nor a start of the job.
/// Catches a capacity refusal that fails the job (as before), that counts it as a failure, that
/// leaves its allocation booking the host, that does not keep the job off the host and that is
/// written as a held refusal.
#[test]
fn a_start_refused_for_capacity_sends_the_job_back_to_the_queue_and_counts_no_failure() {
    for (name, starting) in [("placed", false), ("starting", true)] {
        let (store, jobs) = tables();
        let (placed_job, booking) = placed(&jobs, name, "host-a", spec(), BOOKED);
        if starting {
            jobs.transition(&placed_job.id, JobState::Placed, JobState::Starting)
                .unwrap();
        }
        let now = jobs.now().unwrap();
        let failures = Failures::default();
        let commits = store.commit_stats().txns;
        let reply = beat(&jobs, &booking, refusal(&booking, "no-capacity"), &failures).unwrap();
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
        let requeued = job(&jobs, &placed_job.id);
        assert_eq!(
            (requeued.state, requeued.host.clone(), requeued.placed_at),
            (JobState::Queued, None, None),
            "{name}: the job did not go back to the queue"
        );
        assert_eq!(requeued.capacity_refusals, [host("host-a")], "{name}");
        assert_eq!(requeued.held_refusals, Vec::<HostId>::new(), "{name}");
        assert_eq!(
            requeued.prepare_bars.get(&host("host-a")),
            Some(&Exclusion::Until(now.saturating_add(CAPACITY_BAR))),
            "{name}: the host is not barred for the capacity bar"
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
            (
                JobEventKind::Requeued,
                Some(host("host-a")),
                QueueCode::WaitingForCapacity
            ),
            "{name}: the queue status does not say why the job waits again"
        );
        assert_eq!(
            requeued.queue.detail(),
            "host host-a had no room for the job when its start reached it; the job is queued again",
            "{name}"
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

/// A refusal for capacity listed again after the requeue is answered `Stale` with a stop and
/// requeues nothing. Catches a second requeue, or a refusal counted, for one refusal.
#[test]
fn a_capacity_refusal_listed_again_after_the_requeue_is_stale_and_requeues_nothing() {
    let (_store, jobs) = tables();
    let (placed_job, booking) = placed(&jobs, "again", "host-a", spec(), BOOKED);
    let failures = Failures::default();
    assert_eq!(
        refused(&jobs, &booking, "no-capacity", &failures),
        Ack::Accepted
    );
    let reply = beat(&jobs, &booking, refusal(&booking, "no-capacity"), &failures).unwrap();
    assert_eq!(reply.job_acks[0].ack, Ack::Stale);
    let requeued = job(&jobs, &placed_job.id);
    assert_eq!(
        (requeued.state, requeued.capacity_refusals),
        (JobState::Queued, vec![host("host-a")])
    );
    assert_eq!(failures.told(), []);
}

/// The refusal after [`MAX_CAPACITY_REQUEUES`] requeues ends the job as a refused start ends,
/// failed to start with `AgentRefused` and told once, with a detail that names each host that
/// refused it once, in the order each first did; every refusal before it requeued the job.
/// Catches a bound that is never reached, one reached a refusal early, and a detail that does
/// not say which hosts refused.
#[test]
fn the_refusal_after_the_last_capacity_requeue_ends_the_job_naming_every_host() {
    let (_store, jobs) = tables();
    let (placed_job, mut booking) = placed(&jobs, "bounded", "host-a", spec(), BOOKED);
    let failures = Failures::default();
    let hosts = [
        "host-b", "host-a", "host-c", "host-b", "host-d", "host-a", "host-e", "host-c",
    ];
    assert_eq!(hosts.len(), MAX_CAPACITY_REQUEUES);
    for next in hosts {
        booking = refused_then_placed(&jobs, &booking, "no-capacity", next, &failures);
    }
    assert_eq!(
        refused(&jobs, &booking, "no-capacity", &failures),
        Ack::Accepted
    );
    let ended = job(&jobs, &placed_job.id);
    assert_eq!(
        ended.state,
        JobState::FailedToStart(StartFailure::AgentRefused)
    );
    assert_eq!(
        ended.start_failure_detail.as_deref(),
        Some("no-capacity: host-a, host-b, host-c, host-d, host-e")
    );
    assert_eq!(ended.capacity_refusals.len(), MAX_CAPACITY_REQUEUES);
    assert_eq!(failures.told(), [StartFailure::AgentRefused]);
    assert_eq!(
        allocation(&jobs, &placed_job.id).step,
        AllocationStep::Releasing
    );
}

/// A start refused for capacity of a job whose cancel was asked for ends it as before (failed
/// to start, with the code, told once) and writes no capacity refusal. Catches a cancelled job
/// sent back to the queue.
#[test]
fn a_capacity_refusal_of_a_cancelled_job_ends_it_as_before() {
    let (_store, jobs) = tables();
    let failures = Failures::default();
    let (cancelled, booking) = placed(&jobs, "cancelled", "host-a", spec(), BOOKED);
    jobs.request_cancel(&cancelled.id).unwrap();
    assert_eq!(
        refused(&jobs, &booking, "no-capacity", &failures),
        Ack::Accepted
    );
    let ended = job(&jobs, &cancelled.id);
    assert_eq!(
        (ended.state, ended.start_failure_detail.as_deref()),
        (
            JobState::FailedToStart(StartFailure::AgentRefused),
            Some("no-capacity")
        )
    );
    assert_eq!(ended.capacity_refusals, Vec::<HostId>::new());
    assert_eq!(failures.told(), [StartFailure::AgentRefused]);
}

/// The two bounds are counted apart: a job [`MAX_HELD_REQUEUES`] held refusals sent back is
/// still requeued by a capacity refusal, and one [`MAX_CAPACITY_REQUEUES`] capacity refusals
/// sent back is still requeued by a held refusal; the next refusal of each code then ends the
/// job naming only the hosts that refused it for that code. Catches a shared count, and a detail
/// that names the hosts of the other code.
#[test]
fn held_and_capacity_refusals_are_counted_apart() {
    let failures = Failures::default();

    let (_store, jobs) = tables();
    let (held_job, mut booking) = placed(&jobs, "held-first", "host-a", spec(), BOOKED);
    for next in ["host-b", "host-c", "host-d"] {
        booking = refused_then_placed(&jobs, &booking, "host-held", next, &failures);
    }
    booking = refused_then_placed(&jobs, &booking, "no-capacity", "host-e", &failures);
    let mixed = job(&jobs, &held_job.id);
    assert_eq!(mixed.held_refusals.len(), MAX_HELD_REQUEUES);
    assert_eq!(mixed.capacity_refusals, [host("host-d")]);
    assert_eq!(
        refused(&jobs, &booking, "host-held", &failures),
        Ack::Accepted
    );
    assert_eq!(
        job(&jobs, &held_job.id).start_failure_detail.as_deref(),
        Some("host-held: host-a, host-b, host-c, host-e")
    );

    let (_store, jobs) = tables();
    let (capacity_job, mut booking) = placed(&jobs, "capacity-first", "host-a", spec(), BOOKED);
    for _ in 0..MAX_CAPACITY_REQUEUES {
        booking = refused_then_placed(&jobs, &booking, "no-capacity", "host-b", &failures);
    }
    booking = refused_then_placed(&jobs, &booking, "host-held", "host-c", &failures);
    let mixed = job(&jobs, &capacity_job.id);
    assert_eq!(mixed.capacity_refusals.len(), MAX_CAPACITY_REQUEUES);
    assert_eq!(mixed.held_refusals, [host("host-b")]);
    assert_eq!(
        refused(&jobs, &booking, "no-capacity", &failures),
        Ack::Accepted
    );
    assert_eq!(
        job(&jobs, &capacity_job.id).start_failure_detail.as_deref(),
        Some("no-capacity: host-a, host-b, host-c")
    );
    assert_eq!(failures.told(), [StartFailure::AgentRefused; 2]);
}

/// A listed refused start is told to the metrics once with its host and code: `no-capacity`
/// on host-a and `input-not-allowed` on host-b, a held host's `host-held` that sends its job
/// back to the queue, and a refusal that lists no code as `none`; a relisting of a result the
/// server already took is answered a duplicate, and of one that sent its job back to the queue
/// stale, and neither is told again. Catches a refusal not
/// counted, counted twice, counted only when the job fails, or counted again when the agent
/// lists it again.
#[test]
fn each_accepted_refused_start_is_told_once_with_its_host_and_code() {
    let (_store, jobs) = tables();
    let failures = Failures::default();
    let (_, on_a) = placed(&jobs, "on-a", "host-a", spec(), BOOKED);
    let (_, on_b) = placed(&jobs, "on-b", "host-b", spec(), BOOKED);
    let (_, held) = placed(&jobs, "held", "host-c", spec(), BOOKED);
    let (_, bare) = placed(&jobs, "bare", "host-d", spec(), BOOKED);
    assert_eq!(
        refused(&jobs, &on_a, "no-capacity", &failures),
        Ack::Accepted
    );
    assert_eq!(
        refused(&jobs, &on_b, "input-not-allowed", &failures),
        Ack::Accepted
    );
    assert_eq!(refused(&jobs, &held, "host-held", &failures), Ack::Accepted);
    let mut none = refusal(&bare, "x");
    none.detail = None;
    beat(&jobs, &bare, none, &failures).unwrap();
    let told = |host: &str, code: &str| (host.to_owned(), code.to_owned());
    let once = [
        told("host-a", "no-capacity"),
        told("host-b", "input-not-allowed"),
        told("host-c", "host-held"),
        told("host-d", "none"),
    ];
    assert_eq!(failures.refused_told(), once);
    assert_eq!(
        refused(&jobs, &on_b, "input-not-allowed", &failures),
        Ack::Duplicate
    );
    assert_eq!(failures.refused_told(), once, "a duplicate was counted");
    // The capacity refusal sent its job back to the queue: its listing again is stale.
    assert_eq!(refused(&jobs, &on_a, "no-capacity", &failures), Ack::Stale);
    assert_eq!(failures.refused_told(), once, "a stale listing was counted");
}

/// A pool that sets `capacity_requeues = 1` ends the job at its second capacity refusal, the
/// detail naming both hosts, while the first refusal still requeues it; a pool that sets 0 ends
/// it at the first; and a pool that sets none still allows the default eight requeues (the
/// test of the ninth refusal above). Catches a count that is still the built-in one.
#[test]
fn a_pool_that_sets_the_capacity_requeue_count_changes_when_the_job_ends() {
    for (count, ends_at) in [(0, 1), (1, 2), (3, 4)] {
        let limits = JobLimits {
            capacity_requeues: count,
            ..JobLimits::DEFAULT
        };
        let failures = Failures::default();
        let (_store, jobs) = tables();
        let (refused_job, mut booking) = placed(&jobs, "counted", "host-a", spec(), BOOKED);
        for refusal_number in 1..ends_at {
            let reply = beat_under(
                limits,
                &jobs,
                &booking,
                refusal(&booking, "no-capacity"),
                &failures,
            )
            .unwrap();
            assert_eq!(reply.job_acks[0].ack, Ack::Accepted);
            assert_eq!(
                job(&jobs, &refused_job.id).state,
                JobState::Queued,
                "count {count}: refusal {refusal_number} ended the job"
            );
            let next = format!(
                "host-{}",
                char::from(b'b' + u8::try_from(refusal_number).unwrap())
            );
            booking = jobs.place(&refused_job.id, host(&next), BOOKED).unwrap().1;
        }
        let reply = beat_under(
            limits,
            &jobs,
            &booking,
            refusal(&booking, "no-capacity"),
            &failures,
        )
        .unwrap();
        assert_eq!(reply.job_acks[0].ack, Ack::Accepted);
        let ended = job(&jobs, &refused_job.id);
        assert_eq!(
            ended.state,
            JobState::FailedToStart(StartFailure::AgentRefused),
            "count {count}: refusal {ends_at} did not end the job"
        );
        assert_eq!(ended.capacity_refusals.len(), ends_at - 1);
        assert_eq!(failures.told(), [StartFailure::AgentRefused]);
    }
}

/// A pool that sets `capacity_refusal_bar = 5m` bars the refusing host for five minutes from
/// the store's time, not for the default minute. Catches a bar that is still the built-in one.
#[test]
fn a_pool_that_sets_the_capacity_refusal_bar_bars_the_host_for_that_long() {
    let bar = Duration::from_secs(300);
    let limits = JobLimits {
        capacity_refusal_bar: bar,
        ..JobLimits::DEFAULT
    };
    let (_store, jobs) = tables();
    let (barred_job, booking) = placed(&jobs, "long-bar", "host-a", spec(), BOOKED);
    let now = jobs.now().unwrap();
    let failures = Failures::default();
    beat_under(
        limits,
        &jobs,
        &booking,
        refusal(&booking, "no-capacity"),
        &failures,
    )
    .unwrap();
    assert_eq!(
        job(&jobs, &barred_job.id).prepare_bars.get(&host("host-a")),
        Some(&Exclusion::Until(now.saturating_add(bar)))
    );
}
