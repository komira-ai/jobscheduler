//! The per-job half of a heartbeat on the job tables alone: what the route tests cannot reach
//! (a retired id that is no job id, listed states that move nothing, a failed end commit, a
//! start the wire cannot carry, the room for commands).

use js_agent_wire::{HeartbeatReply, HeldJob, HeldState, JobResult, NetworkCeiling, Outcome};
use js_jobs::{Booked, JobEnd, JobSpec, JobState, JobStoreError, Needs};
use js_state::StoreError;
use js_types::{HostId, Resources};

use js_state::Store;

use super::{BeatContext, Command, serve};
use crate::agent_api::job_rig::{BOOKED, placed, result, spec, starts, stops, tables};
use crate::metrics::NoMetrics;

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
