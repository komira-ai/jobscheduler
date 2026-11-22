//! The placement poller's wake signal and its pass, on the job rig's agent side. Nothing waits
//! on time: a poke is checked by polling the poller's wait once.

use js_jobs::{JobId, JobState, NewJob};

use super::{pass, poked_pass};
use crate::agent_api::job_rig::{Rig, clocked_rig, spec};
use crate::agent_api::peer_rig::NEVER;

/// The job id `name`.
fn id(name: &str) -> JobId {
    JobId::parse(name).unwrap()
}

/// A placing rig with `host-a` registered, whose agent reports `per-job`, and the queued job
/// `job-a`, which `host-a` has room for.
fn rig_with_a_queued_job(name: &str) -> Rig {
    let mut rig = clocked_rig(name, true, NEVER);
    rig.register("host-a", true);
    let new = NewJob {
        id: Some(id("job-a")),
        spec: spec(),
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    rig
}

/// Asserts that no poke is waiting until one comes, that a poke is kept until the poller takes
/// it, and that pokes that come before it does are kept as one. It catches a poke that is lost
/// when no pass waits, and pokes that queue up a pass each.
#[test]
fn pokes_before_the_poller_waits_are_kept_as_one() {
    let poller = super::Poller::default();
    assert!(!poller.take_poke());
    poller.poke();
    poller.poke();
    assert!(poller.take_poke());
    assert!(!poller.take_poke());
}

/// Asserts that the poller's pass, once poked, places a queued job the host has room for, and
/// takes the poke. It catches a pass that waits for the tick, and one that leaves the poke.
#[tokio::test]
async fn a_poked_pass_places_the_queued_job() {
    let rig = rig_with_a_queued_job("poller_places");
    rig.agents.poller.poke();
    pass(&rig.agents).await;
    assert_eq!(rig.job(&id("job-a")).state, JobState::Placed);
    assert!(!rig.agents.poller.take_poke());
}

/// Asserts that the in-process turn of the poller does nothing without a poke (the job stays
/// queued), and with one takes it and places the job, leaving no poke behind. It catches a turn
/// that runs a pass unpoked, one that skips a poke, and one that leaves the poke for a second
/// pass.
#[tokio::test]
async fn a_poked_turn_runs_a_pass_only_when_poked() {
    let rig = rig_with_a_queued_job("poller_turn");
    assert!(!poked_pass(&rig.agents).await);
    assert_eq!(rig.job(&id("job-a")).state, JobState::Queued);
    rig.agents.poller.poke();
    assert!(poked_pass(&rig.agents).await);
    assert_eq!(rig.job(&id("job-a")).state, JobState::Placed);
    assert!(!rig.agents.poller.take_poke());
}

/// Asserts that a server that drains places nothing when poked: its hosts are leaving for
/// another server. It catches a poller that places past the start of a drain.
#[tokio::test]
async fn a_poked_pass_of_a_draining_server_places_nothing() {
    let rig = rig_with_a_queued_job("poller_draining");
    rig.agents.draining.begin();
    rig.agents.poller.poke();
    pass(&rig.agents).await;
    assert_eq!(rig.job(&id("job-a")).state, JobState::Queued);
}
