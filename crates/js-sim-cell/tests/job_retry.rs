//! Retry and completion callbacks in the cell, on the real server and its job service: the
//! cell's server accepts a retry policy and a callback, a job whose attempt is lost is
//! tried again while its item keeps waiting, the cell's receiver gets the callback once
//! after the job's last attempt, and its actions (a receiver that is down, a second
//! instance that claims callbacks and vanishes) do what they say.
//!
//! Sizes: two hosts beating every 5 s for at most seven virtual minutes, and one job a
//! test.

use std::path::Path;
use std::time::Duration;

use js_jobs::{JobState, StartFailure};
use js_sim::Faults;
use js_sim_cell::actions::{
    HOST_REFUSAL, agent_refuses_starts, agent_takes_starts, agents, callback_receiver_down,
    claim_callbacks_and_vanish, kill_agents_mid_job, restart_server,
};
use js_sim_cell::{
    Action, CallbackCall, Cell, ClientEvent, Expect, HowEnded, ItemId, JobFront, ServerSpec, Setup,
    Shape, Work,
};
use js_testkit::dir::TestDir;
use js_types::CallbackAnswer;
use js_types::sched::FarmTime;

/// How long each job here runs.
const RUN: Duration = Duration::from_secs(60);

fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A cell of `hosts` correct agents of a registered pool, its client the job front.
fn cell(name: &str, hosts: usize) -> Cell {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell"),
        name,
    );
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), hosts);
    let mut cell = Cell::new(Setup::new(5), spec, dir, Faults::default()).expect("the cell starts");
    cell.set_front(Box::new(JobFront::new()));
    for _ in 0..hosts {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell
}

fn run_to(cell: &mut Cell, at: u64) {
    cell.run_until(ms(at)).expect("no violation");
}

/// Kills the agent that runs the job, replacing it 10 s later: the host's new process lists
/// nothing, which is its word that the attempt is gone.
fn lose_the_attempt(cell: &mut Cell) {
    kill_agents_mid_job(1, Duration::from_secs(10)).run(cell);
}

/// What `expect` says is not met by `cell`, or `None`.
fn unmet(cell: &Cell, expect: &Expect) -> Option<String> {
    expect.unmet(cell)
}

/// The calls the receiver got, in order.
fn calls(cell: &Cell) -> Vec<CallbackCall> {
    cell.observations().callback_calls.clone()
}

/// When the item first heard its terminal, and how.
fn terminal(cell: &Cell, item: ItemId) -> (FarmTime, HowEnded) {
    let heard = &cell.observations().items[&item].heard;
    let ended = heard.iter().find_map(|heard| match heard.event {
        ClientEvent::Terminal { how } => Some((heard.at, how)),
        _ => None,
    });
    ended.expect("the item ended")
}

/// Asserts that a job that asks for two attempts and a callback, whose first attempt is lost
/// while it runs, reads queued and then placed again with the item still waiting, runs its
/// second attempt and succeeds: attempts read 1 then 2 (of at most 2), the item hears one
/// terminal, `succeeded`, only then, and the receiver gets one call, answered 2xx after the
/// terminal, with the job's row already ended. Catches a front that ends the item at the lost
/// attempt, a callback made at the retry rather than the final end, and an attempt number the
/// cell does not read.
#[test]
fn a_job_whose_attempt_is_lost_is_retried_and_its_callback_comes_once_after_the_last() {
    let mut cell = cell("job_retry_success", 2);
    run_to(&mut cell, 1_000);
    let item = cell.submit(&Work::run_for(RUN).retry(2).callback());
    run_to(&mut cell, 30_000);
    assert!(cell.observations().callback_calls.is_empty());
    lose_the_attempt(&mut cell);
    run_to(&mut cell, 400_000);
    let seen = cell.observations();
    let attempts: Vec<(u32, u32)> = seen
        .attempt_reads
        .iter()
        .map(|read| (read.number, read.max_attempts))
        .collect();
    assert_eq!(attempts, [(1, 2), (2, 2)]);
    let states: Vec<JobState> = seen.job_reads.iter().map(|read| read.state).collect();
    let queued = states.iter().position(|state| *state == JobState::Queued);
    let queued = queued.expect("the job read queued between its attempts");
    assert!(states[queued..].contains(&JobState::Running), "{states:?}");
    assert_eq!(states.last(), Some(&JobState::Succeeded));
    let (ended_at, how) = terminal(&cell, item);
    assert_eq!(how, HowEnded::Succeeded);
    let made = calls(&cell);
    assert_eq!(made.len(), 1, "{made:?}");
    assert!(made[0].at >= ended_at, "the call comes after the terminal");
    assert!(made[0].ended);
    assert_eq!(made[0].state, Some(JobState::Succeeded));
    assert_eq!(made[0].answer, CallbackAnswer::Delivered);
    assert_eq!(made[0].job.as_str(), "sim-job-0");
    for expect in [
        Expect::attempts_of(item, 2),
        Expect::callbacks_delivered(item, 1),
        Expect::callback_calls(item, 1),
        Expect::job_state(item, JobState::Succeeded),
        Expect::terminal_count(1, HowEnded::Succeeded),
    ] {
        assert_eq!(unmet(&cell, &expect), None, "{expect:?}");
    }
    let wanted = Expect::attempts_of(item, 1);
    assert_eq!(
        unmet(&cell, &wanted).as_deref(),
        Some("attempt(item0): wanted 1, got 2")
    );
}

/// Asserts that a job that asks for two attempts and whose two attempts are both lost ends
/// `lost` after its second attempt (attempts read 1 and 2), the item hears `lost` once, and
/// the receiver gets one call, after that end, with the job's row `lost`. Catches a job that
/// is tried a third time, and a callback made after the first lost attempt.
#[test]
fn a_job_whose_attempts_run_out_ends_lost_and_its_callback_comes_after_the_last() {
    let mut cell = cell("job_retry_exhausted", 2);
    run_to(&mut cell, 1_000);
    let item = cell.submit(&Work::run_for(RUN).retry(2).callback());
    run_to(&mut cell, 30_000);
    lose_the_attempt(&mut cell);
    run_to(&mut cell, 75_000);
    assert!(cell.observations().callback_calls.is_empty());
    lose_the_attempt(&mut cell);
    run_to(&mut cell, 400_000);
    let (ended_at, how) = terminal(&cell, item);
    assert_eq!(how, HowEnded::Lost);
    let made = calls(&cell);
    assert_eq!(made.len(), 1, "{made:?}");
    assert!(made[0].at >= ended_at && made[0].ended);
    assert_eq!(made[0].state, Some(JobState::Lost));
    for expect in [
        Expect::attempts_of(item, 2),
        Expect::job_state(item, JobState::Lost),
        Expect::callbacks_delivered(item, 1),
    ] {
        assert_eq!(unmet(&cell, &expect), None, "{expect:?}");
    }
}

/// Asserts that a job that asks for a callback and no retry, whose attempt is lost, ends
/// `lost` at its first and only attempt (read as 1 of at most 1, with no `attempt` line in the
/// trace), and gets its callback once. Catches a job without a policy that is tried again.
#[test]
fn a_job_without_a_policy_is_lost_at_its_first_attempt_and_its_callback_is_made() {
    let mut cell = cell("job_retry_none", 2);
    run_to(&mut cell, 1_000);
    let item = cell.submit(&Work::run_for(RUN).callback());
    run_to(&mut cell, 30_000);
    lose_the_attempt(&mut cell);
    run_to(&mut cell, 200_000);
    let attempts = &cell.observations().attempt_reads;
    assert_eq!(attempts.len(), 1);
    assert_eq!((attempts[0].number, attempts[0].max_attempts), (1, 1));
    for expect in [
        Expect::job_state(item, JobState::Lost),
        Expect::callbacks_delivered(item, 1),
        Expect::attempts_of(item, 1),
    ] {
        assert_eq!(unmet(&cell, &expect), None, "{expect:?}");
    }
}

/// Asserts that a receiver switched down for a while answers each call made meanwhile a failed
/// try, and 2xx after; that the server tries the callback again with the same `webhook-id`
/// until it is delivered, exactly once; and that the receiver and its calls survive a restart
/// of the server in between. Catches a receiver that answers 2xx while down, a callback that
/// gets a new id at each try, a delivery counted twice, and a receiver that a restart resets.
#[test]
fn a_receiver_that_is_down_is_tried_again_with_the_same_webhook_id_until_it_answers() {
    let mut cell = cell("job_retry_receiver_down", 1);
    run_to(&mut cell, 1_000);
    callback_receiver_down(Duration::from_secs(120)).run(&mut cell);
    let item = cell.submit(&Work::run_for(Duration::from_secs(20)).callback());
    run_to(&mut cell, 60_000);
    let before = calls(&cell);
    assert!(!before.is_empty(), "the callback was tried");
    assert!(
        before
            .iter()
            .all(|call| call.answer == CallbackAnswer::Failed(js_types::FailureClass::Failed))
    );
    assert_eq!(unmet(&cell, &Expect::callbacks_delivered(item, 0)), None);
    restart_server().run(&mut cell);
    let kept = cell.server().expect("the node").receiver().calls_after(0);
    assert_eq!(kept, before, "a restart keeps the receiver's calls");
    run_to(&mut cell, 400_000);
    let made = calls(&cell);
    assert!(made.len() > before.len());
    let ids: std::collections::BTreeSet<&str> =
        made.iter().map(|call| call.webhook_id.as_str()).collect();
    assert_eq!(ids.len(), 1, "one webhook id for every try: {ids:?}");
    let last = made.last().expect("a call");
    assert_eq!(last.answer, CallbackAnswer::Delivered);
    assert!(last.at >= ms(121_000), "the receiver was down until 121 s");
    assert_eq!(unmet(&cell, &Expect::callbacks_delivered(item, 1)), None);
    assert_eq!(
        unmet(&cell, &Expect::callback_calls(item, made.len())),
        None
    );
}

/// Asserts that a second instance that claims the due callbacks and never answers
/// ([`Cell::claim_callbacks_and_vanish`]) holds a callback for 30 s: it finds nothing due
/// before the job ends, the cell's server makes no call to the receiver meanwhile, and after
/// the lapse it delivers the callback, once; and that the claim finds nothing due when the
/// server is down. Catches a claim that does not keep the server from the callback, a lapse
/// that never hands it back, and a claim that answers.
#[test]
fn a_lapsed_claim_of_a_second_instance_is_delivered_by_the_cells_server() {
    let mut cell = cell("job_retry_claim", 1);
    run_to(&mut cell, 1_000);
    let item = cell.submit(&Work::run_for(Duration::from_secs(20)).callback());
    // The job's end falls between two ticks of the server, and the claim is made in that
    // gap, so the server has made no try before it: step until a callback is due.
    let mut at = 15_000;
    run_to(&mut cell, at);
    while cell.claim_callbacks_and_vanish() == 0 {
        at += 250;
        assert!(at < 100_000, "a callback came due");
        run_to(&mut cell, at);
    }
    assert!(calls(&cell).is_empty(), "the claim came before any try");
    run_to(&mut cell, at + 29_000);
    assert!(calls(&cell).is_empty(), "no call while the claim holds");
    run_to(&mut cell, at + 60_000);
    let made = calls(&cell);
    assert_eq!(made.len(), 1, "{made:?}");
    assert_eq!(made[0].answer, CallbackAnswer::Delivered);
    assert!(made[0].at >= ms(at + 30_000), "after the lapse");
    assert_eq!(unmet(&cell, &Expect::callbacks_delivered(item, 1)), None);
    claim_callbacks_and_vanish().run(&mut cell);
    cell.stop_server();
    assert_eq!(cell.claim_callbacks_and_vanish(), 0);
}

/// Steps `cell` by a second until an agent has made a result, within a minute of `from`.
fn until_a_result(cell: &mut Cell, from: u64) -> u64 {
    let mut at = from;
    while cell.observations().results_made.is_empty() {
        at += 1_000;
        assert!(at < from + 60_000, "an agent made a result");
        run_to(cell, at);
    }
    at
}

/// Asserts that when every agent refuses the start with a code of the host's and takes starts
/// again after the first refusal, a job that asks for two attempts is refused once (launching
/// nothing, its result made as a held refusal), is placed again on the other host and
/// succeeds: one launch only, attempts read 1 then 2, and the refused host is not the one
/// that launched. Catches a refusal that launches the job, one that is not kept until
/// acknowledged, and a host's code that does not retry.
#[test]
fn a_start_refused_with_a_hosts_code_is_retried_on_another_host() {
    let mut cell = cell("job_retry_refused_once", 2);
    run_to(&mut cell, 1_000);
    agent_refuses_starts(agents(0..2), HOST_REFUSAL).run(&mut cell);
    let item = cell.submit(&Work::run_for(RUN).retry(2));
    let at = until_a_result(&mut cell, 1_000);
    assert!(
        cell.ledger().launches().is_empty(),
        "a refused start launches nothing"
    );
    agent_takes_starts(agents(0..2)).run(&mut cell);
    run_to(&mut cell, at + 300_000);
    let seen = cell.observations();
    let refused = seen.results_made[0].host.clone();
    assert!(seen.results_made[0].held && seen.results_made.len() == 2);
    let launches = cell.ledger().launches();
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_ne!(launches[0].host, refused);
    for expect in [
        Expect::attempts_of(item, 2),
        Expect::job_state(item, JobState::Succeeded),
        Expect::terminal_count(1, HowEnded::Succeeded),
    ] {
        assert_eq!(unmet(&cell, &expect), None, "{expect:?}");
    }
}

/// Asserts that a job that asks for two attempts, on agents that refuse every start with a
/// code of the host's, is refused on each attempt and ends failed to start (agent refused)
/// after its second, with nothing launched. Catches a refused job that is tried a third
/// time and one that is never refused again.
#[test]
fn a_job_refused_on_every_attempt_ends_failed_to_start_after_its_last() {
    let mut cell = cell("job_retry_refused_always", 2);
    run_to(&mut cell, 1_000);
    agent_refuses_starts(agents(0..2), HOST_REFUSAL).run(&mut cell);
    let item = cell.submit(&Work::run_for(RUN).retry(2));
    run_to(&mut cell, 400_000);
    assert!(cell.ledger().launches().is_empty());
    let ended = JobState::FailedToStart(StartFailure::AgentRefused);
    for expect in [Expect::attempts_of(item, 2), Expect::job_state(item, ended)] {
        assert_eq!(unmet(&cell, &expect), None, "{expect:?}");
    }
}

/// Asserts that a start refused with a code of the job's own (`invalid-job`) is not retried
/// whatever the policy: the job asks for three attempts and ends failed to start (agent
/// refused) at its first. Catches a job-fault refusal that retries.
#[test]
fn a_start_refused_with_a_jobs_code_ends_at_its_first_attempt() {
    let mut cell = cell("job_retry_refused_job_fault", 2);
    run_to(&mut cell, 1_000);
    agent_refuses_starts(agents(0..2), "invalid-job").run(&mut cell);
    let item = cell.submit(&Work::run_for(RUN).retry(3));
    run_to(&mut cell, 200_000);
    let ended = JobState::FailedToStart(StartFailure::AgentRefused);
    for expect in [Expect::attempts_of(item, 1), Expect::job_state(item, ended)] {
        assert_eq!(unmet(&cell, &expect), None, "{expect:?}");
    }
}
