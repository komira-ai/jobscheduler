//! The per-job heartbeat against a fake agent, scenario by scenario (design section 5.3 and
//! section 8, rows 37 and 38): starts resent until listed, unknown and ended jobs told to
//! stop, a cancel answered by a retired id, a late start after a cancel.

use std::collections::BTreeMap;

use js_agent_wire::{Ack, BookedResources, HeldState, JobStartSpec, Outcome};
use js_jobs::{AllocationStep, EndReason, JobEnd, JobId, JobState};

use super::job_rig::{FakeAgent, image, rig, starts, stops};
use super::*;

/// A placed job's start is in every reply, the same each time, with the allocation's booked
/// resources, the job's wall limit and the allocation id, until the host lists the job; then
/// it is not sent again. It catches a start sent once only, one resent after the host listed
/// the job, and one that carries other resources than the allocation booked.
#[tokio::test]
async fn a_start_is_resent_until_listed() {
    let mut rig = rig("start_resent", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    let expected = JobStartSpec {
        job_id: "job-a".to_owned(),
        allocation_id: rig.allocation(&id).allocation_id.to_string(),
        image: image(),
        command: vec!["run".to_owned(), "--fast".to_owned()],
        run: None,
        os: None,
        arch: None,
        env: BTreeMap::new(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1500,
            memory_bytes: 1 << 30,
            disk_bytes: Some(2 << 30),
            gpu_indices: Vec::new(),
        },
        wall_ms: 600_000,
        network: Default::default(),
        ports: Vec::new(),
    };
    let mut last = None;
    for _ in 0..3 {
        let reply = rig.beat(&agent).await.unwrap();
        assert_eq!(starts(&reply), std::slice::from_ref(&expected));
        assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
        last = Some(reply);
    }
    agent.take(&last.unwrap());
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    assert_eq!(
        rig.states(&id),
        (JobState::Starting, AllocationStep::Booked)
    );
    assert!(starts(&rig.beat(&agent).await.unwrap()).is_empty());
}

/// A host that lists a job id the store never made, an id that is no job id, and a job that
/// was lost (its allocation freed) is told to stop each of them; the store gains no job and no
/// allocation, the lost job stays lost and its allocation freed, and the host's listing is not
/// clean until it lists none of them. It catches a server that accepts such a listing, makes a
/// row for it, revives the lost job, or says nothing.
#[tokio::test]
async fn a_listed_unknown_job_is_told_to_stop() {
    let mut rig = rig("unknown_listed", true);
    rig.register("host-a", true);
    let host = HostId::new("host-a").unwrap();
    let mut agent = FakeAgent::new("host-a");
    let lost = rig.place("job-lost", "host-a");
    let allocation = rig.allocation(&lost).allocation_id.to_string();
    rig.jobs
        .end_and_free(&lost, JobEnd::Lost, EndReason::HostAbsent)
        .unwrap();
    let long = "x".repeat(70);
    agent.hold("ghost", &allocation, HeldState::Running);
    agent.hold("job-lost", &allocation, HeldState::Running);
    agent.hold(&long, &allocation, HeldState::Starting);
    let before = rig.commits();

    let reply = rig.beat(&agent).await.unwrap();
    let mut told = vec!["ghost".to_owned(), "job-lost".to_owned(), long.clone()];
    told.sort();
    assert_eq!(stops(&reply), told);
    assert!(starts(&reply).is_empty());
    assert_eq!(rig.commits(), before);
    assert!(!rig.agents.listing_clean(&host));
    let ghost = JobId::parse("ghost").unwrap();
    assert_eq!(rig.jobs.get_job(&ghost).unwrap(), None);
    assert_eq!(rig.jobs.get_allocation(&ghost).unwrap(), None);
    assert_eq!(rig.states(&lost), (JobState::Lost, AllocationStep::Freed));
    assert!(rig.jobs.live_on(&host).unwrap().is_empty());

    // The agent stops them and lists their results: each is acknowledged stale, nothing
    // moves, and once it lists none of them its listing is clean again.
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    let verdicts: Vec<_> = reply.job_acks.iter().map(|ack| ack.ack).collect();
    assert_eq!(verdicts, [Ack::Stale; 3]);
    assert_eq!(rig.states(&lost), (JobState::Lost, AllocationStep::Freed));
    agent.take(&reply);
    assert!(agent.listing().is_empty());
    rig.beat(&agent).await.unwrap();
    assert!(rig.agents.listing_clean(&host));
    assert_eq!(rig.commits(), before);
}

/// A running job is cancelled, ends cancelled and its allocation is freed; when the host
/// lists it again as running (it kept the container across a partition), the job stays
/// cancelled, the host is told to stop it and the allocation stays freed under the same id. It
/// catches a late listing that moves the job's state or books it again.
#[tokio::test]
async fn a_cancelled_job_is_never_revived_by_a_late_listing() {
    let mut rig = rig("never_revived", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-a");
    rig.beat(&agent).await.unwrap();
    assert_eq!(rig.states(&id), (JobState::Running, AllocationStep::Booked));
    rig.jobs.request_cancel(&id).unwrap();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["job-a"]);
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Releasing)
    );
    agent.take(&reply);
    rig.beat(&agent).await.unwrap();
    let freed = rig.allocation(&id);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::Cancelled))
    );

    agent.hold(
        "job-a",
        &freed.allocation_id.to_string(),
        HeldState::Running,
    );
    let before = rig.commits();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["job-a"]);
    assert!(starts(&reply).is_empty());
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    assert_eq!(rig.allocation(&id), freed);
    assert_eq!(rig.commits(), before);
}

/// A placed job is cancelled before the host listed it; the host answers the stop with the
/// retired id, and the job becomes cancelled and its allocation freed in exactly one commit. A
/// failure injected at that commit leaves both as they were (and the answer is a 500), and
/// the next heartbeat applies it once. It catches the two changes made as two commits, and
/// one applied without the other.
#[tokio::test]
async fn retired_id_answer_cancels_and_frees_in_one_commit() {
    let mut rig = rig("retired_one_commit", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    rig.jobs.request_cancel(&id).unwrap();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(
        (stops(&reply), starts(&reply)),
        (vec!["job-a".to_owned()], vec![])
    );
    agent.take(&reply);
    assert!(!agent.holds("job-a"));

    rig.store.fail_next();
    let refused = rig.beat(&agent).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (500, "state-write-failed")
    );
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));

    let before = rig.commits();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 1);
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    assert_eq!(rig.allocation(&id).end_reason, Some(EndReason::Cancelled));
    assert_eq!(reply.retired_acks, ["job-a"]);
    assert!(reply.commands.is_empty());
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.commits(), before + 1);
    assert!(reply.retired_acks.is_empty());
}

/// A job's start reaches the agent, then the job is cancelled: no reply from then on carries
/// its start, the agent stops it before it made a container, and a copy of the start that
/// arrives after the stop is not run. The reply that carries the stop also starts another job,
/// so a start and a stop travel together. It catches a start resent after the cancel intent
/// is committed.
#[tokio::test]
async fn a_late_start_after_a_cancel_does_not_run() {
    let mut rig = rig("late_start", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let id = rig.place("job-a", "host-a");
    let first = rig.beat(&agent).await.unwrap();
    assert_eq!(starts(&first).len(), 1);
    agent.take(&first);
    rig.jobs.request_cancel(&id).unwrap();
    rig.place("job-b", "host-a");

    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(stops(&reply), ["job-a"]);
    let started: Vec<_> = starts(&reply).into_iter().map(|spec| spec.job_id).collect();
    assert_eq!(started, ["job-b"]);
    agent.take(&reply);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(rig.job(&id).state, JobState::Cancelled);
    assert!(starts(&reply).is_empty());
    agent.take(&reply);
    agent.take(&first);
    assert!(!agent.holds("job-a"));
    let reply = rig.beat(&agent).await.unwrap();
    assert!(starts(&reply).is_empty());
    assert_eq!(
        rig.states(&id),
        (JobState::Cancelled, AllocationStep::Freed)
    );
    assert!(agent.launched.is_empty());
    agent.run("job-b");
    agent.end("job-b", Outcome::Succeeded, Some(0));
    agent.take(&rig.beat(&agent).await.unwrap());
    assert_eq!(agent.launched, ["job-b"]);
}
