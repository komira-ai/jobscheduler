//! The fault family: what goes wrong between the server and its hosts. Agents that die
//! and return, agents that never return, a partition longer than the grace that heals,
//! a lossy wire, a host that goes silent holding a run the server never saw listed, a
//! restart of the server, and a host forgotten and seated again. Each runs over sixteen seeds (the 200-host scenario over four) under every invariant of `Invariant::ALL`
//! (the step invariants after every step, the end invariants once it has settled), and
//! states in its doc comment the row of the design (sections 7 and 8) it covers.
//!
//! Each states only what the server does today and the design also promises; none pins
//! a behaviour that section 8 marks Open. A scenario about a situation shows with a
//! witness that the situation happened in some seed, and two negative cases run a
//! scenario with agents that lack the behaviour it relies on and show it fails.
//!
//! Sizes, in agent requests a seed (heartbeats, with the hellos besides), each at most
//! 30,000:
//! - agents that die: [`DIE_HOSTS`] beating every [`DIE_INTERVAL`] for
//!   [`DIE_SECOND_AT`] plus [`DIE_SETTLE`], 1,420 s, make about 9,500;
//! - agents killed for good: [`GOOD_HOSTS`] beating every 5 s for [`GOOD_KILL_AT`] plus
//!   [`GOOD_SETTLE`], 420 s, make 4,200;
//! - the partition: [`CUT_HOSTS`] beating every 5 s for [`CUT_AT`], [`CUT_FOR`] and
//!   [`CUT_SETTLE`], 350 s, make 7,000;
//! - the lossy wire: [`LOSSY_HOSTS`] beating every 5 s for [`LOSSY_FOR`] plus
//!   [`LOSSY_SETTLE`], 900 s, make 3,600;
//! - the silent host: [`SILENT_HOSTS`] beating every 5 s for about 340 s make 2,040;
//! - the restart: about 3,840 (`cell/server_restart.rs`);
//! - the forgotten host: [`FORGET_HOSTS`] beating every 5 s for about 340 s make 680.

#[path = "cell/server_restart.rs"]
mod server_restart;

use std::time::Duration;

use js_conformance::agent::Defect;
use js_sim::{Chance, Faults};
use js_sim_cell::actions::{
    Agents, agents, drawn_agents, kill_agents, kill_agents_for_good, partition_agents,
    replace_agents, set_bus_faults, submit_work,
};
use js_sim_cell::{
    Counter, Expect, HostIs, HowEnded, Invariant, Scenario, ScenarioFailure, ServerSpec, Shape,
    Work,
};
use js_types::Resources;

/// Every scenario of the family runs over this many seeds.
const SEEDS: u64 = 16;

/// A unit of work that runs for `run` and asks for all the cores of a host
/// ([`Shape::gpu_host`]), so a host runs one at a time and a scenario that counts on one
/// run per host keeps its count.
fn one_per_host(run: Duration) -> Work {
    let host = Resources::new(8_000, 1 << 30, 0, None).unwrap_or_default();
    Work::run_for(run).needs(host)
}

/// Agents that die: two hundred hosts beating every 30 s, held lost after 90 s.
const DIE_HOSTS: usize = 200;
const DIE_INTERVAL: Duration = Duration::from_secs(30);
const DIE_GRACE: Duration = Duration::from_secs(90);
const DIE_RUNS: u32 = 400;
const DIE_RUN: Duration = Duration::from_secs(300);
const DIE_GIVE_UP: Duration = Duration::from_secs(2_400);
/// The ten agents that die, spread over the hosts.
const DIE_AGENTS: [usize; 10] = [0, 20, 40, 60, 80, 100, 120, 140, 160, 180];
const DIE_AT: Duration = Duration::from_secs(75);
const DIE_REPLACE_AT: Duration = Duration::from_secs(95);
const DIE_SECOND_AT: Duration = Duration::from_secs(135);
const DIE_SECOND_RUNS: u32 = 100;
const DIE_SETTLE: Duration = Duration::from_secs(1_300);
/// The 200-host scenario is the family's largest by far, and over the job service a seed
/// of it costs about four CPU minutes, so it runs [`DIE_SEEDS`] seeds, on this many threads,
/// a quarter each.
// TODO(js-placement-pass-cost): restore 16 seeds, the family's [`SEEDS`], once the
// placement pass stops re-reading every active job and re-placing every queued job on
// every heartbeat; that follow-up measures the cost of the 16 seeds before it lands.
const DIE_SEEDS: u64 = 4;
const DIE_THREADS: u64 = 4;

/// Agents killed for good: five of fifty hosts, at the server's default heartbeat.
const GOOD_HOSTS: usize = 50;
const GOOD_RUNS: u32 = 100;
const GOOD_RUN: Duration = Duration::from_secs(60);
const GOOD_GIVE_UP: Duration = Duration::from_secs(300);
const GOOD_DEAD: [usize; 5] = [0, 10, 20, 30, 40];
const GOOD_KILL_AT: Duration = Duration::from_secs(60);
const GOOD_SETTLE: Duration = Duration::from_secs(360);

/// The partition: twenty of a hundred agents cut for 30 s, longer than the server's
/// default grace of 15 s, while their runs end.
const CUT_HOSTS: usize = 100;
const CUT_RUNS: u32 = 100;
/// Launched at about 5 s, so each run ends inside the cut.
const CUT_RUN: Duration = Duration::from_secs(35);
const CUT_AGENTS: usize = 20;
const CUT_AT: Duration = Duration::from_secs(20);
const CUT_FOR: Duration = Duration::from_secs(30);
const CUT_SETTLE: Duration = Duration::from_secs(300);

/// The lossy wire: a fifth of the messages lost and the rest delayed by up to 3 s each
/// way; a worker that takes 7 s to act on a reply, longer than the 5 s interval, so the
/// next heartbeat leaves before it has acted on a start and does not list the job; runs
/// shorter than the interval; a grace of 60 s, so loss alone never holds a host lost.
const LOSSY_HOSTS: usize = 20;
const LOSSY_RUNS: u32 = 200;
const LOSSY_RUN: Duration = Duration::from_secs(2);
const LOSSY_INTERVAL: Duration = Duration::from_secs(5);
const LOSSY_GRACE: Duration = Duration::from_secs(60);
const LOSSY_DROP_PERCENT: u32 = 20;
const LOSSY_MAX_DELAY: Duration = Duration::from_secs(3);
const LOSSY_WORKER_LAG: Duration = Duration::from_secs(7);
const LOSSY_FOR: Duration = Duration::from_secs(600);
const LOSSY_SETTLE: Duration = Duration::from_secs(300);

/// The silent host: ten of thirty agents, drawn from the seed, cut at 7 s, after their
/// starts arrived (about 5 s) and before their next heartbeat (about 10 s), for longer
/// than the grace.
const SILENT_HOSTS: usize = 30;
const SILENT_RUNS: u32 = 20;
const SILENT_RUN: Duration = Duration::from_secs(60);
const SILENT_CUT: usize = 10;
const SILENT_CUT_AT: Duration = Duration::from_secs(7);
const SILENT_CUT_FOR: Duration = Duration::from_secs(30);
const SILENT_SETTLE: Duration = Duration::from_secs(300);

/// The forgotten host: three of ten agents cut, once their runs are done, for longer than
/// the grace and the pool's `lost_retention` together, then healed; a second batch of one
/// run a host after the heal.
const FORGET_HOSTS: usize = 10;
const FORGET_RUNS: u32 = 10;
const FORGET_RUN: Duration = Duration::from_secs(20);
const FORGET_AGENTS: [usize; 3] = [0, 1, 2];
const FORGET_RETENTION: Duration = Duration::from_secs(120);
const FORGET_CUT_AT: Duration = Duration::from_secs(60);
const FORGET_CUT_FOR: Duration = Duration::from_secs(150);
const FORGET_AGAIN_AT: Duration = Duration::from_secs(220);
const FORGET_SETTLE: Duration = Duration::from_secs(120);

/// `scenario` with `count` more hosts of the pool `burst`, whose agents lack `defect`,
/// or correct ones.
fn hosts(scenario: Scenario, count: usize, defect: Option<Defect>) -> Scenario {
    match defect {
        Some(defect) => scenario.broken_hosts("burst", count, defect),
        None => scenario.hosts("burst", count),
    }
}

/// The partition scenario: the first [`CUT_AGENTS`] agents lack `defect`, or are
/// correct, and are the ones cut.
fn partition(defect: Option<Defect>) -> Scenario {
    let scenario = Scenario::new("a_thirty_second_partition_of_twenty_agents_loses_no_result")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            CUT_HOSTS,
        ));
    hosts(scenario, CUT_AGENTS, defect)
        .hosts("burst", CUT_HOSTS - CUT_AGENTS)
        .work(CUT_RUNS, one_per_host(CUT_RUN))
        .at(CUT_AT, partition_agents(agents(0..CUT_AGENTS), CUT_FOR))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            CUT_RUNS as usize,
            HowEnded::Succeeded,
        ))
        .witness("result_resent", |c| c.get(Counter::ResultResent) >= 1)
        .settle_within(CUT_SETTLE)
}

/// The lossy-wire scenario, with agents that lack `defect`, or correct ones.
fn lossy(defect: Option<Defect>) -> Scenario {
    let lossy = Faults {
        min_delay: Duration::from_millis(1),
        max_delay: LOSSY_MAX_DELAY,
        drop: Chance::percent(LOSSY_DROP_PERCENT),
        ..Faults::default()
    };
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), LOSSY_HOSTS)
        .heartbeat(LOSSY_INTERVAL, LOSSY_GRACE);
    let scenario = Scenario::new("a_lossy_wire_launches_every_run_once")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .worker_lag(LOSSY_WORKER_LAG)
        .bus(lossy);
    hosts(scenario, LOSSY_HOSTS, defect)
        .work(LOSSY_RUNS, one_per_host(LOSSY_RUN))
        .at(LOSSY_FOR, set_bus_faults(Faults::default()))
        .expect(Expect::all_items_terminal())
        .witness("start_repeated", |c| c.get(Counter::StartRepeated) >= 1)
        .settle_within(LOSSY_SETTLE)
}

/// Runs `scenario` over [`SEEDS`] and fails unless a seed breaks `invariant`: the
/// scenario's agents lack a behaviour it relies on, so it must go red there.
fn should_fail(scenario: &Scenario, invariant: Invariant) {
    match scenario.try_run_seeds(SEEDS) {
        Err(ScenarioFailure::Violated(report)) => {
            assert_eq!(report.violation.invariant, invariant, "{report}");
        }
        Err(other) => panic!("not a violation: {other}"),
        Ok(passed) => panic!("passed all {} seeds", passed.runs.len()),
    }
}

/// Covers section 7 ("Lost host": it takes no new work; the agent's state dies with it)
/// and section 8, row 4 (an agent restart loses its runs: the new process lists none and
/// the run ends with no result). Two hundred hosts run 400 runs of 300 s; at 75 s ten
/// agents die with their state, and 20 s later a new process on each of those hosts says
/// hello holding nothing (the server refuses it while the host is still live, and seats
/// it once the host is lost). The agents die after the heartbeat that first lists the
/// jobs, which is about 60 s in: a job's start rides the reply to a heartbeat, and the
/// heartbeats are 30 s apart. A second batch of a hundred runs is submitted at 135 s.
/// Proves, with every invariant on, that every item reaches one terminal answer, that
/// nothing is launched twice, booked past a host's capacity or left booked once the
/// clients are done, and that the replaced hosts take work again: each of them launches
/// a run after 135 s (`launched_on`). Red for a server that never seats a replaced host
/// again or keeps it unbookable (`launched-on`), one that keeps the dead process's runs
/// booked after the new process listed none (`settles-when-quiet`), and one that runs a
/// dead process's run again on the new one (`at-most-one-launch`).
#[test]
fn ten_of_two_hundred_agents_die_mid_run() {
    let parts: Vec<_> = (0..DIE_THREADS)
        .map(|part| {
            let seeds = part * DIE_SEEDS / DIE_THREADS..(part + 1) * DIE_SEEDS / DIE_THREADS;
            std::thread::spawn(move || {
                let ran = agents_that_die().try_run_seed_range(seeds);
                ran.map(|_| ()).map_err(|failure| failure.to_string())
            })
        })
        .collect();
    for part in parts {
        match part.join() {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => panic!("{failure}"),
            Err(panicked) => std::panic::resume_unwind(panicked),
        }
    }
}

/// The scenario of [`ten_of_two_hundred_agents_die_mid_run`], built on each of the
/// threads that run its seeds.
fn agents_that_die() -> Scenario {
    let dead = || Agents::Listed(DIE_AGENTS.to_vec());
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), DIE_HOSTS)
        .heartbeat(DIE_INTERVAL, DIE_GRACE);
    let work = one_per_host(DIE_RUN).give_up_after(DIE_GIVE_UP);
    Scenario::new("ten_of_two_hundred_agents_die_mid_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", DIE_HOSTS)
        .work(DIE_RUNS, work.clone())
        .at(DIE_AT, kill_agents(dead()))
        .at(DIE_REPLACE_AT, replace_agents(dead()))
        .at(DIE_SECOND_AT, submit_work(DIE_SECOND_RUNS, work))
        .expect(Expect::all_items_terminal())
        .expect(Expect::launched_on(&DIE_AGENTS, DIE_SECOND_AT))
        .settle_within(DIE_SETTLE)
}

/// Covers section 8, row 41 (a host silent for good keeps its bookings until they are
/// stopped) and section 6.3 (a lost host takes no new work). Five of fifty agents die at
/// 60 s and never return; their hosts read lost, and the clients of the jobs on them
/// give up at their bound and cancel them. Proves, with every invariant on
/// (among them `no-lease-on-a-lost-host`: no item is placed on a host the server lists
/// as lost), that once the clients have cancelled nothing
/// stays booked on the dead hosts (`settles-when-quiet`, `bookings-match-agents`), that
/// every item reaches one terminal answer, that every run not on a dead host succeeds,
/// and that the dead hosts are still listed, lost. Red for a server that places new
/// jobs on a lost host (`no-lease-on-a-lost-host`, and the count of successes, since
/// such an item can never start), and one that keeps a dead host's booking after its
/// job was cancelled (`settles-when-quiet`).
#[test]
fn agents_killed_for_good_leave_no_booking_once_clients_release() {
    let dead = Agents::Listed(GOOD_DEAD.to_vec());
    let lost = GOOD_DEAD.map(|index| Expect::host_state(index, HostIs::Lost));
    let scenario = Scenario::new("agents_killed_for_good_leave_no_booking_once_clients_release")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            GOOD_HOSTS,
        ))
        .hosts("burst", GOOD_HOSTS)
        .work(
            GOOD_RUNS,
            one_per_host(GOOD_RUN).give_up_after(GOOD_GIVE_UP),
        )
        .at(GOOD_KILL_AT, kill_agents_for_good(dead))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            GOOD_RUNS as usize - GOOD_DEAD.len(),
            HowEnded::Succeeded,
        ))
        .settle_within(GOOD_SETTLE);
    lost.into_iter()
        .fold(scenario, Scenario::expect)
        .run_seeds(SEEDS);
}

/// Covers section 7 ("Result lost": the agent holds a result until it is acknowledged
/// and resends it) and section 6.3 (nothing times out for silence alone). Twenty of a
/// hundred agents are cut off for 30 s, longer than the 15 s grace, while their runs
/// end, and the cut heals. Proves, with every invariant on (`no-result-lost` among
/// them), that every result produced during the cut arrives after the heal and is
/// recorded once: each run ends `Succeeded` for its client, no result is accepted twice,
/// and no run is launched twice. The witness shows a result crossed the cut: it was sent
/// again because the first send never got an answer. Red for an agent that forgets an
/// unacknowledged result on a dropped connection (the negative case below), and for a
/// server that ends a lost host's runs for its silence alone (`no-result-lost`, the
/// count of successes).
#[test]
fn a_thirty_second_partition_of_twenty_agents_loses_no_result() {
    partition(None).run_seeds(SEEDS);
}

/// Covers section 8, row 31 (a start is resent until the host lists the run, and only
/// the agent's retired-id list stops a rerun) and section 7 ("Duplicate start": it never
/// runs twice; "Result lost": resent until acknowledged). A fifth of the messages between
/// the agents and the server are lost and the rest delayed by up to 3 s each way for the
/// first 600 s, with runs of 2 s, shorter than the 5 s heartbeat interval. Proves that
/// every run is launched once (`at-most-one-launch`) and every item settles. The witness
/// shows the server sent a start again to a host that already had it: the heartbeat that
/// left while the worker had not yet acted on a start did not list the run. Red for an agent that
/// runs a repeated start (the negative case below), and for a server that starts a run
/// again on a resent result or listing (`at-most-one-launch`).
#[test]
fn a_lossy_wire_launches_every_run_once() {
    lossy(None).run_seeds(SEEDS);
}

/// Covers section 8, row 35 (a host silent while a run is placed or starting: the run is
/// never requeued on silence, because a delivered start may have run). Ten of thirty
/// agents, drawn from the seed, are cut off at 7 s, after their starts arrived and before
/// any heartbeat listed the runs, for 30 s, longer than the grace, and healed. Proves,
/// with the invariants only, that the server never runs a silent host's run on another
/// host (`at-most-one-launch`, `no-double-booking`). The witness shows the situation:
/// some cut agent ran a run no hello or heartbeat of its own had listed. Red for a server
/// that requeues a run on silence.
#[test]
fn a_silent_host_is_never_given_a_second_copy_of_its_run() {
    Scenario::new("a_silent_host_is_never_given_a_second_copy_of_its_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            SILENT_HOSTS,
        ))
        .hosts("burst", SILENT_HOSTS)
        .work(SILENT_RUNS, one_per_host(SILENT_RUN))
        .at(
            SILENT_CUT_AT,
            partition_agents(drawn_agents(SILENT_CUT), SILENT_CUT_FOR),
        )
        .witness("cut_with_unlisted_run", |c| {
            c.get(Counter::CutWithUnlistedRun) >= 1
        })
        .settle_within(SILENT_SETTLE)
        .run_seeds(SEEDS);
}

/// Covers section 8, row 3 and section 6.7 (restart recovery); the scenario and what it
/// proves are in `cell/server_restart.rs`.
#[test]
fn a_restart_of_the_server_with_runs_in_flight_double_books_nothing() {
    server_restart::a_restart_of_the_server_with_runs_in_flight_double_books_nothing()
        .run_seeds(SEEDS);
}

/// Covers section 6.3 (a lost host with no allocation is forgotten after
/// `lost_retention`) and section 6.7, step 5 (a host the server does not know is told
/// `409 unknown-host` and says hello). Three of ten agents, idle, are cut off for 150 s,
/// longer than the 15 s grace and the pool's `lost_retention` of 120 s together, so the
/// server forgets them; the cut heals and a run a host is submitted. Proves, with every
/// invariant on (`no-double-booking` checks at every step that the listing names each
/// host once), that each forgotten host is
/// seated again, listed live at the end, and takes work again (`launched_on`). The
/// witness shows the server had forgotten a host: a heartbeat was answered
/// `unknown-host`. Red for a server that refuses a forgotten host's hello or never books
/// it again (`host-state`, `launched-on`), and one that books a job on two hosts
/// when a host returns (`no-double-booking`).
#[test]
fn a_forgotten_host_that_comes_back_is_seated_once() {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), FORGET_HOSTS)
        .lost_retention(FORGET_RETENTION);
    let forgotten = Agents::Listed(FORGET_AGENTS.to_vec());
    let live = FORGET_AGENTS.map(|index| Expect::host_state(index, HostIs::Live));
    let scenario = Scenario::new("a_forgotten_host_that_comes_back_is_seated_once")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", FORGET_HOSTS)
        .work(FORGET_RUNS, one_per_host(FORGET_RUN))
        .at(FORGET_CUT_AT, partition_agents(forgotten, FORGET_CUT_FOR))
        .at(
            FORGET_AGAIN_AT,
            submit_work(FORGET_RUNS, one_per_host(FORGET_RUN)),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::launched_on(&FORGET_AGENTS, FORGET_AGAIN_AT))
        .witness("unknown_host", |c| c.get(Counter::UnknownHost) >= 1)
        .settle_within(FORGET_SETTLE);
    live.into_iter()
        .fold(scenario, Scenario::expect)
        .run_seeds(SEEDS);
}

/// Asserts that the partition scenario fails `result-kept-until-acknowledged` when the cut
/// agents forget an unacknowledged result when their connection drops
/// (`ForgetsUnackedResult`; the agent node tells its agent the connection dropped for every
/// heartbeat still unanswered when the next is due): a heartbeat leaves without the result
/// of a job that ended during the cut, which no answer had acknowledged.
#[test]
fn a_defective_agent_that_forgets_unacked_results_fails_scenario_three() {
    should_fail(
        &partition(Some(Defect::ForgetsUnackedResult)),
        Invariant::ResultKeptUntilAcknowledged,
    );
}

/// Asserts that the lossy-wire scenario fails `at-most-one-launch` when its agents run a
/// repeated start (`StartRunsTwice`): the start the server sends again, because the
/// heartbeat that left before the worker acted on it did not list the run, runs a second
/// time.
/// With `a_lossy_wire_launches_every_run_once`, whose witness shows such a start arrived,
/// it shows the scenario can fail.
#[test]
fn a_defective_agent_that_runs_a_start_twice_fails_scenario_four() {
    should_fail(
        &lossy(Some(Defect::StartRunsTwice)),
        Invariant::AtMostOneLaunch,
    );
}
