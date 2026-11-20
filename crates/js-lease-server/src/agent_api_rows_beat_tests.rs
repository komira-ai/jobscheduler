//! What a heartbeat does with the host's row and the listing it brings, over servers that share
//! a store: a listing a newer beat overtook, the beats of an agent that sends no sequence, the
//! first listing and the clean flag as the row's, and the job pass that reads the flag.

use std::sync::Mutex;

use js_agent_wire::{Ack, HeldState, Outcome};
use js_jobs::{AllocationStep, JobState, NewJob};

use super::*;
use crate::agent_api::job_rig::{self, BOOKED, FakeAgent, placed, rig, spec};

/// Asserts that of two beats of one host that run at once only one listing commits. The first
/// beat's listing (the result of one job) is about to commit when a newer beat of the host, on
/// another server, runs to completion: it applies its own listing (the result of another job).
/// The first beat's guarded write then finds its row moved, writes nothing, and the beat is
/// answered from the rows with no verdict on its results (which the agent reports again); the
/// newer beat's result is recorded once and accepted in its own reply, and neither job's
/// allocation is freed. It catches a listing that commits though a newer beat was applied (both
/// results recorded), a beat refused or answered 500 when a newer beat overtook it, and an old
/// listing that acknowledges a result it did not commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_a_newer_beat_overtook_is_answered_from_the_rows_with_no_acks() {
    let world = World::new();
    let first = world.server("rows_moved_first", Kind::JOBS);
    let second = world.server("rows_moved_second", Kind::JOBS);
    let session = first.join("host-a").await;
    let (one, _) = placed(&world.jobs, "job-1", "host-a", spec(), BOOKED);
    let (two, _) = placed(&world.jobs, "job-2", "host-a", spec(), BOOKED);
    let state = |id| world.jobs.get_job(id).unwrap().unwrap().state;
    let mut agent = FakeAgent::new("host-a");
    agent.take(
        &first
            .beat(&session, Some(1), agent.listing())
            .await
            .unwrap(),
    );
    agent.run("job-1");
    agent.run("job-2");
    first
        .beat(&session, Some(2), agent.listing())
        .await
        .unwrap();
    assert_eq!(
        (state(&one.id), state(&two.id)),
        (JobState::Running, JobState::Running)
    );

    // The listing the first beat brings has job-1 ended; the newer beat's has job-2 ended.
    let mut older = FakeAgent::new("host-a");
    older.hold("job-2", &world.alloc("job-2"), HeldState::Running);
    older.hold("job-1", &world.alloc("job-1"), HeldState::Running);
    older.end("job-1", Outcome::Succeeded, Some(0));
    agent.end("job-2", Outcome::Succeeded, Some(0));
    // Just before the first beat's listing commits (the commit after its own row write), the
    // second server serves the newer beat of the host to the end.
    let answered = std::sync::Arc::new(Mutex::new(None));
    let newer = beat_body(&session, Some(4), agent.listing(), json!({}));
    let (agents, who) = (Arc::clone(&second.agents), second.who.clone());
    let (store, sink) = (world.store.clone(), answered.clone());
    let runtime = tokio::runtime::Handle::current();
    world.store.before_next(move || {
        store.before_next(move || {
            let reply = runtime.block_on(post_beat(agents, who, newer));
            *sink.lock().unwrap() = Some(reply.map_err(|refused| said(&refused)));
        });
    });
    let commits = world.commits();
    let reply = first
        .beat(&session, Some(3), older.listing())
        .await
        .unwrap();

    assert!(reply.job_acks.is_empty(), "{:?}", reply.job_acks);
    let newer = answered.lock().unwrap().take().unwrap().unwrap();
    let verdicts: Vec<_> = newer
        .job_acks
        .iter()
        .map(|ack| (ack.job_id.as_str(), ack.ack))
        .collect();
    assert_eq!(verdicts, [("job-2", Ack::Accepted)]);
    assert_eq!(
        state(&one.id),
        JobState::Running,
        "the overtaken listing wrote nothing"
    );
    assert_eq!(state(&two.id), JobState::Succeeded);
    for id in [&one.id, &two.id] {
        let step = world.jobs.get_allocation(id).unwrap().unwrap().step;
        assert_ne!(step, AllocationStep::Freed);
    }
    // Both beats' row writes, the newer listing's one commit, and the newer beat's settle.
    assert_eq!(world.commits(), commits + 4);
    let row = world.row("host-a").record;
    assert_eq!(row.last_beat.unwrap().seq, Some(4));
    assert!(!row.state.first_listing_pending && row.state.last_listing_clean);
}

/// Asserts what a container the agent no longer lists costs when the agent sends no beat
/// sequence (the server cannot tell an old beat from a new one): it is freed only once two
/// applied beats in a row omit it. Omitted once, it is kept and counted in the host's row;
/// listed again, the count starts over; omitted once more, kept; omitted again, freed. It
/// catches a container freed by a single stale beat that omits it, and a count that is not
/// reset by a beat that lists the container.
#[tokio::test]
async fn an_agent_with_no_sequence_frees_a_container_after_two_beats_that_omit_it() {
    let mut rig = rig("rows_unsequenced", true).unsequenced();
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let job = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-a");
    rig.beat(&agent).await.unwrap();
    agent.end("job-a", Outcome::Succeeded, Some(0));
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply.job_acks[0].ack, Ack::Accepted);
    let allocation = rig.allocation(&job).allocation_id.to_string();
    let releasing = (JobState::Succeeded, AllocationStep::Releasing);
    let missing = |rig: &job_rig::Rig| {
        let row = rig.jobs.read_node(&id("host-a")).unwrap().unwrap();
        row.record.state.missing_in_a_row
    };

    let nothing = FakeAgent::new("host-a");
    rig.beat(&nothing).await.unwrap();
    assert_eq!(rig.states(&job), releasing, "omitted once: kept");
    assert_eq!(missing(&rig), BTreeMap::from([(job.clone(), 1)]));

    agent.hold("job-a", &allocation, HeldState::Running);
    rig.beat(&agent).await.unwrap();
    assert_eq!(
        missing(&rig),
        BTreeMap::new(),
        "listed again: the count starts over"
    );
    rig.beat(&nothing).await.unwrap();
    assert_eq!(rig.states(&job), releasing, "omitted once since: kept");

    rig.beat(&nothing).await.unwrap();
    assert_eq!(
        rig.states(&job),
        (JobState::Succeeded, AllocationStep::Freed),
        "omitted by two beats in a row: freed"
    );
    assert_eq!(missing(&rig), BTreeMap::new());
}

/// Asserts that an agent that sends a sequence has its container freed at the first beat that
/// omits it, and the row counts nothing. It catches a count kept for an agent whose beats
/// are ordered, which would hold every container for one beat more.
#[tokio::test]
async fn an_agent_with_a_sequence_frees_a_container_at_the_first_beat_that_omits_it() {
    let mut rig = rig("rows_sequenced", true);
    rig.register("host-a", true);
    let mut agent = FakeAgent::new("host-a");
    let job = rig.place("job-a", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-a");
    rig.beat(&agent).await.unwrap();
    agent.end("job-a", Outcome::Succeeded, Some(0));
    rig.beat(&agent).await.unwrap();

    rig.beat(&FakeAgent::new("host-a")).await.unwrap();
    assert_eq!(
        rig.states(&job),
        (JobState::Succeeded, AllocationStep::Freed)
    );
    let row = rig.jobs.read_node(&id("host-a")).unwrap().unwrap();
    assert!(row.record.state.missing_in_a_row.is_empty());
}

/// Asserts that the first listing since a hello is the row's, whichever server takes it. A job
/// the host runs stays running when a later beat on another server omits it (that listing is
/// not the first, though that server never saw one); after a new hello (on the second server,
/// the host having been silent) the first beat, on the first server, loses the job the host
/// does not list. It catches a first-listing flag kept in each server's memory, which loses a
/// running job at every beat that moves to another server and forgives the one a hello made
/// the first listing for.
#[tokio::test]
async fn the_first_listing_is_the_rows_whichever_server_takes_it() {
    let world = World::new();
    let first = world.server("rows_first_a", Kind::JOBS);
    let second = world.server("rows_first_b", Kind::JOBS);
    let session = first.join("host-a").await;
    let (record, _) = placed(&world.jobs, "job-1", "host-a", spec(), BOOKED);
    let state = || world.jobs.get_job(&record.id).unwrap().unwrap().state;
    let mut agent = FakeAgent::new("host-a");
    agent.take(
        &first
            .beat(&session, Some(1), agent.listing())
            .await
            .unwrap(),
    );
    agent.run("job-1");
    first
        .beat(&session, Some(2), agent.listing())
        .await
        .unwrap();
    assert_eq!(state(), JobState::Running);

    second.beat(&session, Some(3), Vec::new()).await.unwrap();
    assert_eq!(
        state(),
        JobState::Running,
        "not the first listing since the hello"
    );

    world.advance(Duration::from_secs(3600));
    let again = second.join("host-a").await;
    assert_eq!(again.epoch, session.epoch + 1);
    assert!(world.row("host-a").record.state.first_listing_pending);
    first.beat(&again, Some(1), Vec::new()).await.unwrap();
    assert_eq!(
        state(),
        JobState::Lost,
        "the first listing since the new hello"
    );
    assert!(!world.row("host-a").record.state.first_listing_pending);
}

/// The queued job `job-q`, which `world` holds.
fn queue(world: &World) -> js_jobs::JobId {
    let new = NewJob {
        id: Some(job("job-q")),
        spec: spec(),
        priority: 0,
        client: None,
    };
    world.jobs.insert_queued(new).unwrap().record.id
}

/// Asserts that the job pass reads each host's clean flag from the rows. A host that lists a
/// job the store does not know, to the first server, takes no job there; its next beat, which
/// lists nothing unknown and reaches the second server (which places nothing), makes it clean,
/// and the first server's next pass then places the job on it, with no beat of its own in
/// between. A pass that cannot read the rows, or the queue, places nothing, and places on the next. It
/// catches a flag kept in each server's memory (the first server would keep the host unclean
/// for ever), a pass that places on a host whose flag it could not read, and a pass that treats
/// an unread row as clean.
#[tokio::test]
async fn the_pass_places_on_a_host_whose_row_says_clean_whichever_server_wrote_it() {
    let world = World::new();
    let first = world.server("rows_pass_first", Kind::PLACING);
    let second = world.server("rows_pass_second", Kind::JOBS);
    let session = first.join("host-a").await;
    let queued = queue(&world);
    let state = || world.jobs.get_job(&queued).unwrap().unwrap().state;
    let ghost = HeldJob {
        job_id: "ghost".to_owned(),
        state: HeldState::Running,
        result: None,
    };

    first.beat(&session, Some(1), vec![ghost]).await.unwrap();
    assert!(!world.row("host-a").record.state.last_listing_clean);
    first.agents.place_jobs();
    assert_eq!(state(), JobState::Queued, "an unclean host takes no job");

    second.beat(&session, Some(2), Vec::new()).await.unwrap();
    assert!(world.row("host-a").record.state.last_listing_clean);
    assert_eq!(
        state(),
        JobState::Queued,
        "the second server places nothing"
    );

    world.store.fail_read_after(0);
    assert!(first.agents.place_jobs().is_empty());
    assert_eq!(
        state(),
        JobState::Queued,
        "a pass that cannot read the rows places nothing"
    );

    // The rows are read; the queue after them is not: the job stays queued for the next pass.
    world.store.fail_read_after(1);
    first.agents.place_jobs();
    assert_eq!(state(), JobState::Queued);

    first.agents.place_jobs();
    assert_eq!(
        state(),
        JobState::Placed,
        "the first server reads the flag the second wrote"
    );
}
