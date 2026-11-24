//! The dispatch poll through the route ([`super::poll`]): two servers over one store, a server
//! that dies or drains, held polls, acknowledgements, the heartbeat of a host that polls and
//! the refusals. The store is the fake behind the wrapper that blocks or fails what it is told
//! to, and every clock is manual. Nothing waits on time: a held poll is observed by the waits it
//! counts ([`crate::held_beat::Wakes::waits`]), and a hold a test does not end is bounded by
//! [`LONG`], so a wake that is lost shows as a reply without the message, not as a slow test
//! that passes.

use std::time::Duration;

use js_agent_wire::{HeldState, InstanceIdentity};
use js_jobs::{AllocationId, JobId, JobState};
use js_state::Store as _;
use serde_json::json;

use super::*;
use crate::agent_api::job_rig::{
    BOOKED, FakeAgent, Rig, START_MS, clocked_rig, placed, rig as untabled, spec, starts, stops,
    whole_over,
};
use crate::agent_api::serve_hello;
use crate::agent_api::whole_tests::{queue_share, rules};
use crate::job_expiry::JobLimits;

#[path = "dispatch_unbuilt_tests.rs"]
mod unbuilt;

/// A bound no test reaches: a hold or a recheck a test does not end returns at it.
const LONG: Duration = Duration::from_secs(60);

/// A recheck no test reaches, longer than [`LONG`]: a test that is to be answered by a wake
/// has no recheck that could answer it first, so a lost wake cannot hide behind one.
const NEVER: Duration = Duration::from_secs(3600);

/// The capabilities of an agent that polls dispatch.
fn polling() -> serde_json::Value {
    json!({ "per-job": "true", "dispatch-poll": "true", "arch": "x86_64" })
}

/// `rig` with its polls held for `hold` at most and rechecked every `recheck`.
fn timed(mut rig: Rig, hold: Duration, recheck: Duration) -> Rig {
    let mut agents = Arc::try_unwrap(rig.agents).ok().unwrap();
    agents.dispatching.times.hold = hold;
    agents.dispatching.times.recheck = recheck;
    rig.agents = Arc::new(agents);
    rig
}

/// A server named `name` over a store of its own, claiming as `claimer`, with `host-a`
/// registered as an agent that polls; its polls end at once when nothing is due.
fn server(name: &str, claimer: &str) -> Rig {
    let mut rig = clocked_rig(name, true, JobLimits::default());
    rig.register_with("host-a", polling());
    rig.agents.dispatching.set_claimer(claimer.to_owned());
    timed(rig, Duration::ZERO, NEVER)
}

/// Another server over the store of `first`, claiming as `claimer`, with `host-a` registered
/// there too (a host says hello to each server it uses).
fn second(first: &Rig, name: &str, claimer: &str) -> Rig {
    let mut rig = whole_over(name, (rules(), None), Some(first), START_MS);
    rig.register_with("host-a", polling());
    rig.agents.dispatching.set_claimer(claimer.to_owned());
    timed(rig, Duration::ZERO, NEVER)
}

/// The wire form of the poll of `host` in the session `rig` holds for it, carrying `acks`.
fn body(rig: &Rig, host: &str, acks: Vec<DispatchAck>) -> Bytes {
    let (epoch, session) = rig.sessions[host].clone();
    let sent = DispatchPoll {
        host_id: host.to_owned(),
        epoch,
        session,
        acks,
    };
    Bytes::from(serde_json::to_vec(&sent).unwrap())
}

/// `body` through the route as the pool's token, with the marks of the request.
async fn sent(
    rig: &Rig,
    body: Bytes,
    marks: (Option<Extension<OneTime>>, Option<Extension<CertifiedHost>>),
) -> Result<DispatchReply, Refused> {
    let agents = State(Arc::clone(&rig.agents));
    let answer = poll(agents, Extension(rig.who.clone()), marks, Ok(body)).await;
    answer.map(|Json(reply)| reply)
}

/// The poll of `host` carrying `acks`, answered by the route of `rig`.
async fn polled_by(
    rig: &Rig,
    host: &str,
    acks: Vec<DispatchAck>,
) -> Result<DispatchReply, Refused> {
    sent(rig, body(rig, host, acks), (None, None)).await
}

/// The poll of `host-a` carrying `acks`.
async fn polled(rig: &Rig, acks: Vec<DispatchAck>) -> Result<DispatchReply, Refused> {
    polled_by(rig, "host-a", acks).await
}

/// The kind and job of each message of `reply`, in order of job id.
fn names(reply: &DispatchReply) -> Vec<(js_agent_wire::DispatchKind, String)> {
    let mut named: Vec<_> = reply
        .messages
        .iter()
        .map(|message| (message.kind, message.job_id.clone()))
        .collect();
    named.sort_by(|a, b| a.1.cmp(&b.1));
    named
}

/// The acknowledgement of each message of `reply`.
fn acks_of(reply: &DispatchReply) -> Vec<DispatchAck> {
    reply.messages.iter().map(Dispatched::ack).collect()
}

/// A start of `job`, as `names` lists it.
fn start(job: &str) -> (js_agent_wire::DispatchKind, String) {
    (js_agent_wire::DispatchKind::Start, job.to_owned())
}

/// The messages `host-a` has queued, handed out or not.
fn queued(rig: &Rig) -> Vec<DispatchMessage> {
    rig.jobs.dispatch_queue(&host()).unwrap()
}

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

/// Waits until the held polls of `rig` have begun to wait `waits` times in all.
async fn waited(rig: &Rig, waits: u64) {
    let mut seen = rig.agents.dispatching.wakes.waits();
    seen.wait_for(|seen| *seen >= waits).await.unwrap();
}

/// Waits until the held heartbeats of `rig` have begun to wait `waits` times in all.
async fn beat_waited(rig: &Rig, waits: u64) {
    let mut seen = rig.agents.wakes.waits();
    seen.wait_for(|seen| *seen >= waits).await.unwrap();
}

/// How many times held polls of `rig` began to wait.
fn waits(rig: &Rig) -> u64 {
    *rig.agents.dispatching.wakes.waits().borrow()
}

/// Catches a message handed to the host twice, and one handed to neither server, when two
/// servers over one store are polled by one host. Each queued message reaches the host
/// exactly once from the server that claims it: the second server, polled while the first
/// holds a claim, hands out nothing of it and only what the first has not seen; the claims
/// carry their server's name; and an acknowledgement sent to either server deletes.
#[tokio::test]
async fn two_servers_hand_each_message_out_once() {
    let a = server("dispatch_two_a", "server-a");
    let b = second(&a, "dispatch_two_b", "server-b");
    a.place("job-1", "host-a");
    a.place("job-2", "host-a");
    let first = polled(&a, Vec::new()).await.unwrap();
    assert_eq!(names(&first), [start("job-1"), start("job-2")]);
    assert_eq!(
        polled(&b, Vec::new()).await.unwrap(),
        DispatchReply::default(),
        "the second server gets nothing while the first holds both"
    );
    a.place("job-3", "host-a");
    let third = polled(&b, Vec::new()).await.unwrap();
    assert_eq!(names(&third), [start("job-3")]);
    assert_eq!(
        polled(&a, Vec::new()).await.unwrap(),
        DispatchReply::default(),
        "the first server gets nothing of what the second claimed"
    );
    let claimers: Vec<(String, Option<String>)> = {
        let mut seen: Vec<_> = queued(&a)
            .into_iter()
            .map(|message| (message.job.to_string(), message.claimed_by))
            .collect();
        seen.sort();
        seen
    };
    let by = |name: &str| Some(name.to_owned());
    assert_eq!(
        claimers,
        [
            ("job-1".to_owned(), by("server-a")),
            ("job-2".to_owned(), by("server-a")),
            ("job-3".to_owned(), by("server-b")),
        ]
    );
    // The acknowledgements of the first server's messages reach the second, and its own reach
    // the first: the queue is emptied by whichever takes them.
    assert_eq!(
        polled(&b, acks_of(&first)).await.unwrap(),
        DispatchReply::default()
    );
    assert_eq!(queued(&a).len(), 1);
    polled(&a, acks_of(&third)).await.unwrap();
    assert!(queued(&a).is_empty());
}

/// Catches a server that dies after claiming a message and leaves it lost, or one that hands it
/// out again before its claim ends. The first server claims the message and is gone without
/// answering; while the store's clock reads at or before the claim's end the second server hands
/// out nothing; one tick after, it hands out the message, under its own name.
#[tokio::test]
async fn a_message_claimed_by_a_server_that_died_is_handed_out_again_after_its_claim_ends() {
    let a = server("dispatch_crash_a", "server-a");
    let b = second(&a, "dispatch_crash_b", "server-b");
    a.place("job-1", "host-a");
    let lost = polled(&a, Vec::new()).await.unwrap();
    assert_eq!(names(&lost), [start("job-1")]);
    drop(lost);
    drop(a);
    let until = queued(&b)[0].claim_until.unwrap();
    let left = until.as_duration() - b.store.now().unwrap().as_duration();
    b.advance(left);
    assert_eq!(
        polled(&b, Vec::new()).await.unwrap(),
        DispatchReply::default(),
        "the claim holds through its last instant"
    );
    b.advance(Duration::from_millis(1));
    let again = polled(&b, Vec::new()).await.unwrap();
    assert_eq!(names(&again), [start("job-1")]);
    assert_eq!(queued(&b)[0].claimed_by.as_deref(), Some("server-b"));
}

/// Catches a drain that cuts off a poll that has claimed, one that lets a server claim after the
/// signal, and one that stops acknowledgements. A barrier holds the first server's poll inside
/// the claim of a message (no sleeps); the drain begins, as it does at the signal
/// ([`crate::server::Running::hold`] begins it first); the barrier is released. The poll is
/// answered with the message and its claim is committed. A poll of the draining server after
/// that claims nothing and still applies its acknowledgement; a message written after the signal
/// stays unclaimed on the first server and is handed out by the second.
#[tokio::test]
async fn a_poll_that_claimed_before_the_drain_is_answered_and_no_claim_follows_it() {
    let a = server("dispatch_drain_a", "server-a");
    let b = second(&a, "dispatch_drain_b", "server-b");
    a.place("job-1", "host-a");
    let (reached, in_the_claim) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    a.store.before_next(move || {
        reached.send(()).unwrap();
        released.recv().unwrap();
    });
    let signal = async {
        in_the_claim.await.unwrap();
        a.agents.draining.begin();
        release.send(()).unwrap();
    };
    let (reply, ()) = tokio::join!(polled(&a, Vec::new()), signal);
    let reply = reply.unwrap();
    assert_eq!(names(&reply), [start("job-1")]);
    assert_eq!(queued(&a)[0].claimed_by.as_deref(), Some("server-a"));

    a.place("job-2", "host-a");
    let after = polled(&a, acks_of(&reply)).await.unwrap();
    assert_eq!(after, DispatchReply::default());
    let left = queued(&a);
    assert_eq!(left.len(), 1, "the acknowledgement was applied");
    assert_eq!(left[0].job.to_string(), "job-2");
    assert_eq!(left[0].claimed_by, None, "the draining server claimed none");
    assert_eq!(waits(&a), 0, "and held none");
    let by_b = polled(&b, Vec::new()).await.unwrap();
    assert_eq!(names(&by_b), [start("job-2")]);
    assert_eq!(queued(&a)[0].claimed_by.as_deref(), Some("server-b"));
}

/// Catches a drain that leaves a poll held on a server that stops, and one that claims for a
/// poll it ends. A poll held with nothing due is answered empty when the drain begins, after the
/// one wait it had begun; a message placed with the drain, and the wake that comes with it, is
/// left unclaimed.
#[tokio::test]
async fn a_held_poll_is_answered_empty_when_the_drain_begins() {
    let rig = timed(server("dispatch_held_drain", "server-a"), LONG, NEVER);
    let drain = async {
        waited(&rig, 1).await;
        rig.agents.draining.begin();
        rig.place("job-1", "host-a");
        rig.agents.dispatching.wakes.wake(&host());
    };
    let (reply, ()) = tokio::join!(polled(&rig, Vec::new()), drain);
    assert_eq!(reply.unwrap(), DispatchReply::default());
    assert_eq!(waits(&rig), 1);
    assert_eq!(queued(&rig)[0].claimed_by, None);
}

/// Catches a hold that never ends. A poll with nothing due is held to its bound and answered
/// empty, after one wait.
#[tokio::test]
async fn a_poll_with_nothing_due_ends_at_the_bound() {
    let rig = server("dispatch_bound", "server-a");
    let reply = polled(&rig, Vec::new()).await.unwrap();
    assert_eq!(reply, DispatchReply::default());
    assert_eq!(waits(&rig), 1);
}

/// Catches a placement that wakes the held heartbeat of a host but not its held poll (or the
/// poll's watch taking the heartbeat's over, which the host's two holds would do if they shared
/// one). A host holds a heartbeat and a poll; the pass places a job on it; the poll is answered
/// with the start, and the heartbeat, which was woken too (it waits again), carries no start
/// since the queue hands it out.
#[tokio::test]
async fn a_placement_wakes_the_held_poll_and_the_held_heartbeat_of_the_host() {
    let mut rig = clocked_rig("dispatch_wake", true, JobLimits::default()).holding_for(LONG);
    let all = json!({ "per-job": "true", "held-beat": "true", "dispatch-poll": "true", "arch": "x86_64" });
    rig.register_with("host-a", all);
    let rig = timed(rig, LONG, NEVER);
    let agent = FakeAgent::new("host-a");
    queue_share(&rig, "job-1", 600);
    let beat = rig.beat(&agent);
    tokio::pin!(beat);
    let placing = async {
        waited(&rig, 1).await;
        beat_waited(&rig, 1).await;
        rig.tick();
        beat_waited(&rig, 2).await;
    };
    let conduct = async {
        tokio::select! {
            biased;
            () = placing => {}
            _ = &mut beat => panic!("the heartbeat ended before the wake"),
        }
    };
    let (reply, ()) = tokio::join!(polled(&rig, Vec::new()), conduct);
    assert_eq!(names(&reply.unwrap()), [start("job-1")]);
    rig.agents.draining.begin();
    let held = beat.await.unwrap();
    assert!(starts(&held).is_empty(), "the queue hands the start out");
    assert!(held.beat_now);
}

/// Catches a held poll that never looks at the store again unless this server places: a message
/// another server wrote, which wakes nothing here, must be found by the recheck. The poll
/// begins its hold, a second server's pass writes a message, and the recheck hands it out.
#[tokio::test]
async fn a_held_poll_finds_a_message_another_server_wrote_at_its_recheck() {
    let a = timed(
        server("dispatch_recheck_a", "server-a"),
        LONG,
        Duration::from_millis(1),
    );
    let b = second(&a, "dispatch_recheck_b", "server-b");
    let writing = async {
        waited(&a, 1).await;
        b.place("job-1", "host-a");
    };
    let (reply, ()) = tokio::join!(polled(&a, Vec::new()), writing);
    assert_eq!(names(&reply.unwrap()), [start("job-1")]);
    assert_eq!(queued(&a)[0].claimed_by.as_deref(), Some("server-a"));
}

/// Catches a hold that answers empty at the first wake, and one that stops listening after it.
/// A wake with nothing to hand out keeps the poll held, waiting again; the message placed after
/// it is handed out.
#[tokio::test]
async fn a_wake_with_nothing_to_hand_out_holds_on() {
    let rig = timed(server("dispatch_spurious", "server-a"), LONG, NEVER);
    let wakes = async {
        waited(&rig, 1).await;
        rig.agents.dispatching.wakes.wake(&host());
        waited(&rig, 2).await;
        rig.place("job-1", "host-a");
        rig.agents.dispatching.wakes.wake(&host());
    };
    let (reply, ()) = tokio::join!(polled(&rig, Vec::new()), wakes);
    assert_eq!(names(&reply.unwrap()), [start("job-1")]);
    assert_eq!(waits(&rig), 2);
}

/// Catches a claim that fails inside the hold being answered as an error for a poll that claimed
/// nothing, or held to the bound, and a first claim that fails being answered as an empty reply
/// (the agent would then poll again at once, with no sign that the store is failing). The first
/// claim failing is 500 `state-write-failed`; one that fails after a wake in the hold is
/// answered empty at once.
#[tokio::test]
async fn a_failed_claim_is_a_500_at_first_and_an_empty_reply_in_the_hold() {
    let rig = timed(server("dispatch_claim_fails", "server-a"), LONG, NEVER);
    rig.store.fail_read_after(0);
    let refused = polled(&rig, Vec::new()).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (500, "state-write-failed")
    );
    let failing = async {
        waited(&rig, 1).await;
        rig.store.fail_read_after(0);
        rig.agents.dispatching.wakes.wake(&host());
    };
    let (reply, ()) = tokio::join!(polled(&rig, Vec::new()), failing);
    assert_eq!(reply.unwrap(), DispatchReply::default());
    assert_eq!(waits(&rig), 1);
}

/// Catches an acknowledgement lost with its poll that fails when it comes again, one that
/// deletes a message the agent has not seen, and a draining server that ignores
/// acknowledgements. The acknowledgement of a message handed out, sent to another server, deletes
/// it with no error, and sent again changes nothing; the acknowledgement of a message never
/// handed out, and one whose id is not an allocation id, leave the queue as it is; a draining
/// server applies them all; and the acknowledgement of a start does not delete the stop of the
/// same allocation.
#[tokio::test]
async fn acknowledgements_delete_what_was_handed_out_on_any_server_and_nothing_else() {
    let a = server("dispatch_ack_a", "server-a");
    let b = second(&a, "dispatch_ack_b", "server-b");
    a.place("job-1", "host-a");
    let first = polled(&a, Vec::new()).await.unwrap();
    // The reply and its acknowledgement are lost with server A's connection; the agent sends
    // the acknowledgement to server B.
    let ack = acks_of(&first);
    assert_eq!(
        polled(&b, ack.clone()).await.unwrap(),
        DispatchReply::default()
    );
    assert!(queued(&b).is_empty());
    assert_eq!(polled(&b, ack).await.unwrap(), DispatchReply::default());

    a.place("job-2", "host-a");
    let unseen = DispatchAck {
        kind: js_agent_wire::DispatchKind::Start,
        allocation_id: queued(&a)[0].allocation_id.to_string(),
    };
    let junk = DispatchAck {
        kind: js_agent_wire::DispatchKind::Start,
        allocation_id: "not-an-allocation".to_owned(),
    };
    a.agents.draining.begin();
    polled(&a, vec![unseen, junk]).await.unwrap();
    let left = queued(&a);
    assert_eq!(left.len(), 1, "a message never handed out stays");
    assert_eq!(left[0].claimed_by, None);

    // A start handed out, then a cancel: the stop is queued. The start's acknowledgement does
    // not delete the stop.
    let third = a.place("job-3", "host-a");
    let handed = polled(&b, Vec::new()).await.unwrap();
    assert_eq!(names(&handed).len(), 2, "job-2 and job-3");
    a.jobs.request_cancel(&third).unwrap();
    let stop = polled(&b, Vec::new()).await.unwrap();
    assert_eq!(
        stop.messages
            .iter()
            .map(|message| (message.kind, message.job_id.as_str()))
            .collect::<Vec<_>>(),
        [(js_agent_wire::DispatchKind::Stop, "job-3")]
    );
    let start_ack = acks_of(&handed)
        .into_iter()
        .filter(|ack| ack.kind == js_agent_wire::DispatchKind::Start);
    polled(&a, start_ack.collect()).await.unwrap();
    let rest = queued(&a);
    assert_eq!(
        rest.iter()
            .map(|message| message.job.to_string())
            .collect::<Vec<_>>(),
        ["job-3"],
        "only the stop of job-3 is left"
    );
}

/// Catches an acknowledgement that cannot be committed being taken for applied: the agent would
/// stop sending it and the message would be handed out again for ever. A failed commit is 500
/// `state-write-failed`, and the message is still there.
#[tokio::test]
async fn an_acknowledgement_that_cannot_be_committed_is_a_500() {
    let rig = server("dispatch_ack_fails", "server-a");
    rig.place("job-1", "host-a");
    let first = polled(&rig, Vec::new()).await.unwrap();
    rig.store.fail_next();
    let refused = polled(&rig, acks_of(&first)).await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (500, "state-write-failed")
    );
    assert_eq!(queued(&rig).len(), 1);
}

/// Catches a stop of a cancelled job that does not reach the host by the queue, and a start for
/// a job that was cancelled after it was queued. A cancel after the start was handed out queues
/// a stop, which the next poll hands out by the job's id and allocation; a start whose job the
/// pass could not build for the host (a host job, sent to a host that did not report
/// `host-jobs`) is left out of the reply, and stays claimed.
#[tokio::test]
async fn a_stop_is_handed_out_and_a_start_that_cannot_be_built_is_left_out() {
    let rig = server("dispatch_stop", "server-a");
    let job = rig.place("job-1", "host-a");
    let first = polled(&rig, Vec::new()).await.unwrap();
    assert_eq!(names(&first), [start("job-1")]);
    let spec_of_start = first.messages[0].start.as_deref().unwrap();
    assert_eq!(
        spec_of_start.allocation_id,
        rig.allocation(&job).allocation_id.to_string()
    );
    rig.jobs.request_cancel(&job).unwrap();
    let second = polled(&rig, acks_of(&first)).await.unwrap();
    assert_eq!(
        second.messages,
        [Dispatched::stop(
            "job-1".to_owned(),
            rig.allocation(&job).allocation_id.to_string()
        )]
    );

    let mut on_the_machine = spec();
    on_the_machine.constraints.isolation = js_jobs::Isolation::Host;
    placed(&rig.jobs, "job-2", "host-a", on_the_machine, BOOKED);
    let none = polled(&rig, Vec::new()).await.unwrap();
    assert_eq!(
        none,
        DispatchReply::default(),
        "the host job cannot be sent"
    );
    assert!(
        queued(&rig)
            .iter()
            .any(|message| message.job.to_string() == "job-2" && message.claimed_by.is_some()),
        "and stays claimed"
    );
}

/// Catches a reply over the wire's cap of commands. More messages are claimed than a reply may
/// carry (one more than the cap, of stops, which build from no row): the reply holds exactly the
/// cap, in the order claimed, and passes the wire's own check of a reply.
#[test]
fn a_reply_carries_at_most_the_cap() {
    let rig = server("dispatch_cap", "server-a");
    let job = rig.place("job-1", "host-a");
    let stop = DispatchMessage::new(
        DispatchKind::Stop,
        &rig.allocation(&job),
        rig.store.now().unwrap(),
    );
    let claimed = vec![stop; limits::MAX_COMMANDS + 1];
    let facts = HostFacts::of(&rig.agents, "host-a").unwrap();
    let everything = (Vec::new(), Vec::new(), limits::MAX_COMMANDS);
    let inputs = Inputs::new(&rig.jobs, &facts, None, everything);
    let context = inputs.context(&crate::metrics::NoMetrics);
    let handed = handed_out(&rig.jobs, &context, &claimed).unwrap();
    assert_eq!(handed.len(), limits::MAX_COMMANDS);
    assert!(DispatchReply { messages: handed }.validate().is_ok());
}

/// The messages of `rig`'s queue for the host and the records they are for.
fn record_of(rig: &Rig, id: &str) -> (DispatchMessage, JobRecord, AllocationRecord) {
    let job = rig.place(id, "host-a");
    let message = queued(rig).pop().unwrap();
    (message, rig.job(&job), rig.allocation(&job))
}

/// Catches a start sent for a job or allocation that is no longer the message's: the allocation
/// it names is not the job's current one, is no longer booked, or the job has ended or has a
/// cancel intent. Each of the four is left out, and the plain case is sent.
#[test]
fn a_start_is_sent_only_for_the_booked_allocation_of_a_live_job_without_a_cancel() {
    let rig = server("dispatch_startable", "server-a");
    let (message, job, allocation) = record_of(&rig, "job-1");
    assert!(startable(&message, &job, &allocation));
    let mut other = allocation.clone();
    other.allocation_id = AllocationId::from_bits([9; 16]);
    assert!(!startable(&message, &job, &other));
    let mut releasing = allocation.clone();
    releasing.step = AllocationStep::Releasing;
    assert!(!startable(&message, &job, &releasing));
    let mut ended = job.clone();
    ended.state = JobState::Cancelled;
    assert!(!startable(&message, &ended, &allocation));
    let mut asked = job;
    asked.cancel_requested = true;
    assert!(!startable(&message, &asked, &allocation));
}

/// Catches an allocation id taken from text that is not one: only 32 lowercase hex digits are
/// an id, and the kinds the store knows are the wire's two. Text of another length, with other
/// digits, in capitals, with a sign or with a character that splits a pair is none; an unknown
/// kind has no store kind.
#[test]
fn an_allocation_id_is_32_lowercase_hex_digits_and_a_kind_is_start_or_stop() {
    let id = AllocationId::from_bits([0xab; 16]);
    assert_eq!(allocation_id(&id.to_string()), Some(id));
    for text in [
        "",
        "abab",
        &"ab".repeat(17),
        &"zz".repeat(16),
        &"AB".repeat(16),
        &format!("+f{}", "ab".repeat(15)),
        &format!("\u{e9}{}", "a".repeat(30)),
    ] {
        assert_eq!(allocation_id(text), None, "{text:?}");
    }
    assert_eq!(kind_of(WireKind::Start), Some(DispatchKind::Start));
    assert_eq!(kind_of(WireKind::Stop), Some(DispatchKind::Stop));
    assert_eq!(kind_of(WireKind::Unknown), None);
}

/// Catches a polling host that is sent through the heartbeat what the queue hands out (a start
/// twice over), and a host that does not poll that is sent less than it was. The beat of a host
/// that polls leaves out a start and a stop the queue carries, and carries a start of a job
/// placed with no queued message (the release before), a stop the queue no longer carries (its
/// acknowledgement deleted it; the job's end waits for the agent) and the stop of a job the
/// agent lists and the store holds no allocation of; the beat of a host that does not poll
/// carries the starts and stops it always did.
#[tokio::test]
async fn the_beat_of_a_polling_host_leaves_out_what_the_queue_carries_and_no_other_host_does() {
    let mut rig = server("dispatch_beat", "server-a");
    rig.register("host-b", true);
    let (polls, plain) = (FakeAgent::new("host-a"), FakeAgent::new("host-b"));
    rig.place("job-queued", "host-a");
    rig.place("job-plain", "host-b");
    let reply = rig.beat(&polls).await.unwrap();
    assert!(starts(&reply).is_empty(), "the queue carries it: {reply:?}");
    let reply = rig.beat(&plain).await.unwrap();
    let sent: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(sent, ["job-plain"], "a host that does not poll is sent it");

    // A job placed by the release before: its start was never queued.
    let old = rig.place("job-old", "host-a");
    let handed = polled(&rig, Vec::new()).await.unwrap();
    assert_eq!(handed.messages.len(), 2);
    let old_start = handed
        .messages
        .iter()
        .find(|message| message.job_id == "job-old")
        .unwrap()
        .ack();
    polled(&rig, vec![old_start]).await.unwrap();
    let reply = rig.beat(&polls).await.unwrap();
    let sent: Vec<String> = starts(&reply).into_iter().map(|s| s.job_id).collect();
    assert_eq!(sent, ["job-old"], "no message carries it, so the beat does");

    // A cancel: the queue carries the stop until the agent acknowledges it.
    let queued_job = JobId::parse("job-queued").unwrap();
    rig.jobs.request_cancel(&queued_job).unwrap();
    rig.jobs.request_cancel(&old).unwrap();
    let reply = rig.beat(&polls).await.unwrap();
    assert_eq!(
        stops(&reply),
        Vec::<String>::new(),
        "the queue carries both stops"
    );
    let handed = polled(&rig, Vec::new()).await.unwrap();
    assert_eq!(handed.messages.len(), 2);
    polled(&rig, acks_of(&handed)).await.unwrap();
    let reply = rig.beat(&polls).await.unwrap();
    assert_eq!(stops(&reply), ["job-old", "job-queued"], "{reply:?}");

    // A job the agent lists that the store holds no allocation of is stopped by the beat.
    let mut ghost = FakeAgent::new("host-a");
    ghost.hold("job-ghost", "alloc-x", HeldState::Running);
    let reply = rig.beat(&ghost).await.unwrap();
    assert!(stops(&reply).contains(&"job-ghost".to_owned()), "{reply:?}");
}

/// The hello body of `host` in the pool of `rig`, reporting `capabilities`, naming the launched
/// instance `instance` when given (a one-time token's hello does).
fn hello_of(
    rig: &Rig,
    host: &str,
    capabilities: &serde_json::Value,
    instance: Option<&str>,
) -> Bytes {
    let mut hello = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": rig.shape(),
        "capabilities": capabilities,
        "phase": "serving",
    });
    if let Some(instance) = instance {
        hello["instance"] = json!({ "provider": "lambdalabs", "instance_id": instance });
    }
    Bytes::from(hello.to_string())
}

/// The poll of `host` in the session `welcome` opened.
fn poll_in(host: &str, welcome: &js_agent_wire::Welcome) -> DispatchPoll {
    DispatchPoll {
        host_id: host.to_owned(),
        epoch: welcome.epoch,
        session: welcome.session.clone(),
        acks: Vec::new(),
    }
}

/// Catches a welcome that says `dispatch_poll` to an agent that does not report the capability
/// or from a server that does not serve the poll to it. A server with the job tables says it to a
/// host whose hello reports `dispatch-poll`, also through a one-time token (and the poll then
/// works under that token), and not to a host that reports `per-job` alone.
#[tokio::test]
async fn a_server_with_the_tables_says_dispatch_poll_to_a_host_that_reports_it() {
    let rig = clocked_rig("dispatch_welcome", false, JobLimits::default());
    let rig = timed(rig, Duration::ZERO, NEVER);
    let instance = InstanceIdentity {
        provider: "lambdalabs".to_owned(),
        instance_id: "i-1".to_owned(),
    };
    let ttl = Duration::from_secs(600);
    let minted = rig.agents.joins.mint(instance, ttl).unwrap();
    let token = Credential::of_token(&minted.token);
    let body = hello_of(&rig, "host-a", &polling(), Some("i-1"));
    let hello = serve_hello(
        &rig.agents,
        token.clone(),
        Some(token.clone()),
        Ok(body),
        &|| (),
    );
    let Json(welcome) = hello.await.unwrap();
    assert!(welcome.dispatch_poll);
    let poll = poll_in("host-a", &welcome);
    let served = sent_as(&rig, &poll, token.clone(), Some(token)).await;
    assert_eq!(served.unwrap(), DispatchReply::default());

    let plain = json!({ "per-job": "true", "arch": "x86_64" });
    let body = hello_of(&rig, "host-b", &plain, None);
    let hello = serve_hello(&rig.agents, rig.who.clone(), None, Ok(body), &|| ());
    let Json(welcome) = hello.await.unwrap();
    assert!(!welcome.dispatch_poll);
}

/// Catches a server without the job tables that says `dispatch_poll` (the agent would poll a
/// queue it cannot read) or that serves the poll. Its welcome to a host that reports the
/// capability does not say it, and the host's poll is refused 409 `dispatch-not-served`, so the
/// agent takes its starts in the heartbeat's reply.
#[tokio::test]
async fn a_server_without_the_tables_never_says_dispatch_poll_and_refuses_the_poll() {
    let rig = untabled("dispatch_welcome_none", false);
    let body = hello_of(&rig, "host-c", &polling(), None);
    let hello = serve_hello(&rig.agents, rig.who.clone(), None, Ok(body), &|| ());
    let Json(welcome) = hello.await.unwrap();
    assert!(!welcome.dispatch_poll);
    let poll = poll_in("host-c", &welcome);
    let refused = sent_as(&rig, &poll, rig.who.clone(), None)
        .await
        .unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (409, "dispatch-not-served")
    );
}

/// The poll `sent` as the credential `who`, with the one-time token `one_time` when given.
async fn sent_as(
    rig: &Rig,
    sent: &DispatchPoll,
    who: Credential,
    one_time: Option<Credential>,
) -> Result<DispatchReply, Refused> {
    let agents = State(Arc::clone(&rig.agents));
    let marks = (
        one_time.map(|presented| Extension(OneTime(presented))),
        None,
    );
    let body = Bytes::from(serde_json::to_vec(sent).unwrap());
    let answer = poll(agents, Extension(who), marks, Ok(body)).await;
    answer.map(|Json(reply)| reply)
}

/// Catches a poll route that is not mounted at the wire's path or not behind the agent routes'
/// authentication, and a reply the agent would refuse. Through the router: the poll at
/// `/v1/agent/dispatch` with the pool's token is answered 200 with a reply whose start passes the
/// check the agent makes of every message ([`Dispatched::validate`]) and names the job and the
/// allocation the store holds; the same poll without a token is 401; a `GET` is not routed.
#[tokio::test]
async fn the_route_is_mounted_behind_the_agent_authentication_and_its_reply_passes_the_agents_check()
 {
    use tower::ServiceExt;

    let rig = server("dispatch_router", "server-a");
    let job = rig.place("job-1", "host-a");
    let app = crate::agent_api::router(Arc::clone(&rig.agents));
    let request = |token: Option<String>, method: &str| {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(js_agent_wire::DISPATCH_PATH)
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let sent = axum::body::Body::from(body(&rig, "host-a", Vec::new()));
        request.body(sent).unwrap()
    };
    let token = "j".repeat(40);
    let answer = app
        .clone()
        .oneshot(request(Some(token.clone()), "POST"))
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::OK);
    let text = axum::body::to_bytes(answer.into_body(), 1 << 20)
        .await
        .unwrap();
    let reply: DispatchReply = serde_json::from_slice(&text).unwrap();
    assert!(reply.validate().is_ok());
    assert_eq!(reply.messages.len(), 1);
    assert!(reply.messages[0].validate().is_ok());
    let spec = reply.messages[0].start.as_deref().unwrap();
    assert_eq!(spec.job_id, job.to_string());
    assert_eq!(
        spec.allocation_id,
        rig.allocation(&job).allocation_id.to_string()
    );
    let anonymous = app.clone().oneshot(request(None, "POST")).await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let read = app.oneshot(request(Some(token), "GET")).await.unwrap();
    assert_eq!(read.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// Catches a poll that is served with a body that is not one, a wrong session, a stale epoch, a
/// certificate of another host, a one-time token that did not register the host and a host that
/// does not poll. Each is refused with its status and code, and nothing is claimed.
#[tokio::test]
async fn a_poll_that_is_not_the_hosts_own_is_refused() {
    let mut rig = server("dispatch_refused", "server-a");
    rig.register("host-b", true);
    rig.place("job-1", "host-a");
    let refusal = |answer: Result<DispatchReply, Refused>| {
        let refused = answer.unwrap_err();
        (refused.status.as_u16(), refused.error)
    };
    assert_eq!(
        refusal(sent(&rig, Bytes::from_static(b"{"), (None, None)).await),
        (400, "invalid-dispatch")
    );
    let unnamed = DispatchPoll {
        host_id: "UPPER".to_owned(),
        epoch: 1,
        session: String::new(),
        acks: Vec::new(),
    };
    let unnamed = sent_as(&rig, &unnamed, rig.who.clone(), None).await;
    assert_eq!(refusal(unnamed), (400, "invalid-dispatch"));

    let (epoch, session) = rig.sessions["host-a"].clone();
    let poll_of = |epoch: u64, session: &str| DispatchPoll {
        host_id: "host-a".to_owned(),
        epoch,
        session: session.to_owned(),
        acks: Vec::new(),
    };
    let wrong = sent_as(
        &rig,
        &poll_of(epoch, "other-session"),
        rig.who.clone(),
        None,
    )
    .await;
    assert_eq!(refusal(wrong), (403, "session-mismatch"));
    let stale = sent_as(&rig, &poll_of(epoch + 1, &session), rig.who.clone(), None).await;
    assert_eq!(refusal(stale), (409, "stale-epoch"));
    let stranger = DispatchPoll {
        host_id: "host-z".to_owned(),
        ..poll_of(epoch, &session)
    };
    let stranger = sent_as(&rig, &stranger, rig.who.clone(), None).await;
    assert_eq!(refusal(stranger), (409, "unknown-host"));

    let certified = Some(Extension(CertifiedHost(HostId::new("host-b").unwrap())));
    let mismatch = sent(&rig, body(&rig, "host-a", Vec::new()), (None, certified)).await;
    assert_eq!(refusal(mismatch), (403, "certificate-host-mismatch"));
    let token = Credential::of_token(&"t".repeat(40));
    let unregistered = poll_of(epoch, &session);
    let one_time = sent_as(&rig, &unregistered, rig.who.clone(), Some(token)).await;
    assert_eq!(refusal(one_time), (403, "join-token-not-admitted"));
    assert_eq!(
        refusal(polled_by(&rig, "host-b", Vec::new()).await),
        (409, "dispatch-not-served"),
        "a host that did not report the capability"
    );
    assert_eq!(queued(&rig)[0].claimed_by, None, "nothing was claimed");
}

/// Asserts that a dispatch poll that reaches another server than the one that took the host's
/// hello is served: the second server's registry has never heard the host, takes the session
/// from the host's row, judges the poll's secret against it and hands out what is due. It
/// catches a poll refused `unknown-host` on any server but the hello's (the agent would say
/// hello again there, and its next listing would be judged a first one).
#[tokio::test]
async fn a_poll_that_reaches_another_server_than_the_hello_is_served() {
    let first = server("dispatch_rows_first", "first");
    let other = whole_over(
        "dispatch_rows_second",
        (rules(), None),
        Some(&first),
        START_MS,
    );
    let other = timed(other, Duration::ZERO, NEVER);
    other.agents.dispatching.set_claimer("second".to_owned());
    let job = first.place("job-a", "host-a");
    let reply = sent(&other, body(&first, "host-a", Vec::new()), (None, None)).await;
    assert_eq!(names(&reply.unwrap()), vec![start(job.as_str())]);
    let session = other
        .agents
        .registry
        .own_host(&host())
        .map(|own| own.host.epoch);
    assert_eq!(
        session,
        Some(first.sessions["host-a"].0),
        "the row's session was taken"
    );

    // A third server whose read of the row fails refuses the poll with what the agent retries.
    let third = whole_over(
        "dispatch_rows_third",
        (rules(), None),
        Some(&first),
        START_MS,
    );
    let third = timed(third, Duration::ZERO, NEVER);
    first.store.fail_read_after(0);
    let refused = sent(&third, body(&first, "host-a", Vec::new()), (None, None)).await;
    let refused = refused.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (503, "state-write-failed")
    );
}
