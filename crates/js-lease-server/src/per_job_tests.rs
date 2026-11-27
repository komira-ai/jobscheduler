//! The per-job half of a heartbeat on the job tables alone: what the route tests cannot reach
//! (a retired id that is no job id, listed states that move nothing, a failed end commit, a
//! start the wire cannot carry, the room for commands).

use std::sync::Mutex;
use std::time::Duration;

use js_agent_wire::{
    CpuPeak as WirePeak, CpuSeries as WireSeries, CpuThrottle as WireThrottle, HeartbeatReply,
    HeldJob, HeldState, JobResult, JobUsage, MemorySeries as WireMemorySeries, NetworkCeiling,
    Outcome,
};
use js_jobs::{
    Booked, CpuPeak, CpuThrottle, JobEnd, JobSpec, JobState, JobStoreError, Needs, Output,
    ResourceUsage, ResultReason, StartFailure,
};
use js_state::StoreError;
use js_types::{CpuSeries, HostId, MemorySeries, Resources};

use js_state::Store;

use super::{BeatContext, Command, end_of, serve};
use crate::agent_api::job_rig::{BOOKED, placed, result, spec, starts, stops, tables};
use crate::metrics::{MetricsSink, NoMetrics};

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

fn listed(job_id: &str, state: HeldState, result: Option<JobResult>) -> HeldJob {
    HeldJob {
        job_id: job_id.to_owned(),
        state,
        result,
    }
}

/// A retired id that is no job id (longer than a job id may be) is acknowledged and changes
/// nothing. It catches an answer that drops the id, which the agent would list for ever.
#[test]
fn a_retired_id_that_is_no_job_id_is_acknowledged() {
    let (store, jobs) = tables();
    let long = "x".repeat(70);
    let ceiling = NetworkCeiling::default();
    let before = store.commit_stats().txns;
    let reply = serve(
        &jobs,
        &BeatContext {
            host: &host(),
            ceiling: &ceiling,
            host_jobs: false,
            shape: None,
            logs: None,
            polls_dispatch: false,
            metrics: &NoMetrics,
            layered_rootfs: false,
            mount_providers: &[],
            xcodes: &[],
            limits: crate::job_expiry::JobLimits::DEFAULT,
        },
        &[],
        std::slice::from_ref(&long),
        8,
        None,
    )
    .unwrap();
    assert_eq!(reply.retired_acks, [long]);
    assert!(reply.commands.is_empty() & reply.clean);
    assert_eq!(store.commit_stats().txns, before);
}

/// A job listed as exited with no result, or in a state this version does not know, is not
/// moved: there is no step to commit. It catches a listing read as a step it does not name.
#[test]
fn a_listed_state_that_names_no_step_moves_nothing() {
    let (store, jobs) = tables();
    let (exited, _) = placed(&jobs, "job-exited", "host-a", spec(), BOOKED);
    let (unknown, _) = placed(&jobs, "job-unknown", "host-a", spec(), BOOKED);
    let listing = [
        listed("job-exited", HeldState::ExitedUnacked, None),
        listed("job-unknown", HeldState::Unknown, None),
    ];
    let before = store.commit_stats().txns;
    let ceiling = NetworkCeiling::default();
    let reply = serve(
        &jobs,
        &BeatContext {
            host: &host(),
            ceiling: &ceiling,
            host_jobs: false,
            shape: None,
            logs: None,
            polls_dispatch: false,
            metrics: &NoMetrics,
            layered_rootfs: false,
            mount_providers: &[],
            xcodes: &[],
            limits: crate::job_expiry::JobLimits::DEFAULT,
        },
        &listing,
        &[],
        8,
        None,
    )
    .unwrap();
    assert_eq!(store.commit_stats().txns, before);
    assert!(reply.commands.is_empty() & reply.job_acks.is_empty());
    for id in [exited.id, unknown.id] {
        assert_eq!(jobs.get_job(&id).unwrap().unwrap().state, JobState::Placed);
    }
}

/// A store failure at the commit of a listed end is the error the heartbeat answers 500 for,
/// and the job is left running. It catches an end whose failed commit is acknowledged.
#[test]
fn a_failed_end_commit_is_an_error() {
    let (store, jobs) = tables();
    let (job, allocation) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    jobs.transition(&job.id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(&job.id, JobState::Starting, JobState::Running)
        .unwrap();
    let ended = result(
        "job-a",
        &allocation.allocation_id.to_string(),
        Outcome::Succeeded,
        Some(0),
    );
    let listing = [listed("job-a", HeldState::ExitedUnacked, Some(ended))];
    store.fail_next();
    let ceiling = NetworkCeiling::default();
    let answer = serve(
        &jobs,
        &BeatContext {
            host: &host(),
            ceiling: &ceiling,
            host_jobs: false,
            shape: None,
            logs: None,
            polls_dispatch: false,
            metrics: &NoMetrics,
            layered_rootfs: false,
            mount_providers: &[],
            xcodes: &[],
            limits: crate::job_expiry::JobLimits::DEFAULT,
        },
        &listing,
        &[],
        8,
        None,
    );
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

/// A placed job whose start the wire cannot carry (its allocation books GPUs by count) is
/// sent no start and stays placed; the starts that can be sent stop at the room the reply
/// has. It catches a start sent with made-up device indices, and a reply over its bound.
#[test]
fn an_unsendable_start_is_not_sent_and_starts_stop_at_the_room() {
    let (_store, jobs) = tables();
    let gpus = Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 2,
    };
    let (gpu, _) = placed(&jobs, "job-gpu", "host-a", spec(), gpus);
    placed(&jobs, "job-b", "host-a", spec(), BOOKED);
    placed(&jobs, "job-c", "host-a", spec(), BOOKED);
    let ceiling = NetworkCeiling::default();
    let reply = serve(
        &jobs,
        &BeatContext {
            host: &host(),
            ceiling: &ceiling,
            host_jobs: false,
            shape: None,
            logs: None,
            polls_dispatch: false,
            metrics: &NoMetrics,
            layered_rootfs: false,
            mount_providers: &[],
            xcodes: &[],
            limits: crate::job_expiry::JobLimits::DEFAULT,
        },
        &[],
        &[],
        1,
        None,
    )
    .unwrap();
    let ids = |commands: Vec<Command>| {
        let reply = HeartbeatReply {
            commands,
            ..HeartbeatReply::default()
        };
        let started: Vec<_> = starts(&reply).into_iter().map(|spec| spec.job_id).collect();
        (started, stops(&reply))
    };
    assert_eq!(ids(reply.commands), (vec!["job-b".to_owned()], vec![]));
    let reply = serve(
        &jobs,
        &BeatContext {
            host: &host(),
            ceiling: &ceiling,
            host_jobs: false,
            shape: None,
            logs: None,
            polls_dispatch: false,
            metrics: &NoMetrics,
            layered_rootfs: false,
            mount_providers: &[],
            xcodes: &[],
            limits: crate::job_expiry::JobLimits::DEFAULT,
        },
        &[],
        &[],
        8,
        None,
    )
    .unwrap();
    let both = vec!["job-b".to_owned(), "job-c".to_owned()];
    assert_eq!(ids(reply.commands), (both, vec![]));
    assert_eq!(
        jobs.get_job(&gpu.id).unwrap().unwrap().state,
        JobState::Placed
    );
}

/// A job that books the whole host is, while it is placed, sent no stop (the agent holds no
/// container for it, so a stop would be answered retired and free a host still preparing) and
/// no start, and the reply names its allocation unless a cancel intent lets the host go; once
/// it is starting it is sent its start, with the CPU, memory and disk the registry books for
/// the host, until the host lists it, and the reply goes on naming it while it runs; with a
/// cancel intent a running one is stopped. It catches a stop for a preparing host, a start
/// before the host is ready, a start with the booking of a share, a start resent after it is
/// listed, and a reply that names a cancelled job's host.
#[test]
fn a_whole_host_job_is_started_only_once_ready_and_never_stopped_while_preparing() {
    let (_store, jobs) = tables();
    let whole = || JobSpec {
        needs: Needs::WholeHost,
        ..spec()
    };
    let (job, allocation) = placed(&jobs, "job-w", "host-a", whole(), Booked::WholeHost);
    let leased = allocation.allocation_id.to_string();
    let ceiling = NetworkCeiling::default();
    let booked = Resources::new(8000, 16 << 30, 0, None)
        .unwrap()
        .with_disk_bytes(3 << 30);
    let context = BeatContext {
        host: &host(),
        ceiling: &ceiling,
        host_jobs: false,
        shape: Some(&booked),
        logs: None,
        polls_dispatch: false,
        metrics: &NoMetrics,
        layered_rootfs: false,
        mount_providers: &[],
        xcodes: &[],
        limits: crate::job_expiry::JobLimits::DEFAULT,
    };
    let beat = |listed: &[HeldJob]| serve(&jobs, &context, listed, &[], 8, None).unwrap();

    let reply = beat(&[]);
    assert_eq!((reply.commands.len(), reply.exclusive), (0, Some(leased)));

    jobs.request_cancel(&job.id).unwrap();
    let reply = beat(&[]);
    assert_eq!((reply.commands.len(), reply.exclusive), (0, None));

    jobs.end_job(&job.id, JobEnd::Lost).unwrap();
    let (second, next) = placed(&jobs, "job-x", "host-a", whole(), Booked::WholeHost);
    let next_lease = next.allocation_id.to_string();
    jobs.transition(&second.id, JobState::Placed, JobState::Starting)
        .unwrap();
    let reply = beat(&[]);
    assert_eq!(reply.exclusive, Some(next_lease.clone()));
    let sent = starts(&HeartbeatReply {
        commands: reply.commands,
        ..HeartbeatReply::default()
    });
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].job_id, "job-x");
    assert_eq!(sent[0].allocation_id, next_lease);
    assert_eq!(sent[0].booked.cpu_millis, 8000);
    assert_eq!(sent[0].booked.memory_bytes, 16 << 30);
    assert_eq!(sent[0].booked.disk_bytes, Some(3 << 30));

    let held = [listed("job-x", HeldState::Running, None)];
    let reply = beat(&held);
    assert_eq!(
        (reply.commands.len(), reply.exclusive),
        (0, Some(next_lease.clone()))
    );

    jobs.request_cancel(&second.id).unwrap();
    let reply = beat(&held);
    assert_eq!(reply.exclusive, Some(next_lease));
    let asked = HeartbeatReply {
        commands: reply.commands,
        ..HeartbeatReply::default()
    };
    assert_eq!(stops(&asked), ["job-x"]);
    assert!(starts(&asked).is_empty());
}

/// The tail every [`result`] carries, as the stored end keeps it.
fn done() -> Output {
    Output {
        stdout: "done\n".to_owned(),
        stderr: String::new(),
        truncated: false,
        usage: None,
        logs: None,
        cpu_series: None,
        memory_series: None,
    }
}

/// A result of the job `job-a` ending with `outcome` and `exit_code`.
fn ended(outcome: Outcome, exit_code: Option<i32>) -> JobResult {
    result("job-a", "00", outcome, exit_code)
}

/// A start the agent refused with the code `detail` ends as the agent's refusal keeping the
/// code: `input-unavailable` is the host's input failing, `input-not-allowed` and a code this
/// version does not know are the agent's refusal, and a refusal with no code keeps none. It
/// catches a code that is dropped, a refusal of the host's input blamed on the agent, and a
/// new code read as the host's input.
#[test]
fn a_refused_start_keeps_the_agents_code_and_names_the_input_that_was_unavailable() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let refused = |detail: Option<&str>| {
        let mut refusal = ended(Outcome::FailedToStart, None);
        refusal.detail = detail.map(str::to_owned);
        end_of(&job, &refusal)
    };
    let kept = |failure, detail: &str| {
        Some(JobEnd::Refused {
            failure,
            detail: detail.to_owned(),
        })
    };
    assert_eq!(
        refused(Some("input-unavailable")),
        kept(StartFailure::InputUnavailable, "input-unavailable")
    );
    assert_eq!(
        refused(Some("input-not-allowed")),
        kept(StartFailure::AgentRefused, "input-not-allowed")
    );
    assert_eq!(
        refused(Some("some-later-code")),
        kept(StartFailure::AgentRefused, "some-later-code")
    );
    assert_eq!(
        refused(None),
        Some(JobEnd::FailedToStart(StartFailure::AgentRefused))
    );
}

/// A job that ran and whose result names an input that failed ends as an input failure, with
/// exactly the exit status the agent reported (none stays none, a stop nobody asked for and a
/// cancel keep theirs), the failure's code and the output, whatever the outcome: succeeded,
/// failed, killed with and without a cancel intent, timed out, out of memory. It catches an
/// input failure hidden behind the outcome's own end, an exit status made up or dropped, and
/// a code that is lost.
#[test]
fn an_input_failure_on_a_job_that_ran_ends_it_as_an_input_failure_whatever_the_outcome() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let mut cancelled = job.clone();
    cancelled.cancel_requested = true;
    let outcomes = [
        (&job, Outcome::Succeeded, Some(0)),
        (&job, Outcome::Failed, Some(2)),
        (&job, Outcome::Killed, Some(137)),
        (&job, Outcome::Killed, None),
        (&cancelled, Outcome::Killed, Some(143)),
        (&cancelled, Outcome::Killed, None),
        (&job, Outcome::TimedOut, Some(124)),
        (&job, Outcome::TimedOut, None),
        (&job, Outcome::OutOfMemory, Some(137)),
        (&job, Outcome::OutOfMemory, None),
    ];
    for code in ["input-unreadable", "host-port-lost"] {
        for (record, outcome, exit_code) in &outcomes {
            let mut reported = ended(*outcome, *exit_code);
            reported.input_failure = Some(code.to_owned());
            assert_eq!(
                end_of(record, &reported),
                Some(JobEnd::InputFailed {
                    exit_code: *exit_code,
                    input_failure: code.to_owned(),
                    output: done(),
                }),
                "{code} {outcome:?} {exit_code:?}"
            );
        }
    }
}

/// A result that never ran the job ignores an input failure it names: a start the agent
/// refused ends by its refusal (the code kept, none when it gave none), and an outcome this
/// version does not know ends nothing. It catches a job that never ran blamed on an input
/// that failed while it ran.
#[test]
fn an_input_failure_on_a_job_that_never_ran_is_ignored() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let mut refused = ended(Outcome::FailedToStart, None);
    refused.input_failure = Some("input-unreadable".to_owned());
    assert_eq!(
        end_of(&job, &refused),
        Some(JobEnd::FailedToStart(StartFailure::AgentRefused))
    );
    refused.detail = Some("input-unavailable".to_owned());
    assert_eq!(
        end_of(&job, &refused),
        Some(JobEnd::Refused {
            failure: StartFailure::InputUnavailable,
            detail: "input-unavailable".to_owned(),
        })
    );
    let mut unknown = ended(Outcome::Unknown, None);
    unknown.input_failure = Some("host-port-lost".to_owned());
    assert_eq!(end_of(&job, &unknown), None);
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

/// Serves one heartbeat of `host-a` listing `result` for a job, reporting to `metrics`, and
/// returns the verdict on it.
fn listed_end(
    jobs: &crate::job_store::ServerJobs,
    metrics: &Failures,
    result: JobResult,
) -> Option<js_agent_wire::Ack> {
    let ceiling = NetworkCeiling::default();
    let context = BeatContext {
        host: &host(),
        ceiling: &ceiling,
        host_jobs: false,
        shape: None,
        logs: None,
        polls_dispatch: false,
        metrics,
        layered_rootfs: false,
        mount_providers: &[],
        xcodes: &[],
        limits: crate::job_expiry::JobLimits::DEFAULT,
    };
    let listing = [listed("job-a", HeldState::ExitedUnacked, Some(result))];
    let reply = serve(jobs, &context, &listing, &[], 8, None).unwrap();
    reply.job_acks.first().map(|ack| ack.ack)
}

/// A result with an input failure moves a job the host listed as starting, or running, to
/// failed with the reason input failed, the agent's code of the input and the exit status it
/// reported kept in the stored result, and no start failure is counted for it. It catches an
/// input failure stored as a plain exit, one that loses its code, and one counted as a start
/// that failed.
#[test]
fn an_input_failed_result_fails_the_job_with_the_reason_and_the_code() {
    for running in [false, true] {
        let (_store, jobs) = tables();
        let (job, allocation) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
        jobs.transition(&job.id, JobState::Placed, JobState::Starting)
            .unwrap();
        if running {
            jobs.transition(&job.id, JobState::Starting, JobState::Running)
                .unwrap();
        }
        let mut reported = result(
            "job-a",
            &allocation.allocation_id.to_string(),
            Outcome::Failed,
            Some(2),
        );
        reported.input_failure = Some("host-port-lost".to_owned());
        let failures = Failures::default();
        let ack = listed_end(&jobs, &failures, reported);
        assert_eq!(ack, Some(js_agent_wire::Ack::Accepted), "{running}");
        let stored = jobs.get_job(&job.id).unwrap().unwrap();
        assert_eq!(stored.state, JobState::Failed, "{running}");
        let kept = stored.result.unwrap();
        assert_eq!(kept.reason, ResultReason::InputFailed, "{running}");
        assert_eq!(kept.input_failure.as_deref(), Some("host-port-lost"));
        assert_eq!(kept.exit_code, Some(2));
        assert_eq!(kept.stdout_tail, "done\n");
        assert!(failures.0.lock().unwrap().is_empty());
    }
}

/// A start the agent refused with `input-unavailable` ends the job as failed to start with
/// the start failure input unavailable, stores the agent's code as the start failure detail
/// and counts the start failure once; a refusal with another code is counted as the agent's
/// and keeps its code, and one with no code stores no detail. It catches a refusal of the
/// host's input counted or shown as the agent's, and a code that is not stored.
#[test]
fn a_refused_start_is_stored_with_its_code_and_counted() {
    let cases = [
        (
            Some("input-unavailable"),
            StartFailure::InputUnavailable,
            Some("input-unavailable"),
        ),
        (
            Some("input-not-allowed"),
            StartFailure::AgentRefused,
            Some("input-not-allowed"),
        ),
        (None, StartFailure::AgentRefused, None),
    ];
    for (detail, failure, stored_detail) in cases {
        let (_store, jobs) = tables();
        let (job, allocation) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
        let mut refusal = result(
            "job-a",
            &allocation.allocation_id.to_string(),
            Outcome::FailedToStart,
            None,
        );
        refusal.detail = detail.map(str::to_owned);
        let failures = Failures::default();
        let ack = listed_end(&jobs, &failures, refusal);
        assert_eq!(ack, Some(js_agent_wire::Ack::Accepted), "{detail:?}");
        let stored = jobs.get_job(&job.id).unwrap().unwrap();
        assert_eq!(stored.state, JobState::FailedToStart(failure), "{detail:?}");
        assert_eq!(stored.start_failure_detail.as_deref(), stored_detail);
        assert_eq!(*failures.0.lock().unwrap(), [failure], "{detail:?}");
    }
}

/// A result that carries usage ends the job with that usage stored in its output, whatever the
/// way it ended (exited, failed, an input failure), as the exact CPU time and peak memory the
/// agent counted; a result without usage stores none, never a zero. It catches usage dropped
/// on the way to the stored result (every job then reads as never measured), a unit slip
/// between microseconds and the stored time, and an unknown made into a zero.
#[test]
fn the_usage_of_a_result_is_stored_with_the_jobs_output() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let counted = JobUsage {
        peak_memory_bytes: 3 << 30,
        cpu_usage_usec: 4_500_001,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak: None,
        peak_swap_bytes: None,
    };
    let stored = ResourceUsage {
        cpu_time: Duration::new(4, 500_001_000),
        peak_memory_bytes: 3 << 30,
        cpu_throttle: None,
        cpu_pressure_some: None,
        cpu_peak: None,
        peak_swap_bytes: None,
    };
    let mut measured = ended(Outcome::Succeeded, Some(0));
    measured.usage = Some(counted);
    let Some(JobEnd::Exited { output, .. }) = end_of(&job, &measured) else {
        panic!("a clean exit ends exited");
    };
    assert_eq!(output.usage, Some(stored));
    let mut failed = ended(Outcome::Failed, Some(2));
    failed.usage = Some(counted);
    let Some(JobEnd::Exited { output, .. }) = end_of(&job, &failed) else {
        panic!("a nonzero exit ends exited");
    };
    assert_eq!(output.usage, Some(stored));
    let mut input = ended(Outcome::Succeeded, Some(0));
    input.usage = Some(counted);
    input.input_failure = Some("input-unreadable".to_owned());
    let Some(JobEnd::InputFailed { output, .. }) = end_of(&job, &input) else {
        panic!("an input failure ends so");
    };
    assert_eq!(output.usage, Some(stored));
    let Some(JobEnd::Exited { output, .. }) = end_of(&job, &ended(Outcome::Succeeded, Some(0)))
    else {
        panic!("a clean exit ends exited");
    };
    assert_eq!(output.usage, None);
}

/// A usage that reports the CPU periods, the throttled periods and the CPU pressure stores each
/// with the output (the pressure in microseconds as a duration), and a usage that does not
/// stores `None` for each, not a zero. It catches a counter dropped on the way to the stored
/// result, a unit slip on the pressure, and a counter that is absent made into a zero.
#[test]
fn the_cpu_counters_of_a_usage_are_stored_when_reported_and_none_when_not() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let mut reported = ended(Outcome::Succeeded, Some(0));
    reported.usage = Some(JobUsage {
        peak_memory_bytes: 1 << 30,
        cpu_usage_usec: 2_000_000,
        cpu_throttle: Some(WireThrottle {
            periods: 600,
            throttled_periods: 45,
        }),
        cpu_pressure_some_usec: Some(1_250_001),
        cpu_peak: None,
        peak_swap_bytes: None,
    });
    let Some(JobEnd::Exited { output, .. }) = end_of(&job, &reported) else {
        panic!("a clean exit ends exited");
    };
    assert_eq!(
        output.usage,
        Some(ResourceUsage {
            cpu_time: Duration::from_secs(2),
            peak_memory_bytes: 1 << 30,
            cpu_throttle: Some(CpuThrottle {
                periods: 600,
                throttled_periods: 45,
            }),
            cpu_pressure_some: Some(Duration::new(1, 250_001_000)),
            cpu_peak: None,
            peak_swap_bytes: None,
        })
    );
    let mut older = ended(Outcome::Succeeded, Some(0));
    older.usage = Some(JobUsage {
        peak_memory_bytes: 1 << 30,
        cpu_usage_usec: 2_000_000,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak: None,
        peak_swap_bytes: None,
    });
    let Some(JobEnd::Exited { output, .. }) = end_of(&job, &older) else {
        panic!("a clean exit ends exited");
    };
    assert_eq!(
        output.usage,
        Some(ResourceUsage {
            cpu_time: Duration::from_secs(2),
            peak_memory_bytes: 1 << 30,
            cpu_throttle: None,
            cpu_pressure_some: None,
            cpu_peak: None,
            peak_swap_bytes: None,
        })
    );
}

/// A usage that reports a CPU peak stores its millicores and its window with the output, and
/// a usage that does not stores `None`, not a zero. It catches a peak dropped on the way to the
/// stored result, its two numbers swapped, and an absent peak made into a zero.
#[test]
fn the_cpu_peak_of_a_usage_is_stored_when_reported_and_none_when_not() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let usage = |cpu_peak| JobUsage {
        peak_memory_bytes: 1 << 30,
        cpu_usage_usec: 2_000_000,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak,
        peak_swap_bytes: None,
    };
    let stored = |reported: JobUsage| {
        let mut result = ended(Outcome::Succeeded, Some(0));
        result.usage = Some(reported);
        let Some(JobEnd::Exited { output, .. }) = end_of(&job, &result) else {
            panic!("a clean exit ends exited");
        };
        output.usage.expect("a usage").cpu_peak
    };
    assert_eq!(
        stored(usage(Some(WirePeak {
            millicores: 3_500,
            window_usec: 5_000_000,
        }))),
        Some(CpuPeak {
            millicores: 3_500,
            window_usec: 5_000_000,
        })
    );
    assert_eq!(stored(usage(None)), None);
}

/// A usage that reports a swap peak stores it with the output, a swap peak of 0 (measured, none
/// used) stores `Some(0)`, and a usage that does not report one stores `None`, not a zero. It
/// catches a swap peak dropped between the agent's report and the stored result (the estimator
/// then never sees a run that swapped) and an absent peak made into a zero.
#[test]
fn the_swap_peak_of_a_usage_is_stored_when_reported_and_none_when_not() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let stored = |peak_swap_bytes| {
        let mut result = ended(Outcome::Succeeded, Some(0));
        result.usage = Some(JobUsage {
            peak_memory_bytes: 1 << 30,
            cpu_usage_usec: 2_000_000,
            cpu_throttle: None,
            cpu_pressure_some_usec: None,
            cpu_peak: None,
            peak_swap_bytes,
        });
        let Some(JobEnd::Exited { output, .. }) = end_of(&job, &result) else {
            panic!("a clean exit ends exited");
        };
        output.usage.expect("a usage").peak_swap_bytes
    };
    assert_eq!(stored(Some(768 << 20)), Some(768 << 20));
    assert_eq!(stored(Some(0)), Some(0));
    assert_eq!(stored(None), None);
}

/// A result that carries a CPU series ends the job with the series stored in its output as
/// reported (its granularity, points, run length and partial flag), whatever the way it ended and
/// whether or not the result reports a usage; a result without one stores none; and a series
/// whose granularity does not fit 16 bits (which the job rows cannot hold) is dropped and the
/// rest of the result still stored. The total CPU of the series is not compared with the usage.
/// It catches a series dropped on the way to the stored result, one tied to the usage, one
/// changed or checked against the usage, and a series the store cannot keep stored.
#[test]
fn the_cpu_series_of_a_result_is_stored_as_reported_and_none_when_not() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let wire = WireSeries {
        granularity_secs: 4,
        points: vec![0, 16, 40],
        run_usec: 9_000_000,
        partial: true,
    };
    let stored = CpuSeries::new(4, vec![0, 16, 40], 9_000_000, true);
    let output_of = |result: &JobResult| match end_of(&job, result) {
        Some(JobEnd::Exited { output, .. } | JobEnd::InputFailed { output, .. }) => output,
        other => panic!("an exit or an input failure, not {other:?}"),
    };
    let mut reported = ended(Outcome::Succeeded, Some(0));
    reported.cpu_series = Some(wire.clone());
    assert_eq!(output_of(&reported).cpu_series, stored);
    assert_eq!(output_of(&reported).usage, None);

    // A usage whose CPU total is far from the series' is stored with the series untouched.
    reported.usage = Some(JobUsage {
        peak_memory_bytes: 1 << 20,
        cpu_usage_usec: 1,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak: None,
        peak_swap_bytes: None,
    });
    assert_eq!(output_of(&reported).cpu_series, stored);
    assert!(output_of(&reported).usage.is_some());

    let mut failed = ended(Outcome::Failed, Some(2));
    failed.cpu_series = Some(wire.clone());
    assert_eq!(output_of(&failed).cpu_series, stored);
    let mut refused_input = ended(Outcome::Succeeded, Some(0));
    refused_input.cpu_series = Some(wire.clone());
    refused_input.input_failure = Some("input-unreadable".to_owned());
    assert_eq!(output_of(&refused_input).cpu_series, stored);

    assert_eq!(
        output_of(&ended(Outcome::Succeeded, Some(0))).cpu_series,
        None
    );

    let mut wide = ended(Outcome::Succeeded, Some(0));
    wide.cpu_series = Some(WireSeries {
        granularity_secs: 1 << 20,
        ..wire
    });
    let output = output_of(&wide);
    assert_eq!(
        (output.cpu_series, output.stdout),
        (None, "done\n".to_owned())
    );
}

/// A result that carries a memory series ends the job with it stored in its output as reported
/// (its granularity, points, run length and partial flag), beside a usage and with none; a
/// result without one stores none; and a series whose granularity does not fit 16 bits (which
/// the job rows cannot hold) or is not a power of two is dropped, the rest of the result still
/// stored. It catches a series dropped on the way to the stored result, one tied to the usage,
/// and one the store cannot keep stored.
#[test]
fn the_memory_series_of_a_result_is_stored_as_reported_and_none_when_not() {
    let (_store, jobs) = tables();
    let (job, _) = placed(&jobs, "job-a", "host-a", spec(), BOOKED);
    let wire = WireMemorySeries {
        granularity_secs: 4,
        points: vec![0, 300, 70_000],
        run_usec: 9_000_000,
        partial: true,
    };
    let stored = MemorySeries::new(4, vec![0, 300, 70_000], 9_000_000, true);
    let output_of = |result: &JobResult| match end_of(&job, result) {
        Some(JobEnd::Exited { output, .. }) => output,
        other => panic!("an exit, not {other:?}"),
    };
    let mut reported = ended(Outcome::Succeeded, Some(0));
    reported.memory_series = Some(wire.clone());
    assert_eq!(output_of(&reported).memory_series, stored);
    assert_eq!(output_of(&reported).usage, None);

    reported.usage = Some(JobUsage {
        peak_memory_bytes: 1 << 20,
        cpu_usage_usec: 1,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak: None,
        peak_swap_bytes: None,
    });
    assert_eq!(output_of(&reported).memory_series, stored);
    assert!(output_of(&reported).usage.is_some());
    assert_eq!(
        output_of(&ended(Outcome::Succeeded, Some(0))).memory_series,
        None
    );

    for granularity_secs in [1 << 20, 3] {
        let mut bad = ended(Outcome::Succeeded, Some(0));
        bad.memory_series = Some(WireMemorySeries {
            granularity_secs,
            ..wire.clone()
        });
        let output = output_of(&bad);
        assert_eq!(
            (output.memory_series, output.stdout),
            (None, "done\n".to_owned())
        );
    }
}
