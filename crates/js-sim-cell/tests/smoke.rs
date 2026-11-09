//! The smoke family: small scenarios that prove the whole machine works, the real server
//! with its job service, registering agents on the real wire and the job client, under every
//! invariant of `Invariant::ALL` (the step invariants after every step, the end
//! invariants once each scenario has settled); and the two negative cases of the per-job
//! agents, each with its correct twin, on the smoke shape of the broken-agent family (runs
//! shorter than the heartbeat, workers slower than it, and a bus that loses messages). The scenarios are in
//! `cell/burst.rs`, `cell/lossy_wire.rs` and `cell/partition.rs`, which the sweep
//! (`tests/sweep.rs`) runs too over many more seeds.
//!
//! Each states only what the server does today and the design also promises; none pins
//! a behaviour that section 8 of the design marks Open. A unit of work asks for its cpu
//! and memory and no GPU (see `JobFront`), so a host of the smoke shape runs eight jobs
//! at once.
//!
//! Sizes, in agent requests a seed (heartbeats, with the hellos besides):
//! - a hundred hosts: 2,000 (`cell/burst.rs`), over [`BURST_SEEDS`] seeds;
//! - the lossy wire: about 8,400 (`cell/lossy_wire.rs`), over [`LOSSY_SEEDS`] seeds;
//! - the partition: about 7,700 (`cell/partition.rs`), over [`CUT_SEEDS`] seeds;
//! - each negative case and its twin: two hosts beating every second for about two
//!   minutes, about 250, over [`REPEAT_SEEDS`] seeds.

#[path = "cell/burst.rs"]
mod burst;
#[path = "cell/lossy_wire.rs"]
mod lossy_wire;
#[path = "cell/partition.rs"]
mod partition;

use std::time::Duration;

use js_conformance::agent::Defect;
use js_sim::{Chance, Faults};
use js_sim_cell::actions::set_bus_faults;
use js_sim_cell::{Counter, Expect, Invariant, Scenario, ScenarioFailure, ServerSpec, Shape, Work};

/// The hundred hosts' seeds.
const BURST_SEEDS: u64 = 4;

/// The lossy wire's seeds.
const LOSSY_SEEDS: u64 = 8;

/// The partition's seeds.
const CUT_SEEDS: u64 = 8;

/// The broken-agent family's world: two hosts beating every second, four runs of 200 ms,
/// workers that take 1.5 s to act on a reply, longer than the interval, and a bus that loses
/// 30% of the messages, so that a call is now and then given up after the request limit (a
/// grace of 60 s keeps the host from being held lost meanwhile).
const REPEAT_HOSTS: usize = 2;
const REPEAT_RUNS: u32 = 4;
const REPEAT_RUN: Duration = Duration::from_millis(200);
const REPEAT_WORKER_LAG: Duration = Duration::from_millis(1_500);
const REPEAT_DROP_PERCENT: u32 = 30;
const REPEAT_INTERVAL: Duration = Duration::from_secs(1);
const REPEAT_GRACE: Duration = Duration::from_secs(60);
const REPEAT_SETTLE: Duration = Duration::from_secs(120);
const REPEAT_QUIET_AT: Duration = Duration::from_secs(60);
const REPEAT_SEEDS: u64 = 2;

/// Proves the machine at size with no fault: 400 jobs of 300 s on a hundred hosts that
/// beat every 30 s all succeed, each launched once, no host booked past what it holds, and
/// nothing left booked or held at the end. Red for a server that double-books a host
/// (`no-double-booking`), one that relaunches a job on a resent heartbeat
/// (`at-most-one-launch`), one that books a job twice or reuses an allocation id
/// (`one-allocation-per-job`), one that leaves a booking after the job ends
/// (`bookings-match-agents`), and one that loses or fails jobs (the expected count of
/// successes).
#[test]
fn a_hundred_hosts_settle_four_hundred_runs() {
    burst::a_hundred_hosts_settle_four_hundred_runs().run_seeds(BURST_SEEDS);
}

/// Proves that the client's work settles while one message in ten between the agents and
/// the server is lost for the first 900 s: every item ends exactly once, no run is launched
/// twice, and nothing is left booked or held once the loss has ended; its witness shows a
/// result was sent again because its acknowledgement was lost. Red for a server that
/// relaunches a job it hears about again (`at-most-one-launch`), one that double-books a
/// host when an answer is lost (`no-double-booking`), one that frees an allocation on
/// silence (`freed-only-when-confirmed`), one that stores a resent result twice
/// (`result-kept-until-acknowledged`), and one that keeps a booking after the job ends
/// (`bookings-match-agents`). The scenario is in `cell/lossy_wire.rs`.
#[test]
fn a_lossy_agent_wire_settles_every_run() {
    lossy_wire::a_lossy_agent_wire_settles_every_run().run_seeds(LOSSY_SEEDS);
}

/// Proves that work settles across a partition: ten agents drawn from the seed are cut
/// off for 60 s, longer than the grace, while they run, and healed. What the server does
/// with a job on a host that went lost and came back may differ from the design (section
/// 8, rows 4 and 35), so the scenario states only what both promise, through the
/// invariants: every item is terminal or given up, nothing is launched twice, nothing is
/// double-booked, and nothing is left booked once the cut has healed. Red for a server
/// that books a lost host's capacity to a second job while its first still runs
/// (`no-double-booking`), one that starts a job again on a host that comes back
/// (`at-most-one-launch`), and one that keeps a lost host's booking after the job ended
/// (`bookings-match-agents`, `settles-when-quiet`).
#[test]
fn a_partition_of_ten_agents_that_heals_settles_every_run() {
    partition::a_partition_of_ten_agents_that_heals_settles_every_run().run_seeds(CUT_SEEDS);
}

/// The broken-agent family's smoke shape: agents lacking `defect`, or
/// correct ones, and every item expected terminal.
fn lagging_lossy(name: &str, defect: Option<Defect>) -> Scenario {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), REPEAT_HOSTS)
        .heartbeat(REPEAT_INTERVAL, REPEAT_GRACE);
    let lossy = Faults {
        drop: Chance::percent(REPEAT_DROP_PERCENT),
        ..Faults::default()
    };
    let scenario = Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .worker_lag(REPEAT_WORKER_LAG)
        .bus(lossy)
        .at(REPEAT_QUIET_AT, set_bus_faults(Faults::default()))
        .work(REPEAT_RUNS, Work::run_for(REPEAT_RUN))
        .expect(Expect::all_items_terminal())
        .settle_within(REPEAT_SETTLE);
    match defect {
        Some(defect) => scenario.broken_hosts("burst", REPEAT_HOSTS, defect),
        None => scenario.hosts("burst", REPEAT_HOSTS),
    }
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
    let broken = lagging_lossy(name, Some(Defect::ForgetsUnackedResult));
    assert_eq!(violated(&broken), Invariant::ResultKeptUntilAcknowledged);
    lagging_lossy(name, None)
        .witness("result_resent", |c| c.get(Counter::ResultResent) >= 1)
        .run_seeds(REPEAT_SEEDS);
}

/// Proves the start half of the per-job agent: the server sends a job's start again while
/// the agent's worker has not yet acted on it, so its heartbeats do not list the job; agents
/// lacking `StartRunsTwice`'s fix launch the repeated start and fail `at-most-one-launch`,
/// and correct agents pass with the witness `start_repeated` held. Catches an agent that
/// relaunches a repeated job start, and a world in which no start is ever repeated.
#[test]
fn a_defective_agent_that_relaunches_a_repeated_jobstart_fails_at_most_one_launch() {
    let name = "a_defective_agent_that_relaunches_a_repeated_jobstart";
    let broken = lagging_lossy(name, Some(Defect::StartRunsTwice));
    assert_eq!(violated(&broken), Invariant::AtMostOneLaunch);
    lagging_lossy(name, None)
        .witness("start_repeated", |c| c.get(Counter::StartRepeated) >= 1)
        .run_seeds(REPEAT_SEEDS);
}
