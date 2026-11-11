//! The per-job half of a heartbeat on the job tables alone: what the route tests cannot reach
//! (a retired id that is no job id, listed states that move nothing, a failed end commit, a
//! start the wire cannot carry, the room for commands).

use std::sync::Mutex;
use std::time::Duration;

use js_agent_wire::{HeartbeatReply, HeldJob, HeldState, JobResult, NetworkCeiling, Outcome};
use js_jobs::{
    Booked, JobEnd, JobSpec, JobState, JobStoreError, Needs, Output, ResultReason, StartFailure,
};
use js_state::StoreError;
use js_types::{HostId, Resources};

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
            metrics: &NoMetrics,
            layered_rootfs: false,
        },
        &[],
        std::slice::from_ref(&long),
        8,
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
            metrics: &NoMetrics,
            layered_rootfs: false,
        },
        &listing,
        &[],
        8,
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
            metrics: &NoMetrics,
            layered_rootfs: false,
        },
        &listing,
        &[],
        8,
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
            metrics: &NoMetrics,
            layered_rootfs: false,
        },
        &[],
        &[],
        1,
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
            metrics: &NoMetrics,
            layered_rootfs: false,
        },
        &[],
        &[],
        8,
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
        metrics: &NoMetrics,
        layered_rootfs: false,
    };
    let beat = |listed: &[HeldJob]| serve(&jobs, &context, listed, &[], 8).unwrap();

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
        metrics,
        layered_rootfs: false,
    };
    let listing = [listed("job-a", HeldState::ExitedUnacked, Some(result))];
    let reply = serve(jobs, &context, &listing, &[], 8).unwrap();
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
