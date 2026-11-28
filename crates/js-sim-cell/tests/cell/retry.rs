//! The scenarios of the retry family (`retry.rs`): a job whose attempt ends for a cause of
//! its host is tried again while its item keeps waiting, and its completion callback comes
//! once, after its last attempt. Each scenario here runs the job front over the real server
//! and its job service, with the cell's callback receiver.
//!
//! Every wait is virtual. A job that is refused or lost waits at least the policy's first
//! backoff (10 s, with the job's own jitter, `js_types::decide`) before its next attempt, and
//! the host that failed it is out of its next placement for 15 minutes, so a retry needs
//! another host. The cell's agents do not report `start-fence`, so no scenario returns an
//! attempt to the host that failed it.
//!
//! Sizes, in agent requests a seed (heartbeats every 5 s, the server's default, or every 30 s
//! for the late result, with the hellos besides), each an upper bound from the hosts, the
//! virtual time to the last action and the settle, far under the 30,000 a seed of the cell's
//! sizing rule:
//! - the restart between attempts: two hosts for 35 s plus 400 s, 174;
//! - the callbacks after each final state: two hosts for [`PHASE`] times two, a settle of
//!   [`SETTLE`] and the cancel, 250;
//! - the restart with due callbacks: one host for 100 s plus 300 s, 80;
//! - the cancel racing a retry: [`RACE_HOSTS`] hosts for 80 s plus 300 s, 304;
//! - the whole-host retry: [`WHOLE_HOSTS`] hosts for 45 s plus 1,200 s, 1,494;
//! - the late result: two hosts for [`CUT_AT`] plus [`CUT_FOR`] plus [`LATE_SETTLE`], 200;
//! - the lapsed claim: one host for 20 s plus 120 s, 28.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use js_agent_wire::RefusalReason;
use js_conformance::agent::Defect;
use js_jobs::{JobState, StartFailure};
use js_sim_cell::actions::{
    Action, HOST_REFUSAL, agent_refuses_prepare, agent_refuses_starts, agent_takes_starts, agents,
    callback_receiver_down, cancel_work, kill_agents_mid_job, partition_agents, restart_server,
    stop_server,
};
use js_sim_cell::{
    Cell, Counter, Expect, HowEnded, ItemId, Scenario, ServerSpec, Shape, Verdict, Work,
};
use js_types::CallbackAnswer;
use js_types::sched::FarmTime;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// How often the actions below look at what the cell has observed.
const POLL: Duration = Duration::from_millis(50);

/// Runs `then` as soon as `ready` holds of the cell: now, or at the first of the polls (every
/// [`POLL`] of virtual time) at which it does.
fn when(cell: &mut Cell, ready: Rc<dyn Fn(&Cell) -> bool>, then: Rc<dyn Fn(&mut Cell)>) {
    if ready(cell) {
        then(cell);
        return;
    }
    let next = cell.now().saturating_add(POLL);
    cell.at(next, move |cell| when(cell, ready, then));
}

/// An action that runs `then` once `ready` holds of the cell, from when the action runs.
struct Once {
    ready: Rc<dyn Fn(&Cell) -> bool>,
    then: Rc<dyn Fn(&mut Cell)>,
}

impl Action for Once {
    fn run(&self, cell: &mut Cell) {
        when(cell, self.ready.clone(), self.then.clone());
    }
}

/// An action that runs `then` once `ready` holds of the cell.
fn once(
    ready: impl Fn(&Cell) -> bool + 'static,
    then: impl Fn(&mut Cell) + 'static,
) -> impl Action + 'static {
    Once {
        ready: Rc::new(ready),
        then: Rc::new(then),
    }
}

/// How many results the agents made for job `n` that launched nothing: a start refused.
fn refusals_of(cell: &Cell, n: u32) -> usize {
    let job = format!("sim-job-{n}");
    let made = &cell.observations().results_made;
    made.iter()
        .filter(|made| made.held && made.job.as_str() == job)
        .count()
}

/// Whether the job service last read job `n` queued after an attempt that ended: the job
/// waits for its next attempt, with its item still waiting.
fn waits_for_a_retry(cell: &Cell, n: u32) -> bool {
    let job = format!("sim-job-{n}");
    let read = cell
        .observations()
        .job_reads
        .iter()
        .rev()
        .find(|read| read.job.as_str() == job);
    read.is_some_and(|read| read.state == JobState::Queued)
}

/// The pool of `hosts` hosts every scenario here runs over.
fn pool(hosts: usize) -> ServerSpec {
    ServerSpec::registered_pool("burst", Shape::gpu_host(), hosts)
}

/// How long the server is down in the restart between attempts: longer than the grace, so
/// the agent sends the refused start's result more than once before the server is back.
pub const DOWN_FOR: Duration = Duration::from_secs(30);

/// How long each job runs unless it says otherwise.
const RUN: Duration = Duration::from_secs(30);

/// Proves that a crash between an attempt's end and its retry makes one retry. Two hosts whose agents refuse every start with a code
/// of the host's (`host-tainted`) and one job that asks for two attempts. The agent that
/// refused the start keeps the result until the server acknowledges it; the moment it is
/// made, the server is stopped and the agents take starts again, and [`DOWN_FOR`] later the
/// server starts over its own directory and store. The agent sends the result again
/// (witness `result_resent`), the server ends the first attempt and tries the job again once,
/// on the other host, which runs it to success. States that the job read attempt 2 only after
/// the restart, was launched once (under its second allocation), ended succeeded, and that
/// the client heard one terminal. Red for a server that loses the refused attempt over a
/// restart (the job is not retried: `job-state`), retries it twice (a second launch, an
/// attempt 3 past the limit: `attempts-bounded`) or launches the refused attempt.
pub fn restart_between_attempts() -> Scenario {
    restart_between_attempts_of(None)
}

/// [`restart_between_attempts`] with agents that lack `defect`, or correct ones.
pub fn restart_between_attempts_of(defect: Option<Defect>) -> Scenario {
    let job = ItemId(0);
    let retry = Work::run_for(RUN).retry(2);
    let scenario = Scenario::new("restart_between_attempts")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(2));
    let scenario = match defect {
        Some(defect) => scenario.broken_hosts("burst", 2, defect),
        None => scenario.hosts("burst", 2),
    };
    scenario
        .at(
            Duration::ZERO,
            agent_refuses_starts(agents(0..2), HOST_REFUSAL),
        )
        .work(1, retry)
        .at(
            Duration::ZERO,
            once(
                |cell| refusals_of(cell, 0) > 0,
                |cell| {
                    agent_takes_starts(agents(0..2)).run(cell);
                    stop_server().run(cell);
                    let again = cell.now().saturating_add(DOWN_FOR);
                    cell.at(again, |cell| restart_server().run(cell));
                },
            ),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect(Expect::job_state(job, JobState::Succeeded))
        .expect(Expect::attempts_of(job, 2))
        .expect(Expect::allocations_of(job, 2))
        .expect_that("one launch, on the second attempt", |cell| {
            let launches = cell.ledger().launches();
            (launches.len() != 1).then(|| format!("{} launches: {launches:?}", launches.len()))
        })
        .expect_that("the retry came after the restart", |cell| {
            let seen = cell.observations();
            let refused = seen.results_made.first().map(|made| made.at);
            let second = seen.attempt_reads.iter().find(|read| read.number == 2);
            match (refused, second) {
                (Some(refused), Some(second)) if second.at >= refused.saturating_add(DOWN_FOR) => {
                    None
                }
                other => Some(format!("refusal and attempt 2: {other:?}")),
            }
        })
        .witness("result_resent", |c| c.get(Counter::ResultResent) >= 1)
        .settle_within(secs(400))
}

/// How long each phase of [`one_callback_after_retries_are_exhausted`] lasts.
pub const PHASE: Duration = Duration::from_secs(200);

/// How long the cell runs after the last phase.
pub const SETTLE: Duration = Duration::from_secs(200);

/// What is wrong with the callback of `item`, whose job must have ended `state`: it was
/// called once, answered 2xx, after the item heard its end, with the job's row ended in that
/// state; or `None`.
fn callback_after_the_end(cell: &Cell, item: ItemId, state: JobState) -> Option<String> {
    let seen = cell.observations();
    let job = format!("sim-job-{}", item.0);
    let calls: Vec<_> = seen
        .callback_calls
        .iter()
        .filter(|call| call.job.as_str() == job)
        .collect();
    let heard = seen.items.get(&item).and_then(|item| {
        item.heard
            .iter()
            .find(|heard| matches!(heard.event, js_sim_cell::ClientEvent::Terminal { .. }))
            .map(|heard| heard.at)
    });
    match (calls.as_slice(), heard) {
        ([call], Some(heard))
            if call.answer == CallbackAnswer::Delivered
                && call.ended
                && call.state == Some(state)
                && call.at >= heard =>
        {
            None
        }
        _ => Some(format!("{job} heard its end at {heard:?}, calls {calls:?}")),
    }
}

/// Proves rule C1 for a retried job at each final state: it gets exactly one callback, after
/// its last attempt, none before. Two hosts whose agents refuse every start with a code of
/// the host's, and three jobs that ask for a callback, one in each of three phases of
/// [`PHASE`]: the first asks for two attempts, and the agents take starts again once its
/// first is refused, so it succeeds on its second; the second asks for two and is refused
/// on both, so it ends failed to start (agent refused) with no retry left; the third asks
/// for three attempts and is cancelled once its first is refused, while it waits for its
/// retry. States for each that the receiver was called once, answered 2xx, after the item
/// heard its end, with the job's row ended in the state it reached (`callback-after-the-final-end`
/// holds of every call, so none came while a job waited for another attempt), and the
/// attempts the job had (2, 2 and 1). Red for a callback made at a retry (a second call, one
/// with the job's row queued), none at a final state, and an attempt after the cancel.
pub fn one_callback_after_retries_are_exhausted() -> Scenario {
    let (succeeded, exhausted, cancelled) = (ItemId(0), ItemId(1), ItemId(2));
    let refused = JobState::FailedToStart(StartFailure::AgentRefused);
    let all = || agents(0..2);
    Scenario::new("one_callback_after_retries_are_exhausted")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(2))
        .hosts("burst", 2)
        .at(Duration::ZERO, agent_refuses_starts(all(), HOST_REFUSAL))
        .work(1, Work::run_for(RUN).retry(2).callback())
        .at(
            Duration::ZERO,
            once(
                |cell| refusals_of(cell, 0) > 0,
                move |cell| agent_takes_starts(all()).run(cell),
            ),
        )
        .at(PHASE - secs(10), agent_refuses_starts(all(), HOST_REFUSAL))
        .at(PHASE, submit(Work::run_for(RUN).retry(2).callback()))
        .at(2 * PHASE, submit(Work::run_for(RUN).retry(3).callback()))
        .at(
            2 * PHASE,
            once(
                |cell| waits_for_a_retry(cell, 2),
                |cell| cancel_work(vec![ItemId(2)]).run(cell),
            ),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::job_state(succeeded, JobState::Succeeded))
        .expect(Expect::job_state(exhausted, refused))
        .expect(Expect::job_state(cancelled, JobState::Cancelled))
        .expect(Expect::attempts_of(succeeded, 2))
        .expect(Expect::attempts_of(exhausted, 2))
        .expect(Expect::attempts_of(cancelled, 1))
        .expect(Expect::callbacks_delivered(succeeded, 1))
        .expect(Expect::callbacks_delivered(exhausted, 1))
        .expect(Expect::callbacks_delivered(cancelled, 1))
        .expect_that("the callback of the job that succeeded", move |cell| {
            callback_after_the_end(cell, succeeded, JobState::Succeeded)
        })
        .expect_that(
            "the callback of the job whose attempts ran out",
            move |cell| callback_after_the_end(cell, exhausted, refused),
        )
        .expect_that(
            "the callback of the job cancelled while it waited",
            move |cell| callback_after_the_end(cell, cancelled, JobState::Cancelled),
        )
        .settle_within(SETTLE)
}

/// Submits one item of `work` now.
fn submit(work: Work) -> impl Action + 'static {
    js_sim_cell::actions::submit_work(1, work)
}

/// How long the callback of [`restart_with_due_callbacks`] is tried after the job's end.
const SHORT_DEADLINE: Duration = Duration::from_secs(10);

/// How long the receiver of [`restart_with_due_callbacks`] is down from the start, and the
/// server after the first call.
const RECEIVER_DOWN: Duration = Duration::from_secs(40);
const SERVER_DOWN: Duration = Duration::from_secs(60);

/// Proves that callbacks due while the server is down are delivered after it restarts: the
/// deadline of a due callback is raised to at least 120 s after the boot. One host, one job
/// that asks for a callback tried for [`SHORT_DEADLINE`] after its end, and a receiver that
/// is down for [`RECEIVER_DOWN`] from the start. The server is stopped as soon as the first
/// call has been answered with a failure, and started again [`SERVER_DOWN`] later, when the
/// callback's own deadline has passed and the receiver is up. States that the receiver was
/// called more than once, with one webhook id, that the first call was answered with a
/// failure and the last was delivered after the restart (so no call came while the server was
/// down), and that the callback was delivered once. Red for a restart that lets a due
/// callback expire (no delivery) or one that makes a new webhook id.
pub fn restart_with_due_callbacks() -> Scenario {
    let item = ItemId(0);
    Scenario::new("restart_with_due_callbacks")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(1))
        .hosts("burst", 1)
        .at(Duration::ZERO, callback_receiver_down(RECEIVER_DOWN))
        .work(1, Work::run_for(secs(20)).callback_for(SHORT_DEADLINE))
        .at(
            Duration::ZERO,
            once(
                |cell| !cell.observations().callback_calls.is_empty(),
                |cell| {
                    stop_server().run(cell);
                    let again = cell.now().saturating_add(SERVER_DOWN);
                    cell.at(again, |cell| restart_server().run(cell));
                },
            ),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::job_state(item, JobState::Succeeded))
        .expect(Expect::callbacks_delivered(item, 1))
        .expect_that(
            "failed, then delivered after the restart, with one id",
            |cell| {
                let calls = &cell.observations().callback_calls;
                let ids: std::collections::BTreeSet<&str> =
                    calls.iter().map(|call| call.webhook_id.as_str()).collect();
                let (Some(first), Some(last)) = (calls.first(), calls.last()) else {
                    return Some("the receiver got no call".to_owned());
                };
                let held = matches!(first.answer, CallbackAnswer::Failed(_))
                    && last.answer == CallbackAnswer::Delivered
                    && last.at >= first.at.saturating_add(SERVER_DOWN)
                    && ids.len() == 1
                    && calls.len() > 1;
                (!held).then(|| format!("calls {calls:?}"))
            },
        )
        .settle_within(secs(300))
}

/// The cancel race: this many hosts run this many jobs of 300 s, all lost at [`LOSE_AT`].
const RACE_HOSTS: usize = 4;
const RACE_JOBS: u32 = 8;
const LOSE_AT: Duration = Duration::from_secs(40);

/// When each of the jobs is cancelled, from the start: the first two before the server reads
/// the jobs lost (the hosts are killed at [`LOSE_AT`], replaced 10 s later and read absent
/// about 15 s after the kill), one just before, two with it, and three after it, one of them
/// after the retried job's second attempt has started.
const CANCEL_AT: [u64; RACE_JOBS as usize] = [36, 42, 52, 55, 56, 60, 66, 80];

/// The time `job`'s row was last read in a state of its own, and which state: the last
/// `JobRead` of the job.
fn last_read(cell: &Cell, job: &str) -> Option<(FarmTime, JobState)> {
    let reads = &cell.observations().job_reads;
    let read = reads.iter().rev().find(|read| read.job.as_str() == job)?;
    Some((read.at, read.state))
}

/// What is wrong with how each job of [`a_cancel_racing_a_retry_starts_no_attempt_after_it`] ended,
/// or `None`: a job whose cancel was asked while it ran on its lost host ends lost (a job with a
/// cancel asked for is never tried again, and its stop never reached the host), and every other
/// ends cancelled; no job was launched after its cancel was asked.
fn raced_jobs_end_as_the_server_promises(cell: &Cell) -> Option<String> {
    let seen = cell.observations();
    let launches = cell.ledger().launches();
    let mut wrong = Vec::new();
    for (item, asked) in &seen.cancels {
        let job = format!("sim-job-{}", item.0);
        let reads: Vec<(FarmTime, JobState)> = seen
            .job_reads
            .iter()
            .filter(|read| read.job.as_str() == job)
            .map(|read| (read.at, read.state))
            .collect();
        let ended = reads.last().map(|(_, state)| *state);
        // A job read queued before its cancel was asked was retried: the cancel met a queue.
        let retried = reads
            .iter()
            .any(|(at, state)| *state == JobState::Queued && at <= asked);
        let right = match ended {
            Some(JobState::Cancelled) => true,
            Some(JobState::Lost) => !retried,
            _ => false,
        };
        if !right {
            wrong.push(format!("{job} ended {ended:?}, cancel asked at {asked:?}"));
        }
        let after = launches
            .iter()
            .filter(|launch| launch.run.as_str().starts_with(&format!("{job}/")))
            .filter(|launch| launch.at > *asked);
        wrong.extend(
            after.map(|launch| format!("{job} launched at {:?} after its cancel", launch.at)),
        );
    }
    (!wrong.is_empty()).then(|| wrong.join("; "))
}

/// Proves that a job whose cancel races its retry is never tried again after the cancel:
/// [`RACE_HOSTS`] hosts run [`RACE_JOBS`] jobs of 300 s that ask for three attempts, every host's
/// agent is killed at [`LOSE_AT`] and replaced 10 s later, so the server reads each job lost, and
/// each job is cancelled at a time of its own from [`CANCEL_AT`]: before, with and after the
/// server's reading the jobs lost. A job whose cancel was asked while it ran on the lost host ends
/// lost, as a job with a cancel intent does today, and is not retried; a job the server had put
/// back in the queue (the retry won) is ended cancelled by the cancel at once; and the job that had
/// started its second attempt is stopped there and ends cancelled. States that every job ended,
/// that each ended as that says ([`raced_jobs_end_as_the_server_promises`]), that no allocation is
/// left booked, that at least two jobs ended lost and at least two cancelled, and that no job was
/// launched after its cancel was asked. Red for a server that retries a job whose cancel was asked
/// for (`attempts-bounded`, a launch after the cancel), or leaves a cancelled job queued.
pub fn a_cancel_racing_a_retry_starts_no_attempt_after_it() -> Scenario {
    let jobs = RACE_JOBS as usize;
    let mut scenario = Scenario::new("a_cancel_racing_a_retry_starts_no_attempt_after_it")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(RACE_HOSTS))
        .hosts("burst", RACE_HOSTS)
        .work(RACE_JOBS, Work::run_for(secs(300)).retry(3))
        .at(LOSE_AT, kill_agents_mid_job(RACE_HOSTS, secs(10)));
    for (item, at) in CANCEL_AT.iter().enumerate() {
        let item = ItemId(item as u32);
        scenario = scenario.at(secs(*at), cancel_work(vec![item]));
    }
    scenario
        .expect(Expect::all_items_terminal())
        .expect(Expect::jobs_ended(jobs))
        .expect(Expect::no_allocation_live())
        .expect_that("each job ended as the cancel and the loss decide", |cell| {
            raced_jobs_end_as_the_server_promises(cell)
        })
        .expect_that(
            "some cancels met a queued retry and some a lost job",
            |cell| {
                let ends: Vec<Option<(FarmTime, JobState)>> = (0..RACE_JOBS)
                    .map(|n| last_read(cell, &format!("sim-job-{n}")))
                    .collect();
                let lost = ends
                    .iter()
                    .flatten()
                    .filter(|end| end.1 == JobState::Lost)
                    .count();
                let cancelled = ends
                    .iter()
                    .flatten()
                    .filter(|end| end.1 == JobState::Cancelled)
                    .count();
                (lost < 2 || cancelled < 2).then(|| format!("{lost} lost, {cancelled} cancelled"))
            },
        )
        .settle_within(secs(300))
}

/// The late result: two hosts beating every 30 s and held lost after 90 s, both cut off at
/// [`CUT_AT`] for [`CUT_FOR`], longer than the server's start timeout.
const LATE_INTERVAL: Duration = Duration::from_secs(30);
const LATE_GRACE: Duration = Duration::from_secs(90);
const CUT_AT: Duration = Duration::from_secs(45);
const CUT_FOR: Duration = Duration::from_secs(1_155);
const LATE_RUN: Duration = Duration::from_secs(1_500);
const LATE_SETTLE: Duration = Duration::from_secs(1_800);
/// The server's silence limit in the late-result scenario: longer than the cut, so the first
/// attempt ends at the start timeout and retries, rather than being lost to the silence limit
/// (ten minutes by default), which never retries.
const LATE_LIMIT: Duration = Duration::from_secs(3_600);

/// Proves that a late result of an earlier attempt is answered stale and changes nothing.
/// Two hosts beating every 30 s, a job that asks for two attempts, and both hosts cut off at
/// [`CUT_AT`], after the job's start reached its host and before any heartbeat listed it, for
/// [`CUT_FOR`]: longer than the server's start timeout, so the first attempt ends failed to
/// start and the job waits for its second. The host comes back still running the first
/// attempt and reports it; the server tells it to stop, reads its result and answers it
/// stale, frees the allocation, and only then runs the second attempt. States that an agent
/// read a stale verdict for the job, that the job read attempt 2, was launched twice, the
/// second launch after the stale answer, and ended succeeded with one terminal for its item
/// and no allocation left booked. Red for a server that lets the late result end or change
/// the second attempt (`job-state`, `jobs-ended`), or that starts it beside the first
/// (`one-attempt-at-a-time`).
pub fn a_late_result_of_an_earlier_attempt_is_stale() -> Scenario {
    let job = ItemId(0);
    let spec = pool(2)
        .heartbeat(LATE_INTERVAL, LATE_GRACE)
        .silence_limit(LATE_LIMIT);
    Scenario::new("a_late_result_of_an_earlier_attempt_is_stale")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", 2)
        .work(1, Work::run_for(LATE_RUN).retry(2))
        .at(CUT_AT, partition_agents(agents(0..2), CUT_FOR))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect(Expect::job_state(job, JobState::Succeeded))
        .expect(Expect::attempts_of(job, 2))
        .expect(Expect::allocations_of(job, 2))
        .expect(Expect::no_allocation_live())
        .expect_that("a stale answer came before the second launch", |cell| {
            let seen = cell.observations();
            let stale = seen.job_replies.iter().find(|reply| {
                reply
                    .acks
                    .iter()
                    .any(|(_, verdict)| *verdict == Verdict::Stale)
            });
            let launches = cell.ledger().launches();
            match (stale, launches.as_slice()) {
                (Some(stale), [_, second]) if stale.at <= second.at => None,
                (stale, launches) => Some(format!(
                    "stale answer {:?}, launches {launches:?}",
                    stale.map(|reply| reply.at)
                )),
            }
        })
        .settle_within(LATE_SETTLE)
}

/// How often the second instance looks for a due callback: finer than the server's own tick,
/// so that it finds the callback between the job's end and the server's next tick.
const CLAIM_POLL: Duration = Duration::from_millis(10);

/// How long a claim holds a callback before another instance takes it again.
const CLAIM_LAPSE: Duration = Duration::from_secs(30);

/// When the second instance claimed the due callback, shared with the statement that judges
/// it.
type ClaimedAt = Rc<RefCell<Option<FarmTime>>>;

/// Has the second instance claim every due callback and vanish as soon as there is one,
/// noting when ([`Cell::claim_callbacks_and_vanish`]).
fn claim_when_due(cell: &mut Cell, claimed: ClaimedAt) {
    if cell.claim_callbacks_and_vanish() > 0 {
        *claimed.borrow_mut() = Some(cell.now());
        return;
    }
    let next = cell.now().saturating_add(CLAIM_POLL);
    cell.at(next, move |cell| claim_when_due(cell, claimed));
}

/// The action that starts [`claim_when_due`].
struct ClaimWhenDue(ClaimedAt);

impl Action for ClaimWhenDue {
    fn run(&self, cell: &mut Cell) {
        *self.0.borrow_mut() = None;
        claim_when_due(cell, self.0.clone());
    }
}

/// Proves that a callback whose claim lapsed is delivered by another instance. One host, one
/// job that asks for a callback, and a second instance that claims the callback the moment
/// it is due and never answers (an instance that dies holding its claims). The cell's
/// server, which would have delivered it at its next tick, finds the callback claimed, and
/// delivers it once the claim has lapsed, [`CLAIM_LAPSE`] after. States that the receiver
/// got exactly one call, answered 2xx, at or after the lapse, with the job's row ended; that
/// the claim was made before any call; and that the callback was delivered once. Red for a
/// claim that does not keep the server from the callback (a call within the lapse), a lapse
/// that never hands it back (no delivery), and a callback delivered twice.
pub fn a_lapsed_callback_claim_is_delivered_by_another_instance() -> Scenario {
    let item = ItemId(0);
    let claimed: ClaimedAt = Rc::default();
    let judged = claimed.clone();
    Scenario::new("a_lapsed_callback_claim_is_delivered_by_another_instance")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(1))
        .hosts("burst", 1)
        .work(1, Work::run_for(secs(20)).callback())
        .at(Duration::ZERO, ClaimWhenDue(claimed))
        .expect(Expect::all_items_terminal())
        .expect(Expect::callbacks_delivered(item, 1))
        .expect(Expect::callback_calls(item, 1))
        .expect_that("one call, after the lapse of the claim", move |cell| {
            let claimed = *judged.borrow();
            let calls = &cell.observations().callback_calls;
            match (claimed, calls.as_slice()) {
                (Some(claimed), [call])
                    if call.at >= claimed.saturating_add(CLAIM_LAPSE) && call.ended =>
                {
                    None
                }
                (claimed, calls) => Some(format!("claimed at {claimed:?}, calls {calls:?}")),
            }
        })
        .settle_within(secs(120))
}

/// The whole-host world: [`WHOLE_HOSTS`] whole-node hosts, so that each of the two attempts
/// of the job meets every failed prepare it may (the server allows three a job) and still has
/// a host that is ready: attempt 1 is refused by the first two and run by the third, which is
/// lost, attempt 2 refused by the next two and run by the last.
const WHOLE_HOSTS: usize = 6;

/// How long the whole-host job runs.
const WHOLE_RUN: Duration = Duration::from_secs(120);

/// Proves that a whole-host attempt lost while it runs is tried again with its failed
/// prepares counted again from zero. Six whole-node hosts whose first two agents refuse
/// every prepare (`setup-failed`) and one whole-host job that asks for two attempts. The job
/// is placed on the first host, refused, on the second, refused, and on the third, which
/// prepares and runs it. Once it runs, the next two agents are made to refuse and the third
/// host's agent is killed and replaced 10 s later: the server reads the attempt lost and
/// queues the job again, and its second attempt is refused by the next two hosts and run by
/// the last, which succeeds. That is four failed prepares for the job, more than the three
/// the server allows an attempt (`max_replace`), so it ends `exclusive-not-ready` unless
/// each attempt counts its own. States that the job succeeded on its second attempt, had six
/// allocations and was launched twice, once on each host that ran it, that the client heard
/// one terminal and no allocation is left booked. Red for a server that carries the failed
/// prepares of the first attempt into the second (the job ends failed to start), or that does
/// not retry a lost whole-host attempt.
pub fn whole_host_retry() -> Scenario {
    let job = ItemId(0);
    let spec = ServerSpec::whole_node_pool("burst", Shape::cpu_host(), WHOLE_HOSTS);
    Scenario::new("whole_host_retry")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", WHOLE_HOSTS)
        .at(
            Duration::ZERO,
            agent_refuses_prepare(agents(0..2), RefusalReason::SetupFailed),
        )
        .work(1, Work::whole_host(WHOLE_RUN).retry(2))
        .at(
            Duration::ZERO,
            once(
                |cell| waits_running(cell, 0),
                |cell| {
                    agent_refuses_prepare(agents(3..5), RefusalReason::SetupFailed).run(cell);
                    kill_agents_mid_job(1, secs(10)).run(cell);
                },
            ),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Succeeded))
        .expect(Expect::job_state(job, JobState::Succeeded))
        .expect(Expect::attempts_of(job, 2))
        .expect(Expect::allocations_of(job, 6))
        .expect(Expect::no_allocation_live())
        .expect_that(
            "the job ran twice, once on each host that was ready",
            |cell| {
                let hosts: Vec<String> = cell
                    .ledger()
                    .launches()
                    .iter()
                    .map(|launch| launch.host.to_string())
                    .collect();
                (hosts.len() != 2 || hosts[0] == hosts[1]).then(|| format!("launched on {hosts:?}"))
            },
        )
        .settle_within(secs(1_200))
}

/// Whether the job service last read job `n` running.
fn waits_running(cell: &Cell, n: u32) -> bool {
    let job = format!("sim-job-{n}");
    let reads = &cell.observations().job_reads;
    let read = reads.iter().rev().find(|read| read.job.as_str() == job);
    read.is_some_and(|read| read.state == JobState::Running)
}
