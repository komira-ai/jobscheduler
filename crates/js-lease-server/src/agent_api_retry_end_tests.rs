//! How an attempt ends and what follows it, end to end through the route and the job pass (design
//! section 9, `docs/design/job-retry-and-completion-callback.md`): a server that places with
//! `js-sched`, hosts that register and beat, and fake agents, all on one manual clock that only a
//! test moves ([`Rig::advance`]). They cover every end that retries and the ends that never do
//! (a job's own fault, a non-zero exit, a cancel), a result the host lists again after its
//! attempt was retried, the stop a host that still lists an earlier attempt is sent, and the
//! attempts a job's view and its usage history keep.

use std::num::NonZeroUsize;
use std::time::Duration;

use js_agent_wire::{Ack, HeartbeatReply, HeldState, Outcome};
use js_jobs::{AllocationStep, EndReason, JobId, JobSpec, JobState, StartFailure};
use js_proto::v1;
use js_registry::WholeNodeConfig;
use js_types::{HostId, RetryPolicy};
use js_usage::EndedHistory;
use serde_json::json;

use super::job_rig::{BOOKED, FakeAgent, Rig, clocked_rig, placed, spec, starts, stops};
use crate::job_api::read::view;
use crate::job_expiry::JobLimits;
use crate::job_loss::{Observe, Seen};
use crate::job_pass::{run, store_time};
use crate::metrics::NoMetrics;

const SECOND: Duration = Duration::from_secs(1);
const MINUTE: Duration = Duration::from_secs(60);

/// The wait before a retried job's next attempt: the policy's initial and longest wait alike,
/// so that no jitter shows.
const BACKOFF: Duration = Duration::from_secs(30);

/// How long a round of beats steps the clock: below the registry's grace (15 s), so a host that
/// beats each round is never silent.
const STEP: Duration = Duration::from_secs(10);

/// The registry's grace: a host not heard for longer is silent.
const GRACE: Duration = Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS);

/// The agent's code of a start it refused for a cause of the host (not of the job, and one no
/// server rule reads: a code of the job's own fault ends it, `host-held` and the like requeue it).
const HOST_REFUSAL: &str = "vm-unavailable";

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

/// Records the job `id` with [`retrying`] and places it on `host`.
fn start_on(rig: &Rig, id: &str, host: &str, max_attempts: u32) -> JobId {
    placed(&rig.jobs, id, host, retrying(max_attempts), BOOKED)
        .0
        .id
}

/// A server with two hosts registered, `host-a` and `host-b`, and the agent of each.
fn two_hosts(name: &str) -> (Rig, FakeAgent, FakeAgent) {
    let mut rig = clocked_rig(&format!("retry_end_{name}"), true, JobLimits::DEFAULT);
    rig.register("host-a", true);
    rig.register("host-b", true);
    (rig, FakeAgent::new("host-a"), FakeAgent::new("host-b"))
}

/// The milliseconds of the rig's clock now.
fn now_ms(rig: &Rig) -> u64 {
    js_types::Clock::now(&rig.shared).millis()
}

/// One heartbeat of `agent`, and its reply taken.
async fn beat(rig: &Rig, agent: &mut FakeAgent) {
    let reply = rig.beat(agent).await.unwrap();
    agent.take(&reply);
}

/// Moves the clock on by `total`, in rounds of at most [`STEP`]: each round advances the clock,
/// runs a tick, has each of `deaf` beat (their replies ignored: a host that holds on to what it
/// has) and each of `listening` beat and take its reply, so no host that beats is silent.
async fn pass_time(
    rig: &Rig,
    total: Duration,
    listening: &mut [&mut FakeAgent],
    deaf: &[&FakeAgent],
) {
    let mut left = total;
    while !left.is_zero() {
        let step = left.min(STEP);
        left -= step;
        rig.advance(step);
        rig.tick();
        for agent in deaf {
            rig.beat(agent).await.unwrap();
        }
        for agent in listening.iter_mut() {
            let reply = rig.beat(agent).await.unwrap();
            agent.take(&reply);
        }
    }
}

/// [`pass_time`] over every agent of `farm`, each listening.
async fn pass_time_all(rig: &Rig, total: Duration, farm: &mut [FakeAgent]) {
    let mut listening: Vec<&mut FakeAgent> = farm.iter_mut().collect();
    pass_time(rig, total, &mut listening, &[]).await;
}

/// Walks the placed job `id` through the beats of `agent`: the start is taken, then the job is
/// listed as starting, or as running when `running`.
async fn walk(rig: &Rig, agent: &mut FakeAgent, id: &JobId, running: bool) {
    beat(rig, agent).await;
    if running {
        agent.run(id.as_str());
    }
    beat(rig, agent).await;
}

/// The agent refuses the start of `id` with `code`, and the beat that lists the refusal is
/// taken; the reply.
async fn refuse(rig: &Rig, agent: &mut FakeAgent, id: &JobId, code: &str) -> HeartbeatReply {
    let ended = (Outcome::FailedToStart, None);
    agent.end_naming(id.as_str(), ended, (Some(code), None));
    let reply = rig.beat(agent).await.unwrap();
    agent.take(&reply);
    reply
}

/// The verdicts a reply gives on the results listed, as job id and verdict.
fn acks(reply: &HeartbeatReply) -> Vec<(String, Ack)> {
    let each = reply.job_acks.iter();
    each.map(|ack| (ack.job_id.clone(), ack.ack)).collect()
}

/// What the provider reads of the hosts: the host named is absent, every other one present.
struct Absent(&'static str);

impl Observe for Absent {
    fn observe(&self, host: &HostId) -> Result<Seen, String> {
        let seen = match host.as_str() == self.0 {
            true => Seen::Absent,
            false => Seen::Present,
        };
        Ok(seen)
    }
}

/// One job pass over the rig's tables, at its clock, the provider reading `gone` absent; the
/// jobs it lost.
fn pass_reading_absent(rig: &Rig, gone: &'static str) -> Vec<JobId> {
    let rules = WholeNodeConfig::dedicated(1);
    let provider = Absent(gone);
    let now = store_time(now_ms(rig));
    let passed = run(
        &rig.jobs,
        (JobLimits::DEFAULT, &rules),
        (&provider, &NoMetrics),
        now,
    );
    passed.unwrap().lost
}

/// Asserts that the job `id` ended as `state`, still at its first attempt, with exactly one
/// attempt in its history that ended as `outcome` and was `retried` or not, and no wait for a
/// next attempt: what the job's view shows of a job that no retry followed (or that was
/// cancelled while it waited for one).
fn assert_ended_once(
    rig: &Rig,
    id: &JobId,
    state: JobState,
    (outcome, retried): (v1::AttemptOutcome, bool),
) {
    assert_eq!(rig.job(id).state, state);
    let shown = view::job(&rig.job(id));
    assert_eq!(shown.attempt, 1);
    assert_eq!(shown.retry_not_before_ms, None);
    assert_eq!(
        shown.attempt_history.len(),
        1,
        "{:?}",
        shown.attempt_history
    );
    let entry = &shown.attempt_history[0];
    assert_eq!(
        (entry.number, entry.outcome, entry.retried),
        (1, i32::from(outcome), retried)
    );
}

/// One way an attempt ends that retries.
#[derive(Debug, Clone, Copy)]
enum Way {
    /// The start did not complete within the start timeout.
    StartTimeout,
    /// The agent refused the start with `input-unavailable`.
    InputUnavailable,
    /// The agent refused the start with a code of its host.
    HostCode,
    /// The agent refused the start with a code this server does not know.
    UnknownCode,
    /// The provider reads the host absent.
    ProviderAbsent,
    /// The host said hello again and its first listing omitted the running job.
    Unlisted,
}

/// When the attempt `entry` of a job's history ended, in milliseconds on the rig's clock. The wait
/// before the next attempt is counted from this instant, the store's reading in the retry's
/// own commit: each commit of the fake store moves the clock on by its sync cost, so a reading
/// of the clock taken after the retry is later.
fn ended_ms(entry: &v1::Attempt) -> u64 {
    let times = entry.times.as_ref();
    times.and_then(|times| times.ended_at_ms).unwrap()
}

/// What the view of a job retried after `way` shows of its ended attempt, and why its allocation
/// was freed: the attempt's outcome, its start failure, the agent's code, and the end reason.
fn expected(
    way: Way,
) -> (
    v1::AttemptOutcome,
    v1::StartFailureCode,
    &'static str,
    EndReason,
) {
    use v1::AttemptOutcome::{HostLost, StartFailed};
    use v1::StartFailureCode as Code;
    match way {
        Way::StartTimeout => (StartFailed, Code::StartTimeout, "", EndReason::StartTimeout),
        Way::InputUnavailable => (
            StartFailed,
            Code::InputUnavailable,
            "input-unavailable",
            EndReason::Refused,
        ),
        Way::HostCode => (
            StartFailed,
            Code::AgentRefused,
            HOST_REFUSAL,
            EndReason::Refused,
        ),
        Way::UnknownCode => (
            StartFailed,
            Code::AgentRefused,
            "a-code-from-a-later-agent",
            EndReason::Refused,
        ),
        Way::ProviderAbsent | Way::Unlisted => {
            (HostLost, Code::Unspecified, "", EndReason::HostAbsent)
        }
    }
}

/// Plays one attempt of a job with three attempts that ends on `host-a` as `way` says, and
/// asserts what follows: the job is queued with the attempt in its history, its allocation is
/// freed for the reason of the end, and, once the backoff has passed, it is placed again, on the
/// other host, as attempt 2.
async fn second_attempt(way: Way) {
    let (outcome, failure, detail, freed_for) = expected(way);
    let (mut rig, mut a, mut b) = two_hosts(&format!("second_{way:?}"));
    let running = matches!(way, Way::ProviderAbsent | Way::Unlisted);
    let id = start_on(&rig, "job-r", "host-a", 3);
    walk(&rig, &mut a, &id, running).await;
    let live = match running {
        true => JobState::Running,
        false => JobState::Starting,
    };
    assert_eq!(rig.states(&id), (live, AllocationStep::Booked), "{way:?}");

    match way {
        Way::StartTimeout => {
            pass_time(&rig, 14 * MINUTE, &mut [&mut a, &mut b], &[]).await;
            rig.advance(MINUTE + SECOND);
            rig.tick();
        }
        Way::InputUnavailable => {
            refuse(&rig, &mut a, &id, "input-unavailable").await;
        }
        Way::HostCode => {
            refuse(&rig, &mut a, &id, HOST_REFUSAL).await;
        }
        Way::UnknownCode => {
            refuse(&rig, &mut a, &id, "a-code-from-a-later-agent").await;
        }
        Way::ProviderAbsent => {
            let lost = pass_reading_absent(&rig, "host-a");
            assert!(lost.is_empty(), "a retried job is not lost: {lost:?}");
            a = FakeAgent::new("host-a");
        }
        Way::Unlisted => {
            rig.advance(GRACE + SECOND);
            rig.register("host-a", true);
            a = FakeAgent::new("host-a");
            beat(&rig, &mut a).await;
        }
    }

    // The job is queued for its next attempt, which its view shows, with the attempt that ended.
    let queued = rig.job(&id);
    assert_eq!(queued.state, JobState::Queued, "{way:?}");
    let shown = view::job(&queued);
    assert_eq!(shown.state, i32::from(v1::JobState::Queued), "{way:?}");
    assert_eq!(shown.attempt, 1, "{way:?}");
    assert_eq!(shown.attempt_history.len(), 1, "{way:?}");
    let ended = &shown.attempt_history[0];
    assert_eq!(
        shown.retry_not_before_ms,
        Some(ended_ms(ended) + 30_000),
        "{way:?}"
    );
    assert_eq!(
        (
            ended.number,
            ended.host_id.as_str(),
            ended.outcome,
            ended.start_failure,
            ended.start_failure_detail.as_str(),
            ended.retried,
        ),
        (
            1,
            "host-a",
            i32::from(outcome),
            i32::from(failure),
            detail,
            true
        ),
        "{way:?}"
    );

    // Its allocation is freed for the reason the attempt ended: at once when no container is to
    // be confirmed gone, else when the host stops listing the job.
    match way {
        Way::StartTimeout => {
            assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);
            let told = rig.beat(&a).await.unwrap();
            assert_eq!(stops(&told), [id.to_string()], "the host is told to stop");
            a.take(&told);
            beat(&rig, &mut a).await;
            beat(&rig, &mut a).await;
        }
        Way::InputUnavailable | Way::HostCode | Way::UnknownCode => {
            assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);
            beat(&rig, &mut a).await;
        }
        Way::ProviderAbsent | Way::Unlisted => {}
    }
    let freed = rig.allocation(&id);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(freed_for)),
        "{way:?}"
    );
    assert_eq!(rig.job(&id).state, JobState::Queued, "{way:?}");

    // Once its backoff has passed it is placed again, on the host that did not hold attempt 1.
    pass_time(&rig, BACKOFF + STEP, &mut [&mut a, &mut b], &[]).await;
    let again = rig.job(&id);
    assert!(
        matches!(again.state, JobState::Placed | JobState::Starting),
        "{way:?}: {:?}",
        again.state
    );
    assert_eq!(again.attempt.number(), 2, "{way:?}");
    assert_eq!(rig.allocation(&id).host.as_str(), "host-b", "{way:?}");
    let shown = view::job(&again);
    assert_eq!(shown.attempt, 2, "{way:?}");
    assert_eq!(shown.attempt_history.len(), 1, "{way:?}");
    assert_eq!(
        shown.retry_not_before_ms, None,
        "a placed job waits for nothing"
    );
}

/// Asserts that each end that retries makes a second attempt: a start that timed out, a start
/// the agent refused with `input-unavailable`, with a code of its host and with a code this
/// server does not know, a host the provider reads absent (the job is queued, not lost, and its
/// allocation freed in the same pass for the host's absence), and a host that says hello again
/// and does not list the job it ran. Each leaves the job queued with one entry in its attempt
/// history (outcome, host, retried), its allocation freed for the reason of the end once
/// the host confirms (or at once for an absent host), and places it again as attempt 2 once the
/// backoff has passed. Catches an end that still ends the job, a retry placed before its backoff
/// or on the host that failed, an attempt that is not counted or not kept in the history, and an
/// allocation freed for the wrong reason or never.
#[tokio::test]
async fn each_retrying_outcome_makes_a_second_attempt() {
    for way in [
        Way::StartTimeout,
        Way::InputUnavailable,
        Way::HostCode,
        Way::UnknownCode,
        Way::ProviderAbsent,
        Way::Unlisted,
    ] {
        second_attempt(way).await;
    }
}

/// Asserts that the ends that are not the host's fault end the job although attempts are left: a
/// refusal with a code that names the job's own fault (`invalid-job`), a non-zero exit, and a
/// start refused for a cause of the host when the policy has no attempt after the first. Each
/// ends the job at its first attempt, with one history entry not marked retried and no wait.
/// Catches a job's own failure retried on host after host, and a policy's attempt count that is
/// not honoured.
#[tokio::test]
async fn a_job_fault_a_non_zero_exit_and_the_last_attempt_end_the_job() {
    let (rig, mut a, _b) = two_hosts("ends");
    let fault = start_on(&rig, "job-fault", "host-a", 3);
    walk(&rig, &mut a, &fault, false).await;
    refuse(&rig, &mut a, &fault, "invalid-job").await;
    let refused = JobState::FailedToStart(StartFailure::AgentRefused);
    assert_ended_once(
        &rig,
        &fault,
        refused,
        (v1::AttemptOutcome::StartFailed, false),
    );
    let entry = view::job(&rig.job(&fault)).attempt_history[0].clone();
    assert_eq!(entry.start_failure_detail, "invalid-job");
    assert_eq!(rig.allocation(&fault).step, AllocationStep::Releasing);

    let exits = start_on(&rig, "job-exits", "host-a", 3);
    walk(&rig, &mut a, &exits, true).await;
    a.end(exits.as_str(), Outcome::Failed, Some(3));
    beat(&rig, &mut a).await;
    let outcome = (v1::AttemptOutcome::Failed, false);
    assert_ended_once(&rig, &exits, JobState::Failed, outcome);
    let entry = view::job(&rig.job(&exits)).attempt_history[0].clone();
    assert_eq!(entry.exit_code, Some(3));

    let last = start_on(&rig, "job-last", "host-a", 1);
    walk(&rig, &mut a, &last, false).await;
    refuse(&rig, &mut a, &last, HOST_REFUSAL).await;
    let outcome = (v1::AttemptOutcome::StartFailed, false);
    assert_ended_once(&rig, &last, refused, outcome);

    // None of them is placed again, whatever time passes.
    let (mut a, mut b) = (FakeAgent::new("host-a"), FakeAgent::new("host-b"));
    pass_time(&rig, 2 * MINUTE, &mut [&mut a, &mut b], &[]).await;
    for id in [&fault, &exits, &last] {
        assert_eq!(rig.job(id).attempt.number(), 1);
        assert!(rig.job(id).state.is_ended());
    }
}

/// Asserts that a cancel is never retried: a cancel asked for while the job ran, then the
/// provider reads its host absent (the job is lost), the host says hello again without it (lost
/// too), its start timed out (failed to start) or the agent reports it killed (cancelled), ends
/// the job at its first attempt with one history entry not marked retried and never queues
/// another attempt; and a cancel asked for while the job waits for its retry ends it cancelled
/// at once, its earlier attempt kept in the history as retried, and it is never placed again.
/// Catches a cancelled job queued again because its host was lost, and a cancel of a waiting job
/// that leaves it for the next placement.
#[tokio::test]
async fn a_cancelled_attempt_never_retries() {
    use v1::AttemptOutcome::{Cancelled, HostLost, StartFailed};

    // The provider reads the host absent.
    let (rig, mut a, _b) = two_hosts("cancel_absent");
    let id = start_on(&rig, "job-c1", "host-a", 3);
    walk(&rig, &mut a, &id, true).await;
    rig.jobs.request_cancel(&id).unwrap();
    assert_eq!(pass_reading_absent(&rig, "host-a"), [id.clone()]);
    assert_ended_once(&rig, &id, JobState::Lost, (HostLost, false));
    let freed = rig.allocation(&id);
    assert_eq!(
        (freed.step, freed.end_reason),
        (AllocationStep::Freed, Some(EndReason::HostAbsent))
    );

    // The host says hello again and does not list the job.
    let (mut rig, mut a, _b) = two_hosts("cancel_unlisted");
    let id = start_on(&rig, "job-c2", "host-a", 3);
    walk(&rig, &mut a, &id, true).await;
    rig.jobs.request_cancel(&id).unwrap();
    rig.advance(GRACE + SECOND);
    rig.register("host-a", true);
    let mut again = FakeAgent::new("host-a");
    beat(&rig, &mut again).await;
    assert_ended_once(&rig, &id, JobState::Lost, (HostLost, false));

    // The start times out: the host holds on to the job and the cancel is not carried out.
    let (rig, mut a, _b) = two_hosts("cancel_start_timeout");
    let id = start_on(&rig, "job-c3", "host-a", 3);
    walk(&rig, &mut a, &id, false).await;
    rig.jobs.request_cancel(&id).unwrap();
    pass_time(&rig, 14 * MINUTE, &mut [], &[&a]).await;
    rig.advance(MINUTE + SECOND);
    rig.tick();
    let timed_out = JobState::FailedToStart(StartFailure::StartTimeout);
    assert_ended_once(&rig, &id, timed_out, (StartFailed, false));
    assert!(rig.job(&id).cancel_requested);
    assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);

    // The agent stops the job and reports it killed.
    let (rig, mut a, _b) = two_hosts("cancel_killed");
    let id = start_on(&rig, "job-c4", "host-a", 3);
    walk(&rig, &mut a, &id, true).await;
    rig.jobs.request_cancel(&id).unwrap();
    let told = rig.beat(&a).await.unwrap();
    assert_eq!(stops(&told), [id.to_string()]);
    a.take(&told);
    let reported = rig.beat(&a).await.unwrap();
    assert_eq!(acks(&reported), [(id.to_string(), Ack::Accepted)]);
    assert_ended_once(&rig, &id, JobState::Cancelled, (Cancelled, false));

    // A cancel while the job waits for its retry ends it at once.
    let (rig, mut a, mut b) = two_hosts("cancel_backoff");
    let id = start_on(&rig, "job-c5", "host-a", 3);
    walk(&rig, &mut a, &id, false).await;
    refuse(&rig, &mut a, &id, HOST_REFUSAL).await;
    assert_eq!(rig.job(&id).state, JobState::Queued);
    let cancelled = rig.jobs.request_cancel(&id).unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert_ended_once(&rig, &id, JobState::Cancelled, (StartFailed, true));
    // The allocation of the attempt that ended is still freed, by its host's confirmation.
    assert_eq!(rig.allocation(&id).step, AllocationStep::Releasing);
    pass_time(&rig, BACKOFF + STEP, &mut [&mut a, &mut b], &[]).await;
    assert_eq!(rig.allocation(&id).step, AllocationStep::Freed);
    assert_ended_once(&rig, &id, JobState::Cancelled, (StartFailed, true));
    assert!(!b.holds(id.as_str()), "the cancelled job was started again");
}

/// Asserts that a result listed after its attempt was retried is stale: after a start refused
/// for a cause of the host queued the job again, the agent (which never got the verdict) lists
/// the same result under the same allocation again, and the verdict is `Stale` with the job and
/// its allocation unchanged and one history entry; and, once the job is placed on another host,
/// the old host listing that result again is answered `Stale` too and moves nothing. Catches a
/// resent result that ends a job already queued for its next attempt, one that is answered
/// accepted twice, and a late result of an earlier attempt that disturbs the later one.
#[tokio::test]
async fn a_result_resent_after_a_retry_is_stale() {
    let (rig, mut a, mut b) = two_hosts("resent");
    let id = start_on(&rig, "job-resent", "host-a", 3);
    walk(&rig, &mut a, &id, false).await;
    let first_allocation = rig.allocation(&id).allocation_id.to_string();
    let ended = (Outcome::FailedToStart, None);
    a.end_naming(id.as_str(), ended, (Some(HOST_REFUSAL), None));

    // The reply with the verdict is lost on its way: the agent keeps the result.
    let first = rig.beat(&a).await.unwrap();
    assert_eq!(acks(&first), [(id.to_string(), Ack::Accepted)]);
    let queued = rig.job(&id);
    assert_eq!(
        rig.states(&id),
        (JobState::Queued, AllocationStep::Releasing)
    );

    let second = rig.beat(&a).await.unwrap();
    assert_eq!(acks(&second), [(id.to_string(), Ack::Stale)]);
    assert_eq!(rig.job(&id), queued);
    assert_eq!(
        rig.states(&id),
        (JobState::Queued, AllocationStep::Releasing)
    );
    assert_eq!(view::job(&rig.job(&id)).attempt_history.len(), 1);

    // The verdict arrives this time; the host stops listing the job and its allocation is freed;
    // the job is placed on the other host.
    a.take(&second);
    pass_time(&rig, BACKOFF + STEP, &mut [&mut a, &mut b], &[]).await;
    assert_eq!(rig.allocation(&id).host.as_str(), "host-b");
    assert_eq!(rig.job(&id).attempt.number(), 2);

    // The old host lists its attempt's result once more, long after.
    a.hold(id.as_str(), &first_allocation, HeldState::ExitedUnacked);
    a.end_naming(id.as_str(), ended, (Some(HOST_REFUSAL), None));
    let before = rig.job(&id);
    let late = rig.beat(&a).await.unwrap();
    assert_eq!(acks(&late), [(id.to_string(), Ack::Stale)]);
    assert_eq!(rig.job(&id), before);
    assert_eq!(rig.allocation(&id).host.as_str(), "host-b");
    assert_eq!(view::job(&rig.job(&id)).attempt_history.len(), 1);
}

/// Asserts that a stop for an earlier attempt never retires a later one. A host that reports
/// `start-fence` and still lists attempt 1 (its start timed out and the job was queued again) is
/// told to stop the job; once its allocation is freed and the host's 15-minute bar has passed,
/// the job is placed on it again as attempt 2, and the host is sent attempt 2's start under its
/// new allocation and no stop for the job while it holds that allocation. A host that still
/// lists an earlier attempt (the provider read it absent, the job was placed elsewhere) is
/// told to stop the job, and only that host: the host that holds attempt 2 is sent its start and
/// no stop, and the old host's late result of attempt 1 is answered stale and moves nothing.
/// Catches a stop sent to the host that holds the later attempt, a stop for the earlier attempt
/// that is not sent, and a late start that is sent under the old allocation.
#[tokio::test]
async fn a_stop_for_an_earlier_attempt_never_retires_a_later_one() {
    // The same host takes the next attempt: it reports start-fence, and its bar has passed.
    let mut rig = clocked_rig("retry_end_stop_fenced", true, JobLimits::DEFAULT);
    let capabilities = json!({ "per-job": "true", "arch": "x86_64", "start-fence": "true" });
    rig.register_with("host-a", capabilities);
    let mut a = FakeAgent::new("host-a");
    let id = start_on(&rig, "job-fence", "host-a", 3);
    walk(&rig, &mut a, &id, false).await;
    let first = rig.allocation(&id).allocation_id.to_string();
    pass_time(&rig, 14 * MINUTE, &mut [&mut a], &[]).await;
    rig.advance(MINUTE + SECOND);
    rig.tick();
    assert_eq!(
        rig.states(&id),
        (JobState::Queued, AllocationStep::Releasing)
    );

    let told = rig.beat(&a).await.unwrap();
    assert_eq!(stops(&told), [id.to_string()]);
    assert!(starts(&told).is_empty());
    a.take(&told);
    beat(&rig, &mut a).await;
    beat(&rig, &mut a).await;
    assert_eq!(rig.allocation(&id).step, AllocationStep::Freed);

    // The host is barred for 15 minutes from the retry, though it reports start-fence.
    pass_time(&rig, 10 * MINUTE, &mut [], &[&a]).await;
    assert_eq!(rig.job(&id).state, JobState::Queued);
    pass_time(&rig, 6 * MINUTE, &mut [], &[&a]).await;
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    let second = rig.allocation(&id);
    assert_eq!(second.host.as_str(), "host-a");
    assert_ne!(second.allocation_id.to_string(), first);
    assert_eq!(rig.job(&id).attempt.number(), 2);

    let reply = rig.beat(&a).await.unwrap();
    let sent: Vec<(String, String)> = starts(&reply)
        .into_iter()
        .map(|start| (start.job_id, start.allocation_id))
        .collect();
    assert_eq!(sent, [(id.to_string(), second.allocation_id.to_string())]);
    assert!(stops(&reply).is_empty(), "{reply:?}");
    a.take(&reply);
    for _ in 0..2 {
        let reply = rig.beat(&a).await.unwrap();
        assert!(stops(&reply).is_empty(), "{reply:?}");
        assert!(starts(&reply).is_empty(), "{reply:?}");
        a.take(&reply);
    }
    assert_eq!(
        rig.states(&id),
        (JobState::Starting, AllocationStep::Booked)
    );

    // An earlier attempt on another host: stopped there, and only there.
    let (rig, mut a, mut b) = two_hosts("stop_other");
    let id = start_on(&rig, "job-moved", "host-a", 3);
    walk(&rig, &mut a, &id, true).await;
    let lost = pass_reading_absent(&rig, "host-a");
    assert!(lost.is_empty(), "{lost:?}");
    assert_eq!(rig.states(&id), (JobState::Queued, AllocationStep::Freed));
    // Neither host takes what it is told while the clock runs to the end of the backoff, so the
    // replies below are the ones the placement made.
    pass_time(&rig, BACKOFF + STEP, &mut [], &[&a, &b]).await;
    assert_eq!(rig.states(&id), (JobState::Placed, AllocationStep::Booked));
    let second = rig.allocation(&id);
    assert_eq!(second.host.as_str(), "host-b");

    let on_new = rig.beat(&b).await.unwrap();
    let sent: Vec<String> = starts(&on_new).into_iter().map(|s| s.job_id).collect();
    assert_eq!(sent, [id.to_string()]);
    assert!(stops(&on_new).is_empty(), "{on_new:?}");
    b.take(&on_new);

    let on_old = rig.beat(&a).await.unwrap();
    assert_eq!(stops(&on_old), [id.to_string()]);
    assert!(starts(&on_old).is_empty(), "{on_old:?}");
    a.take(&on_old);
    let late = rig.beat(&a).await.unwrap();
    assert_eq!(acks(&late), [(id.to_string(), Ack::Stale)]);
    a.take(&late);

    let on_new = rig.beat(&b).await.unwrap();
    assert!(stops(&on_new).is_empty(), "{on_new:?}");
    assert!(starts(&on_new).is_empty(), "{on_new:?}");
    assert_eq!(
        rig.states(&id),
        (JobState::Starting, AllocationStep::Booked)
    );
    assert_eq!(rig.allocation(&id).allocation_id, second.allocation_id);
}

/// The agent of `host` in `farm`.
fn agent_of<'a>(farm: &'a mut [FakeAgent], host: &str) -> &'a mut FakeAgent {
    farm.iter_mut().find(|agent| agent.host == host).unwrap()
}

/// Asserts that each attempt of a job has its entry in the job's attempt history and its row in
/// the usage history. A job with three attempts has two start refusals for a cause of the host
/// and then succeeds, each on a host of its own: its view shows attempt 3 and three history
/// entries in order (numbers, hosts, outcomes, retried flags, start failure detail, exit code,
/// started time only for the attempt that ran); the first entry's queued time is the submit,
/// each later one's the end of the backoff before it, and the job's own `queued_at` stays the
/// submit; its result names attempt 3; and the usage history holds one row for each attempt's
/// allocation, with no attempt number on the first and 2 and 3 on the others. Catches an attempt
/// missing from the history or out of order, a queued time that is the job's submit for every
/// attempt, a retried flag that is wrong, and a usage row that does not name its attempt.
#[tokio::test]
async fn each_attempt_has_its_history_entry_and_usage_record() {
    use v1::AttemptOutcome::{StartFailed, Succeeded};
    let mut rig = clocked_rig("retry_end_history", true, JobLimits::DEFAULT);
    let mut farm = Vec::new();
    for host in ["host-a", "host-b", "host-c"] {
        rig.register(host, true);
        farm.push(FakeAgent::new(host));
    }
    let id = start_on(&rig, "job-thrice", "host-a", 3);
    let submitted = view::job(&rig.job(&id)).times.unwrap().queued_at_ms;
    pass_time_all(&rig, STEP, &mut farm).await;

    // Attempts 1 and 2: the host that holds the attempt refuses its start.
    for attempt in 1..=2 {
        assert_eq!(rig.job(&id).attempt.number(), attempt);
        let host = rig.job(&id).host.unwrap().to_string();
        let agent = agent_of(&mut farm, &host);
        let reply = refuse(&rig, agent, &id, HOST_REFUSAL).await;
        assert_eq!(acks(&reply), [(id.to_string(), Ack::Accepted)]);
        assert_eq!(rig.job(&id).state, JobState::Queued);
        pass_time_all(&rig, BACKOFF + STEP, &mut farm).await;
        assert_eq!(rig.allocation(&id).step, AllocationStep::Booked);
    }

    // Attempt 3 runs and succeeds.
    assert_eq!(rig.job(&id).attempt.number(), 3);
    let host = rig.job(&id).host.unwrap().to_string();
    agent_of(&mut farm, &host).run(id.as_str());
    pass_time_all(&rig, STEP, &mut farm).await;
    assert_eq!(rig.job(&id).state, JobState::Running);
    agent_of(&mut farm, &host).end(id.as_str(), Outcome::Succeeded, Some(0));
    pass_time_all(&rig, 2 * STEP, &mut farm).await;
    assert_eq!(
        rig.states(&id),
        (JobState::Succeeded, AllocationStep::Freed)
    );

    let done = rig.job(&id);
    let shown = view::job(&done);
    assert_eq!(shown.attempt, 3);
    assert_eq!(shown.times.clone().unwrap().queued_at_ms, submitted);
    assert_eq!(shown.retry_not_before_ms, None);
    assert_eq!(shown.attempt_history.len(), 3);
    assert_eq!(view::result(&done).attempt, 3);
    let history = &shown.attempt_history;
    let numbers: Vec<u32> = history.iter().map(|entry| entry.number).collect();
    assert_eq!(numbers, [1, 2, 3]);
    let outcomes: Vec<i32> = history.iter().map(|entry| entry.outcome).collect();
    let (failed, succeeded) = (i32::from(StartFailed), i32::from(Succeeded));
    assert_eq!(outcomes, [failed, failed, succeeded]);
    let retried: Vec<bool> = history.iter().map(|entry| entry.retried).collect();
    assert_eq!(retried, [true, true, false]);
    let details: Vec<&str> = history
        .iter()
        .map(|entry| entry.start_failure_detail.as_str())
        .collect();
    assert_eq!(details, [HOST_REFUSAL, HOST_REFUSAL, ""]);
    let hosts: Vec<&str> = history.iter().map(|entry| entry.host_id.as_str()).collect();
    assert_eq!(hosts[0], "host-a");
    assert_eq!(hosts[2], host);
    assert!(
        hosts[0] != hosts[1] && hosts[1] != hosts[2] && hosts[0] != hosts[2],
        "each attempt ran on a host of its own: {hosts:?}"
    );
    assert_eq!(history[2].exit_code, Some(0));
    let times: Vec<v1::JobTimes> = history
        .iter()
        .map(|entry| entry.times.clone().unwrap())
        .collect();
    let queued: Vec<u64> = times.iter().map(|times| times.queued_at_ms).collect();
    let (first_wait, second_wait) = (ended_ms(&history[0]), ended_ms(&history[1]));
    assert_eq!(
        queued,
        [submitted, first_wait + 30_000, second_wait + 30_000]
    );
    let started: Vec<bool> = times.iter().map(|t| t.started_at_ms.is_some()).collect();
    assert_eq!(started, [false, false, true]);
    for (attempt, times) in times.iter().enumerate() {
        let placed = times.placed_at_ms.unwrap();
        assert!(times.queued_at_ms <= placed, "attempt {}", attempt + 1);
        assert!(
            placed <= times.ended_at_ms.unwrap(),
            "attempt {}",
            attempt + 1
        );
    }
    assert!(times[0].ended_at_ms.unwrap() < times[1].queued_at_ms);
    assert!(times[1].ended_at_ms.unwrap() < times[2].queued_at_ms);

    // One usage row for each attempt's allocation, oldest first.
    let limit = NonZeroUsize::new(100).unwrap();
    let rows = EndedHistory::new(&*rig.store).range(0, u64::MAX, limit);
    let rows = rows.unwrap().rows;
    let attempts: Vec<Option<u32>> = rows.iter().map(|row| row.attempt).collect();
    assert_eq!(attempts, [None, Some(2), Some(3)]);
    let row_hosts: Vec<Option<HostId>> = rows.iter().map(|row| row.host.clone()).collect();
    let attempt_hosts: Vec<Option<HostId>> =
        hosts.iter().map(|host| HostId::new(host).ok()).collect();
    assert_eq!(row_hosts, attempt_hosts);
}
