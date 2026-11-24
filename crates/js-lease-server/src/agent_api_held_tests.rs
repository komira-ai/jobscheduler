//! Held heartbeats through the route ([`super::held`], design komira-ai/jobscheduler#903): a
//! reply with nothing to send to a host that reported `held-beat` waits for a placement on the
//! host and carries its start, or ends at the bound and says `beat_now`. The tests wait on the
//! server's own progress (each wait of a held reply is counted, [`crate::held_beat::Wakes`]),
//! never on time: a hold that a test does not end is bounded by [`LONG`], so a wake that is lost
//! shows as a reply without the start, not as a slow test that passes.

use std::time::Duration;

use js_agent_wire::{HeartbeatReply, HeldState};
use js_jobs::{AllocationStep, JobState};
use js_types::HostId;

use crate::held_beat::STARTING_WINDOW_MS;
use crate::job_expiry::JobLimits;

use super::job_rig::{FakeAgent, Rig, clocked_rig, starts, whole_rig};
use super::whole_tests::{place_all, queue_share, queue_whole, rules};
use js_jobs::JobId;

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
/// that heartbeat's reply, with no second heartbeat; the reply carries a command, so it does
/// not say `beat_now` (the start is sent again until the agent lists it). It catches a
/// placement that does not wake the held reply, so the start waits for the next heartbeat, and
/// a woken start that asks the agent to beat again before it can list the job.
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
    assert!(!reply.beat_now);
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

/// Asserts that a reply that has a start to send is answered at once, without `beat_now`, that
/// the heartbeat of a host that did not report `held-beat` is never held, and that neither is
/// the heartbeat of a draining host that did. It catches a hold that delays a start the store
/// already holds, one that stretches the interval of an agent that waits the interval after
/// every reply, and one that stretches a drain, which the agent counts in the waits it asks
/// for.
#[tokio::test]
async fn a_reply_with_a_start_a_host_without_held_beat_and_a_draining_host_are_not_held() {
    let (mut rig, mut agent) = held_rig("held_not", LONG);
    rig.place("job-1", "host-a");
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(starts(&reply).len(), 1);
    assert!(!reply.beat_now);
    agent.take(&reply);
    agent.phase = js_agent_wire::Phase::Draining;
    let draining = rig.beat(&agent).await.unwrap();
    assert!(!draining.beat_now);
    rig.register("host-b", true);
    let other = rig.beat(&FakeAgent::new("host-b")).await.unwrap();
    assert_eq!(other, HeartbeatReply::default());
    assert_eq!(waits(&rig), 0);
}

/// Asserts that a reply that carries only the acknowledgement of a result is answered at once,
/// not held, and still says `beat_now`, so the agent's next heartbeat goes at once and is held.
/// It catches an agent left to wait a whole interval after each job's end, with no heartbeat
/// waiting at the server for the next start.
#[tokio::test]
async fn a_reply_with_only_an_acknowledgement_is_not_held_and_says_beat_now() {
    let (rig, mut agent) = held_rig("held_ack", LONG);
    rig.place("job-1", "host-a");
    agent.take(&rig.beat(&agent).await.unwrap());
    agent.run("job-1");
    agent.end("job-1", js_agent_wire::Outcome::Succeeded, Some(0));
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply.commands, []);
    assert_eq!(reply.job_acks.len(), 1);
    assert!(reply.beat_now);
    assert_eq!(waits(&rig), 0);
}

/// Asserts that a reply that acknowledges a taint the beat reported is answered at once, not
/// held, and says `beat_now` (it has no command). It catches a hold that delays the
/// acknowledgement the agent keeps reporting the taint until.
#[tokio::test]
async fn a_reply_with_a_taint_acknowledgement_is_not_held_and_says_beat_now() {
    let (rig, agent) = held_rig("held_taint", LONG);
    let taint = js_agent_wire::Taint {
        reason: "cleanup-failed".to_owned(),
        run_id: None,
        check: None,
    };
    let reply = rig
        .beat_reporting(&agent, Some(taint.clone()))
        .await
        .unwrap();
    assert_eq!(
        reply,
        HeartbeatReply {
            taint_ack: true,
            host_taint: Some(taint),
            beat_now: true,
            ..HeartbeatReply::default()
        }
    );
    assert_eq!(waits(&rig), 0);
}

/// Asserts that the reply to a host the registry holds tainted, which names the taint
/// (`host_taint`) and carries nothing else, is held as a clean host's is: it waits once and
/// ends at the bound with the taint named and `beat_now`. It catches a reply that names the
/// taint counted as something to send, which would answer every beat of a tainted host at
/// once and leave it beating in a loop with no hold.
#[tokio::test]
async fn a_reply_that_only_names_the_held_taint_is_held() {
    let (rig, agent) = held_rig("held_host_taint", Duration::ZERO);
    let taint = js_agent_wire::Taint {
        reason: "residue".to_owned(),
        run_id: None,
        check: None,
    };
    let reported = rig
        .beat_reporting(&agent, Some(taint.clone()))
        .await
        .unwrap();
    assert!(reported.taint_ack);
    assert_eq!(waits(&rig), 0);

    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(
        reply,
        HeartbeatReply {
            host_taint: Some(taint),
            ..quiet_held()
        }
    );
    assert_eq!(waits(&rig), 1);
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

/// Asserts that a reply held when the server begins to drain is answered at the drain, empty
/// and saying `beat_now`, after the one wait it had begun: a placement on the host and its wake
/// that come with the drain do not hold it on or put a start in it (the host is leaving, and
/// the server it says hello to next sends the start). It catches a drain that leaves an
/// agent's heartbeat held on a server that stops, so the agent learns it must move only at the
/// bound or at the next wake.
#[tokio::test]
async fn a_held_reply_is_answered_when_the_drain_begins() {
    let (rig, agent) = held_rig("held_drain", LONG);
    let drain = async {
        waited(&rig, 1).await;
        rig.agents.draining.begin();
        rig.place("job-1", "host-a");
        rig.agents
            .wakes
            .wake(&js_types::HostId::new("host-a").unwrap());
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), drain);
    assert_eq!(reply.unwrap(), quiet_held());
    assert_eq!(waits(&rig), 1);
}

/// Asserts that a server that drains holds no reply: a heartbeat with nothing to send is
/// answered at once, empty and saying `beat_now`, and no hold began. It catches a drain that
/// ends the holds under way but lets a heartbeat that arrives after it wait the bound.
#[tokio::test]
async fn a_draining_server_holds_no_reply() {
    let (rig, agent) = held_rig("held_drained", LONG);
    rig.agents.draining.begin();
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply, quiet_held());
    assert_eq!(waits(&rig), 0);
}

/// A placing server holding replies for `bound`, and replies to a beat that lists a job
/// `starting` for `starting`, with `host-a` registered as an agent that takes held replies;
/// that agent, which has been given `job-1` and lists it `starting`.
async fn starting_rig(name: &str, bound: Duration, starting: Duration) -> (Rig, FakeAgent) {
    let (rig, mut agent) = held_rig(name, bound);
    let rig = rig.holding_starting_for(starting);
    queue_share(&rig, "job-1", 600);
    place_all(&rig);
    agent.take(&rig.beat(&agent).await.unwrap());
    assert!(agent.holds("job-1"));
    (rig, agent)
}

/// Asserts which bound a beat's hold takes: the short one when the listing has a job `starting`
/// (alone or among others) that a hold first saw so less than the window ago, the normal one
/// when it has none, is empty, or every job of it has been `starting` for the window; a job
/// newly listed `starting` takes the short one again. The two bounds differ, so the value says
/// which was taken, and the rig's manual clock moves the window; nothing waits. It catches a
/// hold that takes the normal bound for a host with a start under way, so the start's `running`
/// reaches the server a hold later, the short bound taken for every beat, and a start that waits
/// for minutes keeping its host beating five times a second.
#[test]
fn a_listing_with_a_young_starting_job_takes_the_short_bound_and_any_other_the_normal_one() {
    let (rig, mut agent) = held_rig("held_limit", Duration::from_secs(2));
    let rig = rig.holding_starting_for(Duration::from_secs(1));
    let host = HostId::new("host-a").unwrap();
    let limit = |agent: &FakeAgent| {
        let young = super::held::young_start(&rig.agents, &host, &agent.listing());
        super::held::bound(&rig.agents, young)
    };
    let (short, normal) = (Duration::from_secs(1), Duration::from_secs(2));
    assert_eq!(limit(&agent), normal);
    agent.hold("job-1", "alloc-1", HeldState::Running);
    assert_eq!(limit(&agent), normal);
    agent.hold("job-2", "alloc-2", HeldState::Starting);
    assert_eq!(limit(&agent), short);
    rig.advance(Duration::from_millis(STARTING_WINDOW_MS - 1));
    assert_eq!(limit(&agent), short);
    rig.advance(Duration::from_millis(1));
    assert_eq!(limit(&agent), normal);
    agent.hold("job-3", "alloc-3", HeldState::Starting);
    assert_eq!(limit(&agent), short);
}

/// Asserts that a heartbeat that lists a job `starting` is held no longer than the short bound
/// for such a beat, however long the normal bound is: its reply ends after one wait, empty and
/// saying `beat_now`, and the hold was given the short bound. It shows the hold taking that
/// bound end to end (a hold that took the normal bound here would end a bound later, and say so
/// in the bound it was given).
#[tokio::test]
async fn a_beat_that_lists_a_starting_job_ends_at_the_short_bound() {
    let (rig, agent) = starting_rig("held_starting_short", LONG, Duration::ZERO).await;
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply, quiet_held());
    assert_eq!(waits(&rig), 1);
    assert_eq!(rig.agents.wakes.last_bound(), Some(Duration::ZERO));
}

/// Asserts that a heartbeat that lists no job `starting` (here one that runs) is held to the
/// normal bound: with the short bound at nothing, the reply is still waiting when a placement
/// comes, and carries its start. It catches a short bound taken for every beat of a host that
/// holds a job.
#[tokio::test]
async fn a_beat_that_lists_no_starting_job_is_held_to_the_normal_bound() {
    let (rig, mut agent) = starting_rig("held_not_starting", LONG, Duration::ZERO).await;
    agent.run("job-1");
    let placed = async {
        waited(&rig, 1).await;
        queue_share(&rig, "job-2", 600);
        place_all(&rig);
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), placed);
    let started: Vec<String> = starts(&reply.unwrap())
        .into_iter()
        .map(|s| s.job_id)
        .collect();
    assert_eq!(started, ["job-2"]);
}

/// Asserts that a heartbeat that lists a job `starting` is held to the short bound, and that a
/// placement on the host meanwhile ends that hold at once and its reply carries the start,
/// without `beat_now`. The normal bound is nothing and the short one long, so a hold that took
/// the normal bound would answer at once, empty, before the placement. It catches a hold that
/// does not take the bound of the beat's own listing, and a short hold not watched for wakes.
#[tokio::test]
async fn a_placement_wakes_a_beat_that_lists_a_starting_job() {
    let (rig, agent) = starting_rig("held_starting_wake", Duration::ZERO, LONG).await;
    let placed = async {
        waited(&rig, 1).await;
        queue_share(&rig, "job-2", 600);
        place_all(&rig);
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), placed);
    let reply = reply.unwrap();
    let started: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(started, ["job-2"]);
    assert!(!reply.beat_now);
    assert_eq!(rig.agents.wakes.last_bound(), Some(LONG));
}

/// Asserts that a server that drains places no job, by the tick's pass or a heartbeat's: a
/// queued job stays queued with no allocation while the registered host has room, and a
/// heartbeat of the host gets no start. It catches a draining server that places a job on a
/// host that is leaving it, where another server would have to start it.
#[tokio::test]
async fn a_draining_server_places_no_job() {
    let (rig, agent) = held_rig("held_no_place", Duration::ZERO);
    let job = queue_share(&rig, "job-1", 600);
    rig.agents.draining.begin();
    place_all(&rig);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(starts(&reply).len(), 0, "{reply:?}");
    assert_eq!(rig.job(&job).state, JobState::Queued);
    assert_eq!(
        rig.jobs
            .live_on(&js_types::HostId::new("host-a").unwrap())
            .unwrap()
            .len(),
        0
    );
}

/// A placing server holding replies for `bound`, and paces replies to beats that list a young
/// start by `starting`, with `host-a` registered as an agent that takes held replies and polls
/// dispatch (its starts come by the poll, never in a heartbeat's reply); that agent.
fn polling_rig(name: &str, bound: Duration, starting: Duration) -> (Rig, FakeAgent) {
    let mut rig = clocked_rig(name, true, JobLimits::default())
        .holding_for(bound)
        .holding_starting_for(starting);
    let capabilities = serde_json::json!({
        "per-job": "true",
        "held-beat": "true",
        js_agent_wire::DISPATCH_CAPABILITY: "true",
        "arch": "x86_64",
    });
    rig.register_with("host-a", capabilities);
    (rig, FakeAgent::new("host-a"))
}

/// The reply to a beat of a host that polls dispatch that ended with nothing to send and asks
/// the agent to beat again when it has news, not at once.
fn on_news() -> HeartbeatReply {
    HeartbeatReply::default()
}

/// Whether `bound`, the bound a paced hold was given, is nearly the whole of `spacing`: the rig's
/// store moves the server's clock by a few milliseconds a commit, so a beat right after an
/// answer waits a little less than the spacing, never more.
fn nearly_whole(bound: Option<Duration>, spacing: Duration) -> bool {
    bound.is_some_and(|bound| bound <= spacing && bound > spacing / 2)
}

/// Has `agent` list the job `id`, which the poll handed it, as `starting` under its allocation.
fn taken(rig: &Rig, agent: &mut FakeAgent, id: &JobId) {
    let allocation = rig.allocation(id).allocation_id.to_string();
    agent.hold(id.as_str(), &allocation, HeldState::Starting);
}

/// Asserts that a placement on a host that polls dispatch ends the hold of its idle heartbeat:
/// the reply carries no start (the poll does), is answered after the one wait, and does not say
/// `beat_now`, so the agent's next heartbeat leaves when its worker has taken the start. It
/// catches a hold that rebuilds an empty reply at the wake and holds on to the bound: the agent's
/// one heartbeat thread then stays in that request through the whole start and reports its
/// `running` up to the bound late.
#[tokio::test]
async fn a_placement_ends_the_held_beat_of_a_host_that_polls_dispatch() {
    let (rig, agent) = polling_rig("held_polling_wake", LONG, LONG);
    let job = queue_share(&rig, "job-1", 600);
    let placed = async {
        waited(&rig, 1).await;
        rig.tick();
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), placed);
    assert_eq!(reply.unwrap(), on_news());
    assert_eq!(waits(&rig), 1);
    assert_eq!(rig.states(&job), (JobState::Placed, AllocationStep::Booked));
}

/// Asserts the path of one start on an idle host that polls dispatch: the beat that lists the
/// start `starting` is answered at once (no hold began) without `beat_now`, so the agent's next
/// heartbeat leaves when the container runs; that heartbeat reports the job `running` to the
/// store as it arrives, while its reply is held, and a placement then ends that hold too. The
/// short bound is long here, so a beat held to it would never be answered in the test. It
/// catches the beat with a start held to the short bound, which keeps the agent from reporting
/// `running` until the hold ends, and a reply that asks the agent to beat again at once.
#[tokio::test]
async fn a_lone_start_on_a_host_that_polls_dispatch_is_not_held_and_its_running_goes_at_once() {
    let (rig, mut agent) = polling_rig("held_polling_lone", LONG, LONG);
    let job = queue_share(&rig, "job-1", 600);
    place_all(&rig);
    taken(&rig, &mut agent, &job);
    let reply = rig.beat(&agent).await.unwrap();
    assert_eq!(reply, on_news());
    assert_eq!(waits(&rig), 0);
    assert_eq!(rig.agents.wakes.last_bound(), None);
    agent.run("job-1");
    let next = async {
        waited(&rig, 1).await;
        assert_eq!(rig.states(&job).0, JobState::Running);
        queue_share(&rig, "job-2", 600);
        place_all(&rig);
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), next);
    assert_eq!(reply.unwrap(), on_news());
    assert_eq!(rig.agents.wakes.last_bound(), Some(LONG));
}

/// Asserts that close beats of a host that polls dispatch with a start under way are paced: the
/// first is answered at once; the next three, each sent as soon as the one before was answered
/// (as an agent with news to tell does), are each held for nearly the whole spacing (here ended
/// by a wake); once the spacing has passed on the server's clock since the last answer, the
/// next is answered at once again. No reply says `beat_now`. So such a host is answered no more
/// often than once per spacing, as a held beat with a start was before, plus once per wake. It
/// catches pacing that answers every beat of a burst at once, pacing that never lets a beat go
/// at once again, and a paced reply that asks the agent to beat again at once.
#[tokio::test]
async fn close_beats_of_a_host_that_polls_dispatch_with_a_start_under_way_are_paced() {
    // Shorter than the window, so the start is still young when the spacing has passed.
    let spacing = Duration::from_secs(1);
    let (rig, mut agent) = polling_rig("held_polling_burst", LONG, spacing);
    let host = HostId::new("host-a").unwrap();
    let job = queue_share(&rig, "job-1", 600);
    place_all(&rig);
    taken(&rig, &mut agent, &job);
    assert_eq!(rig.beat(&agent).await.unwrap(), on_news());
    assert_eq!(waits(&rig), 0);
    for n in 1..=3 {
        let woken = async {
            waited(&rig, n).await;
            rig.agents.wakes.wake(&host);
        };
        let (reply, ()) = tokio::join!(rig.beat(&agent), woken);
        assert_eq!(reply.unwrap(), on_news());
        assert_eq!(waits(&rig), n);
        assert!(nearly_whole(rig.agents.wakes.last_bound(), spacing));
    }
    rig.advance(spacing);
    assert_eq!(rig.beat(&agent).await.unwrap(), on_news());
    assert_eq!(waits(&rig), 3);
}

/// Asserts that a paced beat of a host that polls dispatch that no wake ends is answered at the
/// end of its spacing, empty and without `beat_now`; that a beat whose start has been `starting`
/// for longer than the window is still paced (answered at once once the spacing has passed),
/// not held to the normal bound; and that a beat of such a host that lists no start (its job
/// runs) is held to the normal bound and answered at it with `beat_now`, as any host's. It
/// catches a paced hold that never ends, a paced reply at the end of its spacing that asks the
/// agent to beat again at once (a burst would then beat in a loop), a long start whose beat is
/// held to the normal bound (its `running` then waits up to that bound), and pacing applied to
/// a beat with no start.
#[tokio::test]
async fn a_paced_beat_ends_at_its_spacing_a_long_start_stays_paced_and_no_start_is_held() {
    let spacing = Duration::from_millis(200);
    let (rig, mut agent) = polling_rig("held_polling_spacing", Duration::ZERO, spacing);
    let job = queue_share(&rig, "job-1", 600);
    place_all(&rig);
    taken(&rig, &mut agent, &job);
    assert_eq!(rig.beat(&agent).await.unwrap(), on_news());
    assert_eq!(rig.beat(&agent).await.unwrap(), on_news());
    assert_eq!(waits(&rig), 1);
    assert!(nearly_whole(rig.agents.wakes.last_bound(), spacing));
    rig.advance(Duration::from_millis(STARTING_WINDOW_MS));
    assert_eq!(rig.beat(&agent).await.unwrap(), on_news());
    assert_eq!(waits(&rig), 1);
    agent.run("job-1");
    assert_eq!(rig.beat(&agent).await.unwrap(), quiet_held());
    assert_eq!(waits(&rig), 2);
    assert_eq!(rig.agents.wakes.last_bound(), Some(Duration::ZERO));
}

/// Asserts that a paced beat of a host that polls dispatch held when the server begins to drain
/// is answered at the drain saying `beat_now`, so the agent's next heartbeat goes at once to a
/// server that still accepts one. It catches pacing that drops the drain's `beat_now` and leaves
/// the agent waiting its interval on a server that is going away.
#[tokio::test]
async fn a_paced_beat_held_at_a_drain_says_beat_now() {
    let (rig, mut agent) = polling_rig("held_polling_drain", LONG, LONG);
    let job = queue_share(&rig, "job-1", 600);
    place_all(&rig);
    taken(&rig, &mut agent, &job);
    assert_eq!(rig.beat(&agent).await.unwrap(), on_news());
    let drain = async {
        waited(&rig, 1).await;
        rig.agents.draining.begin();
    };
    let (reply, ()) = tokio::join!(rig.beat(&agent), drain);
    assert_eq!(reply.unwrap(), quiet_held());
}
