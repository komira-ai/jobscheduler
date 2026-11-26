//! What the agent side counts of retries (design section 4,
//! `docs/design/job-retry-and-completion-callback.md`): `js_job_retries_total` by how the attempt
//! ended, and `js_job_attempts_exhausted_total` for an end that retries with no attempt left. A
//! real server over fake agents on one manual clock that only a test moves; the registry, the
//! server and the store read it.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use js_agent_wire::{Ack, HeldState, Outcome};
use js_jobs::{JobId, JobSpec, JobState, StartFailure};
use js_types::{HostId, RetryPolicy};

use super::*;
use crate::agent_api::job_rig::{BOOKED, FakeAgent, Rig, clocked_rig, placed, spec};
use crate::job_expiry::JobLimits;

const SECOND: Duration = Duration::from_secs(1);
const MINUTE: Duration = Duration::from_secs(60);

/// The wait before a retried job's next attempt: the policy's initial and longest wait alike.
const BACKOFF: Duration = Duration::from_secs(30);

/// How long a round of beats steps the clock: below the registry's grace (15 s).
const STEP: Duration = Duration::from_secs(10);

/// The registry's grace: a host not heard for longer is silent.
const GRACE: Duration = Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS);

/// The agent's code of a start it refused for a cause of the host.
const HOST_REFUSAL: &str = "vm-unavailable";

/// What a recorder counted: retries by outcome (`start-failed`, `host-lost`), exhausted attempts,
/// start failures by code (`no-capacity`, `start-timeout`, `exclusive-not-ready`,
/// `agent-refused`, `input-unavailable`), and lost jobs.
type Counted = ([u64; 2], u64, [u64; 5], u64);

/// A recorder the agent side of `rig` counts its events on.
fn metered(rig: &Rig) -> Arc<Recorder> {
    let recorder = Arc::new(Recorder::default());
    let meter = Meter::new(recorder.clone(), Arc::new(NoMetrics));
    rig.agents.set_meter(meter);
    recorder
}

/// What `recorder` counted.
fn counted(recorder: &Recorder) -> Counted {
    let retries = recorder.retries.each_ref();
    let failures = recorder.start_failures.each_ref();
    (
        retries.map(|count| count.load(Ordering::Relaxed)),
        recorder.attempts_exhausted.load(Ordering::Relaxed),
        failures.map(|count| count.load(Ordering::Relaxed)),
        recorder.lost.load(Ordering::Relaxed),
    )
}

/// A server with two hosts registered, `host-a` and `host-b`, and the agent of each.
fn hosts(name: &str) -> (Rig, FakeAgent, FakeAgent) {
    let mut rig = clocked_rig(&format!("metrics_retry_{name}"), true, JobLimits::DEFAULT);
    rig.register("host-a", true);
    rig.register("host-b", true);
    (rig, FakeAgent::new("host-a"), FakeAgent::new("host-b"))
}

/// A job that retries the ends that retry: `max_attempts` attempts, [`BACKOFF`] between.
fn retrying(max_attempts: u32) -> JobSpec {
    JobSpec {
        retry: Some(RetryPolicy {
            max_attempts,
            initial_backoff: Some(BACKOFF),
            max_backoff: Some(BACKOFF),
        }),
        ..spec()
    }
}

/// Records the job `id` with `spec` and places it on `host`.
fn start(rig: &Rig, id: &str, host: &str, spec: JobSpec) -> JobId {
    placed(&rig.jobs, id, host, spec, BOOKED).0.id
}

/// Walks the placed job `id` through the beats of `agent`: the start is taken, then the job is
/// listed as starting, or as running when `running`.
async fn walk(rig: &Rig, agent: &mut FakeAgent, id: &JobId, running: bool) {
    let reply = rig.beat(agent).await.unwrap();
    agent.take(&reply);
    if running {
        agent.run(id.as_str());
    }
    let reply = rig.beat(agent).await.unwrap();
    agent.take(&reply);
}

/// The agent refuses the start of `id` with `code` and the beat that lists the refusal is taken.
async fn refuse(rig: &Rig, agent: &mut FakeAgent, id: &JobId, code: &str) {
    let ended = (Outcome::FailedToStart, None);
    agent.end_naming(id.as_str(), ended, (Some(code), None));
    let reply = rig.beat(agent).await.unwrap();
    agent.take(&reply);
}

/// Moves the clock on by `total`, in rounds of at most [`STEP`]: each round advances the clock,
/// runs a tick, and has each of `agents` beat and take its reply.
async fn wait(rig: &Rig, total: Duration, agents: &mut [&mut FakeAgent]) {
    let mut left = total;
    while !left.is_zero() {
        let step = left.min(STEP);
        left -= step;
        rig.advance(step);
        rig.tick();
        for agent in agents.iter_mut() {
            let reply = rig.beat(agent).await.unwrap();
            agent.take(&reply);
        }
    }
}

/// Takes `host` out of the registry as the scaler does when its provider reads it gone.
fn remove(rig: &Rig, host: &str) {
    let host = HostId::new(host).unwrap();
    let registry = &rig.agents.registry;
    registry.drain(&host).unwrap();
    registry.release_host(&host).unwrap();
    registry.remove(&host).unwrap();
}

/// The host `host` says hello again, past the grace, and its first listing, from a fresh agent,
/// names no job: each starting or running job it held is not listed.
async fn hello_again(rig: &mut Rig, host: &str) {
    rig.advance(GRACE + SECOND);
    rig.register(host, true);
    let mut fresh = FakeAgent::new(host);
    let reply = rig.beat(&fresh).await.unwrap();
    fresh.take(&reply);
}

/// Asserts that a retry is counted by how its attempt ended, and nothing else: a start the agent
/// refused for a cause of its host counts one `start-failed` retry, and a host that says hello
/// again without listing the job one `host-lost` retry. Neither is counted as the end a job
/// that does not retry makes (no start failure, no lost job), and neither as an exhausted
/// attempt. Catches a retry counted as a start failure or a loss, which would make a job that
/// went on to succeed look failed, and a retry counted under the other outcome.
#[tokio::test]
async fn a_retry_counts_by_how_the_attempt_ended_and_not_as_an_end() {
    let (mut rig, mut a, mut b) = hosts("outcomes");
    let recorder = metered(&rig);
    assert_eq!(counted(&recorder), ([0, 0], 0, [0; 5], 0));

    let refused = start(&rig, "job-refused", "host-a", retrying(3));
    walk(&rig, &mut a, &refused, false).await;
    refuse(&rig, &mut a, &refused, HOST_REFUSAL).await;
    assert_eq!(rig.job(&refused).state, JobState::Queued);
    assert_eq!(counted(&recorder), ([1, 0], 0, [0; 5], 0));

    let lost = start(&rig, "job-lost", "host-b", retrying(3));
    walk(&rig, &mut b, &lost, true).await;
    assert_eq!(rig.job(&lost).state, JobState::Running);
    hello_again(&mut rig, "host-b").await;
    assert_eq!(rig.job(&lost).state, JobState::Queued);
    assert_eq!(counted(&recorder), ([1, 1], 0, [0; 5], 0));
}

/// Asserts that the end of a job whose last attempt ended in a way that retries counts the end
/// as any end and the exhausted attempts once, and counts no retry: a start refused for a cause of
/// the host at attempt 2 of 2 counts one `agent-refused` start failure and one exhausted attempt
/// beside the retry of attempt 1, and the agent listing the result again counts nothing more; a
/// host lost at attempt 1 of 1 counts one lost job and one exhausted attempt; a start that timed
/// out counts one `start-timeout` failure and one exhausted attempt for the job with no attempt
/// left and one retry, no failure and no exhausted attempt for the job with attempts left. Catches
/// an end of the last attempt that is not counted, one counted twice (once by the commit, once
/// by a resend), and an exhausted attempt counted for a retry.
#[tokio::test]
async fn the_last_attempts_end_counts_the_end_and_the_exhausted_attempts_once() {
    // A start refused at the last attempt.
    let (rig, mut a, mut b) = hosts("exhausted_start");
    let recorder = metered(&rig);
    let id = start(&rig, "job-twice", "host-a", retrying(2));
    walk(&rig, &mut a, &id, false).await;
    refuse(&rig, &mut a, &id, HOST_REFUSAL).await;
    assert_eq!(counted(&recorder), ([1, 0], 0, [0; 5], 0));
    wait(&rig, BACKOFF + 2 * STEP, &mut [&mut a, &mut b]).await;
    assert_eq!(rig.job(&id).attempt.number(), 2);
    assert_eq!(rig.allocation(&id).host.as_str(), "host-b");
    refuse(&rig, &mut b, &id, HOST_REFUSAL).await;
    let ended = JobState::FailedToStart(StartFailure::AgentRefused);
    assert_eq!(rig.job(&id).state, ended);
    assert_eq!(counted(&recorder), ([1, 0], 1, [0, 0, 0, 1, 0], 0));
    // The agent lists the result again: nothing is counted again.
    let allocation = rig.allocation(&id).allocation_id.to_string();
    b.hold(id.as_str(), &allocation, HeldState::ExitedUnacked);
    let listed = (Outcome::FailedToStart, None);
    b.end_naming(id.as_str(), listed, (Some(HOST_REFUSAL), None));
    let again = rig.beat(&b).await.unwrap();
    assert_eq!(again.job_acks[0].ack, Ack::Duplicate);
    assert_eq!(counted(&recorder), ([1, 0], 1, [0, 0, 0, 1, 0], 0));

    // A host lost at the last attempt.
    let (mut rig, mut a, _b) = hosts("exhausted_lost");
    let recorder = metered(&rig);
    let id = start(&rig, "job-once", "host-a", retrying(1));
    walk(&rig, &mut a, &id, true).await;
    hello_again(&mut rig, "host-a").await;
    assert_eq!(rig.job(&id).state, JobState::Lost);
    assert_eq!(counted(&recorder), ([0, 0], 1, [0; 5], 1));

    // A host the provider reads absent, at the last attempt and with attempts left.
    let (rig, mut a, mut b) = hosts("exhausted_absent");
    let recorder = metered(&rig);
    let out = start(&rig, "job-out", "host-a", retrying(1));
    let more = start(&rig, "job-more", "host-b", retrying(3));
    walk(&rig, &mut a, &out, true).await;
    walk(&rig, &mut b, &more, true).await;
    remove(&rig, "host-a");
    rig.tick();
    assert_eq!(rig.job(&out).state, JobState::Lost);
    assert_eq!(counted(&recorder), ([0, 0], 1, [0; 5], 1));
    remove(&rig, "host-b");
    rig.tick();
    assert_eq!(rig.job(&more).state, JobState::Queued);
    assert_eq!(counted(&recorder), ([0, 1], 1, [0; 5], 1));

    // A start that timed out, at the last attempt and with attempts left.
    let (rig, mut a, _b) = hosts("exhausted_timeout");
    let recorder = metered(&rig);
    let last = start(&rig, "job-last", "host-a", retrying(1));
    let left = start(&rig, "job-left", "host-a", retrying(3));
    walk(&rig, &mut a, &last, false).await;
    walk(&rig, &mut a, &left, false).await;
    rig.advance(15 * MINUTE + SECOND);
    rig.tick();
    let timed_out = JobState::FailedToStart(StartFailure::StartTimeout);
    assert_eq!(rig.job(&last).state, timed_out);
    assert_eq!(rig.job(&left).state, JobState::Queued);
    assert_eq!(counted(&recorder), ([1, 0], 1, [0, 1, 0, 0, 0], 0));
}

/// Asserts that an end that never retries counts no exhausted attempt: a job with no retry policy
/// whose start is refused for a cause of the host, a job with a policy whose start is refused
/// for a cause of the job (`invalid-job`) and one that exits non-zero, and a job with a policy
/// whose cancel was asked for and whose host is then lost. The start refusals are counted as
/// start failures, the lost job as lost, and no retry or exhausted attempt is counted for any.
/// Catches an exhausted attempt counted for a job that had no policy, for an end that does not
/// retry, and for a loss that a cancel kept from retrying.
#[tokio::test]
async fn an_end_that_retries_nothing_counts_no_exhausted_attempts() {
    let (mut rig, mut a, _b) = hosts("no_retry");
    let recorder = metered(&rig);

    let plain = start(&rig, "job-plain", "host-a", spec());
    walk(&rig, &mut a, &plain, false).await;
    refuse(&rig, &mut a, &plain, HOST_REFUSAL).await;
    let refused = JobState::FailedToStart(StartFailure::AgentRefused);
    assert_eq!(rig.job(&plain).state, refused);
    assert_eq!(counted(&recorder), ([0, 0], 0, [0, 0, 0, 1, 0], 0));

    let fault = start(&rig, "job-fault", "host-a", retrying(3));
    walk(&rig, &mut a, &fault, false).await;
    refuse(&rig, &mut a, &fault, "invalid-job").await;
    assert_eq!(rig.job(&fault).state, refused);
    assert_eq!(counted(&recorder), ([0, 0], 0, [0, 0, 0, 2, 0], 0));

    let exits = start(&rig, "job-exits", "host-a", retrying(3));
    walk(&rig, &mut a, &exits, true).await;
    a.end(exits.as_str(), Outcome::Failed, Some(3));
    let reply = rig.beat(&a).await.unwrap();
    a.take(&reply);
    assert_eq!(rig.job(&exits).state, JobState::Failed);
    assert_eq!(counted(&recorder), ([0, 0], 0, [0, 0, 0, 2, 0], 0));

    let cancelled = start(&rig, "job-cancelled", "host-a", retrying(3));
    walk(&rig, &mut a, &cancelled, true).await;
    rig.jobs.request_cancel(&cancelled).unwrap();
    hello_again(&mut rig, "host-a").await;
    assert_eq!(rig.job(&cancelled).state, JobState::Lost);
    assert_eq!(counted(&recorder), ([0, 0], 0, [0, 0, 0, 2, 0], 1));
}
