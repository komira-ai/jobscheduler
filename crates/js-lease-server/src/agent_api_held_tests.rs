//! Held heartbeats through the route ([`super::held`], design komira-ai/jobscheduler#903): a
//! reply with nothing to send to a host that reported `held-beat` waits for a placement on the
//! host and carries its start, or ends at the bound and says `beat_now`. The tests wait on the
//! server's own progress (each wait of a held reply is counted, [`crate::held_beat::Wakes`]),
//! never on time: a hold that a test does not end is bounded by [`LONG`], so a wake that is lost
//! shows as a reply without the start, not as a slow test that passes.

use std::time::Duration;

use js_agent_wire::HeartbeatReply;
use js_jobs::{AllocationStep, JobState};

use crate::job_expiry::JobLimits;

use super::job_rig::{FakeAgent, Rig, clocked_rig, starts, whole_rig};
use super::whole_tests::{place_all, queue_share, queue_whole, rules};

/// A bound no test reaches: a hold a test does not end returns at it.
const LONG: Duration = Duration::from_secs(60);

/// A placing server holding replies for `bound`, with `host-a` registered as an agent that
/// takes held replies, and that agent.
fn held_rig(name: &str, bound: Duration) -> (Rig, FakeAgent) {
    let mut rig = clocked_rig(name, true, JobLimits::default()).holding_for(bound);
    rig.register_held("host-a");
    (rig, FakeAgent::new("host-a"))
}

/// Waits until held replies of `rig` have begun to wait `waits` times in all.
async fn waited(rig: &Rig, waits: u64) {
    let mut seen = rig.agents.wakes.waits();
    seen.wait_for(|seen| *seen >= waits).await.unwrap();
}

/// How many times held replies of `rig` began to wait.
fn waits(rig: &Rig) -> u64 {
    *rig.agents.wakes.waits().borrow()
}

/// The reply of a hold that ended with nothing to send.
fn quiet_held() -> HeartbeatReply {
    HeartbeatReply {
        beat_now: true,
        ..HeartbeatReply::default()
    }
}

/// Asserts that a job the tick's pass places while the host's heartbeat is held is started by
/// that heartbeat's reply, which says `beat_now`, with no second heartbeat. It catches a
/// placement that does not wake the held reply, so the start waits for the next heartbeat.
#[tokio::test]
async fn a_job_the_tick_places_rides_on_the_held_reply() {
    let (rig, agent) = held_rig("held_tick", LONG);
    let job = queue_share(&rig, "job-1", 600);
    let tick = async {
        waited(&rig, 1).await;
        rig.tick();
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), tick);
    let reply = reply.unwrap();
    let started: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(started, ["job-1"]);
    assert!(reply.beat_now);
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
    assert_eq!(rig.allocation(&job).host.as_str(), "host-a");
}

/// Asserts that a reply with nothing to send is held to the bound and then answered empty with
/// `beat_now`, after one wait. It catches a hold that never ends and a held reply that does not
/// ask the agent to beat again at once (it would then wait the interval with no heartbeat
/// waiting here).
#[tokio::test]
async fn a_quiet_reply_ends_at_the_bound_and_says_beat_now() {
    let (rig, agent) = held_rig("held_bound", Duration::ZERO);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply, quiet_held());
    assert_eq!(waits(&rig), 1);
}

/// Asserts that a reply that has a start to send is answered at once, without `beat_now`, and
/// that the heartbeat of a host that did not report `held-beat` is never held. It catches a
/// hold that delays a start the store already holds, and one that stretches the interval of an
/// agent that waits the interval after every reply.
#[tokio::test]
async fn a_reply_with_a_start_and_a_host_without_held_beat_are_not_held() {
    let (mut rig, agent) = held_rig("held_not", LONG);
    rig.place("job-1", "host-a");
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(starts(&reply).len(), 1);
    assert!(!reply.beat_now);
    rig.register("host-b", true);
    let other = rig.beat(&FakeAgent::new("host-b")).await.unwrap();
    assert_eq!(other, HeartbeatReply::default());
    assert_eq!(waits(&rig), 0);
}

/// Asserts that a wake that brings nothing to send (no job placed on the host) keeps the reply
/// held, waiting again, and that the start of a job placed after it still rides on the reply.
/// It catches a hold that answers empty at the first wake, and one that stops listening after
/// it.
#[tokio::test]
async fn a_wake_with_nothing_to_send_holds_on() {
    let (rig, agent) = held_rig("held_spurious", LONG);
    let host = js_types::HostId::new("host-a").unwrap();
    let wakes = async {
        waited(&rig, 1).await;
        rig.agents.wakes.wake(&host);
        waited(&rig, 2).await;
        queue_share(&rig, "job-1", 600);
        rig.tick();
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), wakes);
    let reply = reply.unwrap();
    assert_eq!(starts(&reply).len(), 1);
    assert_eq!(waits(&rig), 2);
}

/// Asserts that a held reply whose store read fails after a wake is answered at once as it was
/// (empty, `beat_now`), without waiting again, and with no start built from a read that did not
/// happen. It catches a failed read that holds the reply to the bound, or that answers an error
/// for a beat whose listing was committed.
#[tokio::test]
async fn a_failed_read_while_held_answers_the_reply_as_it_was() {
    let (rig, agent) = held_rig("held_read_fails", LONG);
    let host = js_types::HostId::new("host-a").unwrap();
    let fail = async {
        waited(&rig, 1).await;
        rig.store.fail_read_after(0);
        rig.agents.wakes.wake(&host);
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), fail);
    assert_eq!(reply.unwrap(), quiet_held());
    assert_eq!(waits(&rig), 1);
}

/// Asserts that a job that needs the whole host, placed while the host's heartbeat is held,
/// makes that reply name its allocation at once (the agent then prepares), with no command. It
/// catches a rebuilt reply judged by its commands alone, which would hold a prepare to the
/// bound.
#[tokio::test]
async fn a_whole_host_job_placed_while_held_names_its_allocation_at_once() {
    let mut rig = whole_rig("held_whole", rules(), None).holding_for(LONG);
    rig.register_held("host-a");
    let agent = FakeAgent::new("host-a");
    let job = queue_whole(&rig, "job-w", 600);
    let place = async {
        waited(&rig, 1).await;
        place_all(&rig);
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), place);
    let reply = reply.unwrap();
    let allocation = rig.allocation(&job).allocation_id.to_string();
    assert_eq!(reply.exclusive, Some(allocation));
    assert_eq!(reply.commands, []);
    assert!(reply.beat_now);
}
