//! The smoke family over both fronts: every smoke scenario of the core (`smoke.rs`), run
//! once over the lease front and once over the job front ([`Scenario::over_jobs`]: the
//! server serves the job service too, its agents speak the per-job messages, and the client
//! submits each unit of work as a job), under every invariant of `Invariant::ALL`; and the
//! two negative cases of the per-job agents, each with its correct twin, on the smoke
//! shape of the broken-agent family (runs shorter than the heartbeat on a bus slower than
//! it) over the job front.
//!
//! The scenarios state what the server and the design both promise. Over the job front a
//! unit of work asks for its cpu and memory and no GPU (see `JobFront`), so a host of the
//! smoke shape runs eight jobs at once where it runs one lease.
//!
//! Sizes, in agent requests a seed:
//! - a hundred hosts beating every 30 s for 3,600 s: 12,000, over [`BURST_SEEDS`] seeds
//!   per front; over the job front its quiet end is [`BURST_JOBS_SETTLE`] rather than an
//!   hour, which keeps the end check (every run ends long before) and leaves out the hour
//!   of a hundred hosts beating with nothing to do, each of whose heartbeats the server
//!   answers with a read of the job tables;
//! - the lossy wire: about 8,400 (`cell/lossy_wire.rs`), over [`LOSSY_SEEDS`] seeds per
//!   front;
//! - the partition: twenty hosts beating every 5 s for 1,920 s, about 7,700, over
//!   [`CUT_SEEDS`] seeds per front;
//! - each negative case and its twin: two hosts beating every second for about two
//!   minutes, about 250, over [`REPEAT_SEEDS`] seeds.

#[path = "cell/lossy_wire.rs"]
mod lossy_wire;

use std::time::Duration;

use js_conformance::agent::Defect;
use js_sim::Faults;
use js_sim_cell::actions::{drawn_agents, partition_agents};
use js_sim_cell::{
    Counter, Expect, HowEnded, Invariant, Scenario, ScenarioFailure, ServerSpec, Shape, Work,
};

/// A hundred hosts, four hundred runs of 300 s, as `smoke.rs` has them.
const BURST_HOSTS: usize = 100;
const BURST_RUNS: u32 = 400;
const BURST_RUN: Duration = Duration::from_secs(300);
const BURST_GIVE_UP: Duration = Duration::from_secs(2_400);
const BURST_INTERVAL: Duration = Duration::from_secs(30);
const BURST_GRACE: Duration = Duration::from_secs(90);
const BURST_SETTLE: Duration = Duration::from_secs(3_600);
const BURST_JOBS_SETTLE: Duration = Duration::from_secs(600);
const BURST_SEEDS: u64 = 1;

/// The lossy wire's seeds.
const LOSSY_SEEDS: u64 = 1;

/// The partition, as `smoke.rs` has it: ten of twenty agents cut for longer than the grace.
const CUT_HOSTS: usize = 20;
const CUT_RUNS: u32 = 60;
const CUT_RUN: Duration = Duration::from_secs(120);
const CUT_GIVE_UP: Duration = Duration::from_secs(1_800);
const CUT_AGENTS: usize = 10;
const CUT_AT: Duration = Duration::from_secs(60);
const CUT_FOR: Duration = Duration::from_secs(60);
const CUT_SETTLE: Duration = Duration::from_secs(1_800);
const CUT_SEEDS: u64 = 1;

/// The broken-agent family's world: two hosts beating every second, a grace of 3 s, four
/// runs of 200 ms, and a bus that delays every message 1.5 s, longer than the interval.
const REPEAT_HOSTS: usize = 2;
const REPEAT_RUNS: u32 = 4;
const REPEAT_RUN: Duration = Duration::from_millis(200);
const REPEAT_DELAY: Duration = Duration::from_millis(1_500);
const REPEAT_INTERVAL: Duration = Duration::from_secs(1);
const REPEAT_GRACE: Duration = Duration::from_secs(3);
const REPEAT_SETTLE: Duration = Duration::from_secs(120);
const REPEAT_SEEDS: u64 = 2;

/// Which front a scenario runs over.
#[derive(Clone, Copy)]
enum Over {
    Leases,
    Jobs,
}

/// Both fronts.
const FRONTS: [Over; 2] = [Over::Leases, Over::Jobs];

/// `scenario` over `front`.
fn over(scenario: Scenario, front: Over) -> Scenario {
    match front {
        Over::Leases => scenario,
        Over::Jobs => scenario.over_jobs(),
    }
}

/// `smoke.rs`'s hundred hosts settling four hundred runs.
fn a_hundred_hosts_settle_four_hundred_runs() -> Scenario {
    Scenario::new("a_hundred_hosts_settle_four_hundred_runs")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(
            ServerSpec::registered_pool("burst", Shape::gpu_host(), BURST_HOSTS)
                .heartbeat(BURST_INTERVAL, BURST_GRACE),
        )
        .hosts("burst", BURST_HOSTS)
        .work(
            BURST_RUNS,
            Work::run_for(BURST_RUN).give_up_after(BURST_GIVE_UP),
        )
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            BURST_RUNS as usize,
            HowEnded::Succeeded,
        ))
        .settle_within(BURST_SETTLE)
}

/// `smoke.rs`'s partition of ten agents that heals.
fn a_partition_of_ten_agents_that_heals_settles_every_run() -> Scenario {
    Scenario::new("a_partition_of_ten_agents_that_heals_settles_every_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            CUT_HOSTS,
        ))
        .hosts("burst", CUT_HOSTS)
        .work(CUT_RUNS, Work::run_for(CUT_RUN).give_up_after(CUT_GIVE_UP))
        .at(CUT_AT, partition_agents(drawn_agents(CUT_AGENTS), CUT_FOR))
        .expect(Expect::all_items_terminal())
        .settle_within(CUT_SETTLE)
}

/// Proves that every smoke scenario of the core holds over either front: a hundred hosts
/// settle four hundred runs, all succeeded; a lossy agent wire settles every item, with a
/// result sent again because its acknowledgement was lost; and a partition of ten agents
/// that heals settles every item; each under every invariant, the five job invariants too.
/// Over the job front, red for a server that books a job twice or reuses an allocation id
/// (`one-allocation-per-job`), frees an allocation on silence (`freed-only-when-confirmed`),
/// sends a start before it commits the allocation (`no-start-without-allocation`), stores a
/// resent result twice
/// (`result-kept-until-acknowledged`), or loses or fails jobs (the expected count of
/// successes).
///
/// The six runs are independent cells, each built and run on a thread of its own, so they
/// overlap in time; a run that fails panics its thread, and the scope fails the test.
#[test]
fn smoke_scenarios_run_over_the_job_front() {
    std::thread::scope(|scope| {
        for front in FRONTS {
            scope.spawn(move || {
                let burst = over(a_hundred_hosts_settle_four_hundred_runs(), front);
                let burst = match front {
                    Over::Leases => burst,
                    Over::Jobs => burst.settle_within(BURST_JOBS_SETTLE),
                };
                burst.run_seeds(BURST_SEEDS);
            });
            scope.spawn(move || {
                let lossy = lossy_wire::a_lossy_agent_wire_settles_every_run();
                over(lossy, front).run_seeds(LOSSY_SEEDS);
            });
            scope.spawn(move || {
                let cut = a_partition_of_ten_agents_that_heals_settles_every_run();
                over(cut, front).run_seeds(CUT_SEEDS);
            });
        }
    });
}

/// The broken-agent family's smoke shape over the job front: agents lacking `defect`, or
/// correct ones, and every item expected terminal.
fn slow_bus_over_jobs(name: &str, defect: Option<Defect>) -> Scenario {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), REPEAT_HOSTS)
        .heartbeat(REPEAT_INTERVAL, REPEAT_GRACE);
    let slow = Faults {
        min_delay: REPEAT_DELAY,
        max_delay: REPEAT_DELAY,
        ..Faults::default()
    };
    let scenario = Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .bus(slow)
        .work(REPEAT_RUNS, Work::run_for(REPEAT_RUN))
        .expect(Expect::all_items_terminal())
        .settle_within(REPEAT_SETTLE);
    let scenario = match defect {
        Some(defect) => scenario.broken_hosts("burst", REPEAT_HOSTS, defect),
        None => scenario.hosts("burst", REPEAT_HOSTS),
    };
    scenario.over_jobs()
}

/// The invariant a scenario's first failing seed violated; a failure of another kind fails
/// the test, naming it.
fn violated(scenario: &Scenario) -> Invariant {
    match scenario.try_run_seeds(REPEAT_SEEDS) {
        Err(ScenarioFailure::Violated(report)) => report.violation.invariant,
        other => panic!(
            "wanted a violation, got {:?}",
            other.err().map(|f| f.to_string())
        ),
    }
}

/// Proves the result half of the per-job agent: agents lacking `ForgetsUnackedResult`'s
/// fix, which drop the results they hold when an answer is late, fail
/// `result-kept-until-acknowledged` (a heartbeat leaves without a result no answer had
/// acknowledged), and correct agents in the same world pass with a result sent again
/// (witness `result_resent`). Catches an agent, or a node, that lets a result go before
/// the server acknowledged it, and a `result-kept-until-acknowledged` that never fires.
#[test]
fn a_defective_agent_that_drops_an_unacked_result_fails_result_kept_until_acknowledged() {
    let name = "a_defective_agent_that_drops_an_unacked_result";
    let broken = slow_bus_over_jobs(name, Some(Defect::ForgetsUnackedResult));
    assert_eq!(violated(&broken), Invariant::ResultKeptUntilAcknowledged);
    slow_bus_over_jobs(name, None)
        .witness("result_resent", |c| c.get(Counter::ResultResent) >= 1)
        .run_seeds(REPEAT_SEEDS);
}

/// Proves the start half of the per-job agent: the server sends a job's start again while
/// the agent's heartbeats that do not list the job are still on the slow bus; agents
/// lacking `StartRunsTwice`'s fix launch the repeated start and fail `at-most-one-launch`,
/// and correct agents pass with the witness `start_repeated` held. Catches an agent that
/// relaunches a repeated job start, and a world in which no start is ever repeated.
#[test]
fn a_defective_agent_that_relaunches_a_repeated_jobstart_fails_at_most_one_launch() {
    let name = "a_defective_agent_that_relaunches_a_repeated_jobstart";
    let broken = slow_bus_over_jobs(name, Some(Defect::StartRunsTwice));
    assert_eq!(violated(&broken), Invariant::AtMostOneLaunch);
    slow_bus_over_jobs(name, None)
        .witness("start_repeated", |c| c.get(Counter::StartRepeated) >= 1)
        .run_seeds(REPEAT_SEEDS);
}
