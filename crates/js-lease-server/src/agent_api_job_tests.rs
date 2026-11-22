//! The heartbeat route's per-job half: stops resent, listed states committed with their
//! allocation change, frees only on a host's confirmation, a failed commit answered 500, and
//! the reply of an agent that speaks no per-job message unchanged.

use js_agent_wire::{Ack, HeldState, Outcome};
use js_jobs::{AllocationStep, EndReason, JobId, JobState, NewJob, ResultReason, StartFailure};
use js_proto::v1;
use js_types::{HostId, JobEventKind};

use super::job_rig::{FakeAgent, clocked_rig, rig, spec, starts, stops};
use crate::job_expiry::JobLimits;
use crate::per_job::NO_EXIT_CODE;

/// A running job with a cancel intent is told to stop in every reply while its host lists it
/// running, and no more once the host lists it stopped. It catches a stop sent once only.
#[tokio::test]
async fn a_stop_is_resent_until_the_host_stops_listing_the_job() {
    let mut rig = rig("stop_resent", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-a");
    rig.beat(&agent).await.unwrap();
    rig.jobs.request_cancel(&id).unwrap();
    let mut last = None;
    for _ in 0..3 {
        let reply = rig.beat(&agent).await.unwrap();
        assert_eq!(stops(&reply), ["job-a"]);
        assert_eq!(rig.states(&id), (JobState::Running, AllocationStep::Booked));
        last = Some(reply);
    }
    agent.take(&last.unwrap());
    let reply = rig.beat(&agent).await.unwrap();
    assert!(stops(&reply).is_empty());
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Releasing)
    );
}

/// Each listed state moves the job once, with the allocation change that goes with it:
/// starting, then running (which stamps the start), then the end, which leaves the allocation
/// releasing and is acknowledged; a listing repeated commits nothing and a repeated result is
/// a duplicate. A job the agent ran and ended between two heartbeats is walked through the
/// steps it skipped. It catches a state that moves without its allocation, and a repeated
/// listing that writes.
#[tokio::test]
async fn listed_states_commit_with_the_allocation_change() {
    let mut rig = rig("states_commit", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());

    // Every beat commits two rows of its own (beat_node, settle_node) besides what its
    // listing changes: +3 is one listed state, then +2 per beat that changes nothing.
    let before = rig.commits();
    rig.beat(&agent).await.unwrap();
    // The listed state's commit plus the beat's two row writes (beat_node, settle_node).
    assert_eq!(rig.commits(), before + 3);
    assert_eq!(
        rig.states(&id),
        (JobState::Starting, AllocationStep::Booked)
    );
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 5);

    agent.run("job-a");
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 8);
    assert_eq!(rig.states(&id), (JobState::Running, AllocationStep::Booked));
    assert!(rig.job(&id).started_at.is_some());
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 10);

    agent.end("job-a", Outcome::Succeeded, Some(0));
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 13);
    assert_eq!(
        rig.states(&id),
        (JobState::Succeeded, AllocationStep::Releasing)
    );
    let result = rig.job(&id).result.unwrap();
    assert_eq!(
        (result.exit_code, result.reason, result.stdout_tail.as_str()),
        (Some(0), ResultReason::Exit, "done\n")
    );
    assert_eq!(reply.job_acks[0].ack, Ack::Accepted);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 15);
    assert_eq!(reply.job_acks[0].ack, Ack::Duplicate);

    // Placed, then listed already ended: starting and running are committed first, each
    // once, then the end.
    let quick = rig.place("job-quick", "host-a");
    let allocation = rig.allocation(&quick).allocation_id.to_string();
    agent.hold("job-quick", &allocation, HeldState::ExitedUnacked);
    agent.end("job-quick", Outcome::Failed, Some(3));
    let before = rig.commits();
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 5);
    let job = rig.job(&quick);
    assert_eq!(job.state, JobState::Failed);
    assert!(job.started_at.is_some());
    assert_eq!(job.result.unwrap().exit_code, Some(3));
    assert_eq!(rig.allocation(&quick).step, AllocationStep::Releasing);
}

/// Each outcome an agent lists is recorded as it sent it: a wall-limit stop and an
/// out-of-memory stop fail the job with their reason, a stop nobody asked for and a failure
/// with no status fail it with [`NO_EXIT_CODE`], a refused start fails it to start; an outcome
/// this version does not know is left unanswered and changes nothing; an end the job's state
/// cannot take (a refused start of a running job) is answered stale and changes nothing. A
/// start refused with `input-unavailable` fails to start as the host's input, and with another
/// code as the agent's, each job showing the code it was refused with; a clean exit that
/// names an input failure fails the job with the reason input failed and the exit status kept.
#[tokio::test]
async fn every_listed_outcome_is_recorded_as_sent() {
    let mut rig = rig("outcomes", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let running = rig.place("running", "host-a");
    let allocation = rig.allocation(&running).allocation_id.to_string();
    agent.hold("running", &allocation, HeldState::Running);
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.job(&running).state, JobState::Running);
    agent.end("running", Outcome::FailedToStart, None);
    let cases = [
        ("wall", Outcome::TimedOut, Some(143)),
        ("memory", Outcome::OutOfMemory, None),
        ("killed", Outcome::Killed, Some(0)),
        ("nostatus", Outcome::Failed, None),
        ("refused", Outcome::FailedToStart, None),
        ("unknown", Outcome::Unknown, None),
        ("unavailable", Outcome::FailedToStart, None),
        ("notallowed", Outcome::FailedToStart, None),
        ("inputfailed", Outcome::Succeeded, Some(0)),
    ];
    for (name, outcome, code) in cases {
        let id = rig.place(name, "host-a");
        let allocation = rig.allocation(&id).allocation_id.to_string();
        agent.hold(name, &allocation, HeldState::ExitedUnacked);
        let (detail, input_failure) = match name {
            "unavailable" => (Some("input-unavailable"), None),
            "notallowed" => (Some("input-not-allowed"), None),
            "inputfailed" => (None, Some("input-unreadable")),
            _ => (None, None),
        };
        agent.end_naming(name, (outcome, code), (detail, input_failure));
    }

    let reply = rig.beat(&agent).await.unwrap();
    let verdict = |name: &str| {
        let ack = reply.job_acks.iter().find(|ack| ack.job_id == name);
        ack.map(|ack| ack.ack)
    };
    let ended = |name: &str| {
        let job = rig.job(&JobId::parse(name).unwrap());
        let result = job.result.map(|result| (result.reason, result.exit_code));
        (job.state, result)
    };
    assert_eq!(
        ended("wall"),
        (JobState::Failed, Some((ResultReason::WallLimit, Some(143))))
    );
    assert_eq!(
        ended("memory"),
        (JobState::Failed, Some((ResultReason::Memory, None)))
    );
    let failed = Some((ResultReason::Exit, Some(NO_EXIT_CODE)));
    assert_eq!(ended("killed"), (JobState::Failed, failed));
    assert_eq!(ended("nostatus"), (JobState::Failed, failed));
    assert_eq!(
        ended("refused"),
        (JobState::FailedToStart(StartFailure::AgentRefused), None)
    );
    assert_eq!(
        ended("unavailable"),
        (
            JobState::FailedToStart(StartFailure::InputUnavailable),
            None
        )
    );
    assert_eq!(
        ended("notallowed"),
        (JobState::FailedToStart(StartFailure::AgentRefused), None)
    );
    assert_eq!(
        ended("inputfailed"),
        (JobState::Failed, Some((ResultReason::InputFailed, Some(0))))
    );
    let shown = |name: &str| {
        let job = rig.job(&JobId::parse(name).unwrap());
        let shown = crate::job_api::read::view::job(&job);
        (shown.start_failure(), shown.start_failure_detail)
    };
    assert_eq!(
        shown("unavailable"),
        (
            v1::StartFailureCode::InputUnavailable,
            "input-unavailable".to_owned()
        )
    );
    assert_eq!(
        shown("notallowed"),
        (
            v1::StartFailureCode::AgentRefused,
            "input-not-allowed".to_owned()
        )
    );
    assert_eq!(shown("refused").1, "");
    assert_eq!(ended("unknown"), (JobState::Placed, None));
    assert_eq!(verdict("unknown"), None);
    assert_eq!(ended("running"), (JobState::Running, None));
    assert_eq!(verdict("running"), Some(Ack::Stale));
    for name in [
        "wall",
        "memory",
        "killed",
        "nostatus",
        "refused",
        "unavailable",
        "notallowed",
        "inputfailed",
    ] {
        assert_eq!(verdict(name), Some(Ack::Accepted), "{name}");
    }
}

/// A releasing allocation stays releasing across heartbeats of other hosts (its own host is
/// silent) and across one of its own host that still lists the job, and is freed, with the
/// job's end as its reason, only on a heartbeat of its host that no longer lists the job. It
/// catches a free on the absence of a report.
#[tokio::test]
async fn container_gone_frees_the_allocation_and_silence_does_not() {
    let mut rig = rig("container_gone", true);
    rig.register("host-a", true);
    rig.register("host-b", true);
    let mut agent = FakeAgent::new("host-a");
    let other = FakeAgent::new("host-b");
    let id = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-a");
    agent.end("job-a", Outcome::TimedOut, None);
    let ended = rig.beat(&agent).await.unwrap();
    assert_eq!(
        rig.states(&id),
        (JobState::Failed, AllocationStep::Releasing)
    );
    for _ in 0..3 {
        rig.beat(&other).await.unwrap();
        assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);
    }
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);
    agent.take(&ended);
    rig.beat(&agent).await.unwrap();
    let freed = rig.allocation(&id);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::WallLimit))
    );
}

/// A store failure at the commit of a listed state answers 500 `state-write-failed` with no
/// reply at all (so no start or stop), leaves the job as it was, and the next heartbeat
/// applies the state once. It catches a reply sent over a commit that did not happen.
#[tokio::test]
async fn a_failed_commit_answers_500_and_sends_nothing() {
    let mut rig = rig("failed_commit", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    // The beat's own row write fails first: 503, nothing of the job changes.
    rig.store.fail_next();
    let refused = rig.beat(&agent).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (503, "state-write-failed")
    );
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    // Let the beat's row write through; the listing's commit fails: 500.
    rig.store.fail_after(1);
    let refused = rig.beat(&agent).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (500, "state-write-failed")
    );
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    let before = rig.commits();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 3);
    assert_eq!(
        rig.states(&id),
        (JobState::Starting, AllocationStep::Booked)
    );
    assert!(reply.commands.is_empty());
}

/// The reply to a heartbeat with no job listing is today's, byte for byte: from a server
/// without the job tables, from one with them to a host whose agent does not report the
/// `per-job` capability (and has a job placed on it, which it is not sent), and from one
/// with them to a host that does report it and has nothing. It catches a reply that gains a
/// field, or a start sent to an agent that would read it as unknown.
#[tokio::test]
async fn an_agent_without_job_listing_gets_todays_reply() {
    let today = r#"{"commands":[],"result_acks":[],"taint_ack":false}"#;
    let mut plain = rig("today_plain", false);
    plain.register("host-a", false);
    let reply = plain.beat(&FakeAgent::new("host-a")).await.unwrap();
    assert_eq!(serde_json::to_string(&reply).unwrap(), today);

    let mut rig = rig("today_jobs", true);
    rig.register("host-a", false);
    rig.register("host-b", true);
    let id = rig.place("job-a", "host-a");
    let reply = rig.beat(&FakeAgent::new("host-a")).await.unwrap();
    assert_eq!(serde_json::to_string(&reply).unwrap(), today);
    assert!(starts(&reply).is_empty());
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    let reply = rig.beat(&FakeAgent::new("host-b")).await.unwrap();
    assert_eq!(serde_json::to_string(&reply).unwrap(), today);
}

/// A commit the store fails is a 500 `state-write-failed`, writes nothing, and the next
/// heartbeat, which the store answers, is served: the failure is not remembered. It catches
/// a failed commit that ends the server's serving of agents.
#[tokio::test]
async fn a_failed_listing_commit_is_a_500_and_the_next_heartbeat_is_served() {
    let mut rig = rig("failed_heartbeat", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    // The beat's own row write fails first: 503, nothing of the job changes.
    rig.store.fail_next();
    let refused = rig.beat(&agent).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (503, "state-write-failed")
    );
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    // Let the beat's row write through; the listing's commit fails: 500.
    rig.store.fail_after(1);
    let refused = rig.beat(&agent).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (500, "state-write-failed")
    );
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    assert!(rig.beat(&agent).await.is_ok());
}

/// Under `--placement js-sched`, a start a host refused with `no-capacity` is not the end of the
/// job: through the heartbeat route the job goes back to the queue with the refusing host in its
/// capacity refusals and a `REQUEUED` event naming it, is placed on the other host, which is
/// sent its start, and the job never reads failed to start. It catches a capacity refusal that
/// fails the build, and a requeued job placed again on the host that refused it.
#[tokio::test]
async fn a_start_refused_for_capacity_is_placed_on_another_host_and_started_there() {
    let mut rig = clocked_rig("capacity_requeue", true, JobLimits::default());
    rig.register("host-a", true);
    rig.register("host-b", true);
    let mut agents = [FakeAgent::new("host-a"), FakeAgent::new("host-b")];
    for agent in &agents {
        rig.beat(agent).await.unwrap();
    }
    let new = NewJob {
        id: Some(JobId::parse("job-c").unwrap()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    let id = rig.jobs.insert_queued(new).unwrap().record.id;
    let mut refusing = None;
    for round in 0..4 {
        for agent in &mut agents {
            let reply = rig.beat(agent).await.unwrap();
            agent.take(&reply);
            let state = rig.job(&id).state;
            assert!(
                !matches!(state, JobState::FailedToStart(_)),
                "round {round}: the job reads {state:?}"
            );
            if refusing.is_none() && agent.holds("job-c") {
                agent.end_naming(
                    "job-c",
                    (Outcome::FailedToStart, None),
                    (Some("no-capacity"), None),
                );
                refusing = Some(HostId::new(&agent.host).unwrap());
            }
        }
    }
    let refusing = refusing.expect("a host was sent the job's start");
    let job = rig.job(&id);
    assert_eq!(job.capacity_refusals, std::slice::from_ref(&refusing));
    assert_eq!(job.held_refusals, Vec::<HostId>::new());
    let events = job.queue.events();
    assert!(
        events
            .iter()
            .any(|e| e.kind == JobEventKind::Requeued && e.host.as_ref() == Some(&refusing)),
        "no REQUEUED event names {refusing}: {events:?}"
    );
    let other = rig.allocation(&id).host;
    assert_ne!(
        other, refusing,
        "the job was placed on the refusing host again"
    );
    let started = agents
        .iter()
        .find(|agent| agent.host == other.as_str())
        .unwrap();
    assert!(started.holds("job-c"), "{other} was not sent the start");
}
