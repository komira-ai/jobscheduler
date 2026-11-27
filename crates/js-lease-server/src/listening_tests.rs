//! What a notice of another server wakes here ([`super::heard_by`]), on the job rig's agent side.
//! Nothing waits on time: a poke is checked by polling the poller's wait once, and a wake by
//! polling a watch's once. The notices through a real listener are the dispatch poll's tests
//! (`agent_api/dispatch_notice_tests.rs`).

use js_jobs::{JobId, JobState, NewJob, Wake};
use js_state::postgres::Heard;
use js_state::{Notice, StoreError};
use js_types::HostId;

use super::{heard_by, start};
use crate::agent_api::job_rig::{Rig, clocked_rig, spec};
use crate::agent_api::peer_rig::NEVER;
use crate::held_beat::Watch;
use crate::poller::run;

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// Whether `watch` holds a wake already: its wait ends without yielding.
async fn woken_now(watch: &Watch) -> bool {
    tokio::select! {
        biased;
        () = watch.woken() => true,
        () = std::future::ready(()) => false,
    }
}

/// A placing rig with `host-a` registered, and the job `job-a` queued by another server: written
/// to the tables the rig shares, past this server's job service, so nothing here was poked.
fn queued_elsewhere(name: &str) -> (Rig, JobId) {
    let mut rig = clocked_rig(name, true, NEVER);
    rig.register("host-a", true);
    let id = JobId::parse("job-a").unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    (rig, id)
}

/// Asserts that a queue notice from another server pokes the placement poller, whose pass then
/// places the job that server queued, before any tick. It catches a job another server queued
/// that waits here for the next tick.
#[tokio::test]
async fn a_queue_notice_brings_the_pass_that_places_what_another_server_queued() {
    let (rig, id) = queued_elsewhere("listening_queue");
    heard_by(&rig.agents, &Heard::Notice(Wake::Queue.notice()));
    assert!(rig.agents.poller.take_poke(), "the notice pokes the poller");
    run(&rig.agents).await;
    assert_eq!(rig.job(&id).state, JobState::Placed);
}

/// Asserts that with no notice (a store one process writes, or a notice lost) nothing here is
/// poked by another server's queued job, and the tick's pass places it. It catches a server that
/// leaves such a job queued past the tick, and a poke that comes from nowhere.
#[tokio::test]
async fn without_a_notice_the_job_another_server_queued_is_placed_at_the_tick() {
    let (rig, id) = queued_elsewhere("listening_no_notice");
    assert!(!rig.agents.poller.take_poke());
    assert_eq!(rig.job(&id).state, JobState::Queued);
    rig.agents.place_jobs();
    assert_eq!(rig.job(&id).state, JobState::Placed);
}

/// Asserts that a host notice wakes the held heartbeat and the held dispatch poll of that host,
/// and nothing of another host, and pokes no pass. It catches a host notice that wakes only one
/// of the two waits, or wakes every host.
#[tokio::test]
async fn a_host_notice_wakes_the_held_beat_and_poll_of_that_host_alone() {
    let rig = clocked_rig("listening_host", true, NEVER);
    let agents = &rig.agents;
    let (beat, poll) = (
        agents.wakes.watch(&host("host-a")),
        agents.dispatching.wakes.watch(&host("host-a")),
    );
    let other = agents.dispatching.wakes.watch(&host("host-b"));
    heard_by(agents, &Heard::Notice(Wake::Host(host("host-a")).notice()));
    assert!(woken_now(&beat).await);
    assert!(woken_now(&poll).await);
    assert!(!woken_now(&other).await);
    assert!(!agents.poller.take_poke());
}

/// Asserts that the listening connection made (again) wakes every held heartbeat and poll once
/// and pokes the poller: what was sent while it was down was missed. It catches a connection made
/// again that leaves the waits to their bounds.
#[tokio::test]
async fn the_connection_made_wakes_every_wait_once_and_pokes_the_poller() {
    let rig = clocked_rig("listening_made", true, NEVER);
    let agents = &rig.agents;
    let beat = agents.wakes.watch(&host("host-a"));
    let polls = [host("host-a"), host("host-b")].map(|id| agents.dispatching.wakes.watch(&id));
    heard_by(agents, &Heard::Listening);
    assert!(woken_now(&beat).await);
    for poll in &polls {
        assert!(woken_now(poll).await);
    }
    assert!(agents.poller.take_poke());
}

/// Asserts that a quiet wait, a lost connection and a notice that is not the job store's (another
/// channel, or a host channel naming no host) wake nothing and poke nothing. It catches a server
/// that takes any notice, or a loss, for a wake.
#[tokio::test]
async fn quiet_a_loss_and_a_foreign_notice_wake_nothing() {
    let rig = clocked_rig("listening_nothing", true, NEVER);
    let agents = &rig.agents;
    let poll = agents.dispatching.wakes.watch(&host("host-a"));
    let foreign = |channel: &str, payload: &str| {
        Heard::Notice(Notice {
            channel: channel.to_owned(),
            payload: payload.to_owned(),
        })
    };
    for heard in [
        Heard::Quiet,
        Heard::Lost(StoreError::Disconnected("ended".to_owned())),
        foreign("elsewhere", "host-a"),
        foreign(js_jobs::HOST_CHANNEL, "not a host"),
    ] {
        heard_by(agents, &heard);
    }
    assert!(!woken_now(&poll).await);
    assert!(!agents.poller.take_poke());
}

/// Asserts that a server with no store listens to nothing. It catches a listener started for a
/// server no other server shares a store with.
#[test]
fn a_server_without_a_store_listens_to_nothing() {
    let rig = clocked_rig("listening_none", true, NEVER);
    assert!(start(None, &rig.agents).is_none());
}
