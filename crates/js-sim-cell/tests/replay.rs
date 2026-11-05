//! Whether a seed replays: the real server, twenty agents beating every five seconds, one
//! hand-made lease and run, 10% of the agent wire's messages dropped, and ten agents cut
//! off for thirty seconds before the cut heals.
//!
//! A run is a function of its seed. These tests decide whether it is: the same seed twice
//! in one process, two seeds against each other, and pinned seeds against the golden file
//! (each test binary runs in its own process, so the golden file also pins the hash across
//! processes). The failure report and the stop at the first violation are shown with a
//! violation the test makes itself, through the run loop's test hook.
//!
//! Size: [`HOSTS`] agents over [`HORIZON_MS`] at [`INTERVAL`] make about
//! `HOSTS * HORIZON_MS / INTERVAL` = 480 heartbeats a seed, with the hellos besides; the
//! golden test runs [`SEEDS`] seeds.
//!
//! The golden file also pins one smoke scenario, family `smoke`: the lossy agent wire
//! (`cell/lossy_wire.rs`, about 8,400 agent requests a seed) over [`SMOKE_SEEDS`] seeds, so
//! a change to any decision of the client driver, the scenario builder or the server as
//! the smoke family drives it moves a hash.
//!
//! It also pins the first whole-host scenario, family `whole_host`: fifty whole-node
//! requests over twenty hosts (`cell/whole_host.rs`, about 4,800 agent requests a seed)
//! over [`WHOLE_SEEDS`] seeds under each value of `--placement`, so a change to the
//! agents' side of the whole-host handshake, the client's whole-host path or the server's
//! whole-host decisions under any placement moves a hash; the replay test runs one of its
//! seeds twice under each placement.
//!
//! And one scenario of the whole-host fault family, family `whole_host_faults`: the
//! cancelled whole-host request (`cell/whole_host_cancel.rs`, one host, about 180 agent
//! requests a seed) over [`WHOLE_FAULTS_SEEDS`] seeds under each value of `--placement`,
//! so a change to how the server drops a cancelled request's place or mark, or to the
//! action that cancels it once it is marked, moves a hash; the replay test runs one of
//! its seeds twice under each placement.
//!
//! And one scenario with real agents, family `real_agent`: the agreement family's world
//! without faults (`cell/real_agents.rs`, six real agents, about 1,200 agent requests a
//! seed) over [`REAL_SEEDS`] seeds, so a change to a decision of the real agent lifecycle
//! as the cell runs it, or a source of nondeterminism in it, moves a hash.
//!
//! And one fault scenario, family `faults`: the restart of the server with runs in flight
//! (`cell/server_restart.rs`, about 3,840 agent requests a seed) over [`FAULTS_SEEDS`]
//! seeds, so a restart, which drops the server and opens its lease file again, is shown
//! to replay from its seed.

#[path = "cell/lossy_wire.rs"]
mod lossy_wire;
#[path = "cell/real_agents.rs"]
mod real_agents;
#[path = "cell/server_restart.rs"]
mod server_restart;
#[path = "cell/support.rs"]
mod support;
#[path = "cell/whole_host.rs"]
mod whole_host;
#[path = "cell/whole_host_cancel.rs"]
mod whole_host_cancel;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use clap::ValueEnum;
use js_lease_server::{Placement, StartupError};
use js_sim::golden::GoldenFile;
use js_sim::{Chance, Faults, TraceHash};
use js_sim_cell::replay::REPORT_LINES;
use js_sim_cell::{
    Cell, Failure, FailureReport, Invariant, Observations, ServerSpec, Setup, Shape, Side,
    Violation, run,
};

use crate::support::{lease, ms, scratch, start_run};

/// How many agents the cell runs.
const HOSTS: usize = 20;

/// How long each seed runs, in virtual milliseconds.
const HORIZON_MS: u64 = 120_000;

/// How often the agents beat: the server's default, which the spec keeps.
const INTERVAL: Duration = Duration::from_secs(5);

/// How many seeds the golden file pins.
const SEEDS: u64 = 16;

/// The scenario's name, in reports and in the golden file.
const SCENARIO: &str = "replay";

/// When the client makes its lease and run, and how long the run takes.
const LEASE_AT_MS: u64 = 20_000;
const RUN_MS: u64 = 10_000;

/// The agents cut off, and when the cut starts and heals.
const CUT: [usize; 10] = [10, 11, 12, 13, 14, 15, 16, 17, 18, 19];
const CUT_AT_MS: u64 = 40_000;
const HEAL_AT_MS: u64 = 70_000;

/// The check a test makes fail.
const FAIL_AT_CHECK: u64 = 100;

/// How many seeds of the smoke scenario the golden file pins, and the family they are
/// kept under.
const SMOKE_SEEDS: u64 = 4;
const SMOKE_FAMILY: &str = "smoke";

/// How many seeds of the whole-host scenario the golden file pins under each placement,
/// and the family they are kept under.
const WHOLE_SEEDS: u64 = 2;
const WHOLE_FAMILY: &str = "whole_host";

/// How many seeds of the cancelled whole-host request the golden file pins under each
/// placement, and the family they are kept under.
const WHOLE_FAULTS_SEEDS: u64 = 2;
const WHOLE_FAULTS_FAMILY: &str = "whole_host_faults";

/// How many seeds of the scenario with real agents the golden file pins, and the family
/// they are kept under.
const REAL_SEEDS: u64 = 2;
const REAL_FAMILY: &str = "real_agent";

/// How many seeds of the fault scenario the golden file pins, and the family they are
/// kept under.
const FAULTS_SEEDS: u64 = 4;
const FAULTS_FAMILY: &str = "faults";

/// The server of the scenario.
fn spec() -> ServerSpec {
    ServerSpec::registered_pool("burst", Shape::gpu_host(), HOSTS)
}

/// The scenario for `setup`.
fn build(setup: Setup) -> Result<Cell, StartupError> {
    let faults = Faults {
        drop: Chance::percent(10),
        ..Faults::default()
    };
    let mut cell = Cell::new(setup, spec(), scratch(SCENARIO), faults)?;
    for _ in 0..HOSTS {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell.at(ms(LEASE_AT_MS), |cell| {
        if let Some(lease) = lease(cell) {
            start_run(cell, &lease, RUN_MS);
        }
    });
    cell.at(ms(CUT_AT_MS), |cell| cell.partition(&[&CUT]));
    cell.at(ms(HEAL_AT_MS), Cell::heal);
    Ok(cell)
}

/// The scenario for `setup`, with a violation made at check [`FAIL_AT_CHECK`], and every
/// check the hook was called at written into `seen`.
fn failing(setup: Setup, seen: &Rc<RefCell<Vec<u64>>>) -> Result<Cell, StartupError> {
    let mut cell = build(setup)?;
    let seen = Rc::clone(seen);
    cell.check_also(move |check, observations: &Observations| {
        seen.borrow_mut().push(check);
        (check == FAIL_AT_CHECK).then(|| Violation {
            invariant: Invariant::AtMostOneLaunch,
            at: observations.at,
            detail: "made by the test".to_owned(),
        })
    });
    Ok(cell)
}

/// The hash of `seed`'s run to the horizon, which must hold.
fn hash(seed: u64) -> TraceHash {
    match run(SCENARIO, seed, ms(HORIZON_MS), &build) {
        Ok(cell) => cell.trace_hash(),
        Err(failure) => panic!("seed {seed} failed: {failure:?}"),
    }
}

/// The report of a run that fails at [`FAIL_AT_CHECK`].
fn report(seed: u64, seen: &Rc<RefCell<Vec<u64>>>) -> FailureReport {
    match run(SCENARIO, seed, ms(HORIZON_MS), &|setup| {
        failing(setup, seen)
    }) {
        Err(Failure::Violated(report)) => *report,
        Err(Failure::Start(err)) => panic!("the cell did not start: {err}"),
        Ok(_) => panic!("the made violation did not stop the run"),
    }
}

/// Asserts that one seed run twice in one process gives one hash, and that a run that
/// keeps its trace lines gives the same hash as one that keeps only the hash; and that the
/// run did what the scenario asks (the lease's run launched once, its agent listed live at
/// the end), so the hash covers a run that reached the server. The same for a seed of the
/// scenario with real agents, run twice, whose items all succeed, and for one seed of the
/// whole-host scenario and one of the cancelled whole-host request, each run twice under
/// each placement. Catches a handler path that reads a
/// clock, iterates a hashed collection to decide an outcome, or lets a random value reach
/// the trace (in the server, the fake agent node or the real agent lifecycle); and a trace
/// whose lines depend on whether they are kept. Also that a seed of the fault scenario of
/// a server restart, run twice, gives one hash.
#[test]
fn a_seed_replays_to_the_same_trace_hash() {
    assert_eq!(spec().heartbeat_interval(), INTERVAL);
    let first = run(SCENARIO, 7, ms(HORIZON_MS), &build).expect("seed 7 runs");
    assert_eq!(first.trace_hash(), hash(7));
    let mut kept = build(Setup::new(7).with_trace_lines()).expect("the cell starts");
    kept.run_until(ms(HORIZON_MS)).expect("seed 7 runs");
    assert_eq!(kept.trace_hash(), first.trace_hash());
    let lines = kept.trace_lines().expect("lines are kept");
    assert_eq!(lines[0], "kbf-trace-format 1 sim");
    assert!(lines.iter().any(|line| line.ends_with(" cell launch h000")));
    assert_eq!(first.ledger().launches().len(), 1);
    let listed = &first.observations().server.hosts;
    assert_eq!(listed.len(), HOSTS);
    assert!(
        listed.values().all(|host| !host.lost),
        "every host is live at the end"
    );
    for placement in Placement::value_variants() {
        let scenario = whole_host::fifty_whole_host_requests(*placement);
        let once = scenario.run_seeds(1).runs[0].hash;
        let again = scenario.run_seeds(1).runs[0].hash;
        assert_eq!(once, again, "{}", whole_host::spelled(*placement));
        let cancel = whole_host_cancel::a_cancelled_request(*placement);
        let once = cancel.run_seeds(1).runs[0].hash;
        let again = cancel.run_seeds(1).runs[0].hash;
        assert_eq!(once, again, "{}", whole_host::spelled(*placement));
    }

    let real = real_agents::without_faults(Side::Real);
    let once = real.run_seeds(1).runs.remove(0);
    let again = real.run_seeds(1).runs.remove(0);
    assert_eq!(once.hash, again.hash);
    assert_eq!(once.outcomes.len(), real_agents::RUNS as usize);
    let succeeded = Some(js_sim_cell::HowEnded::Succeeded);
    assert!(
        once.outcomes
            .values()
            .all(|item| item.terminal == succeeded),
        "{:?}",
        once.outcomes
    );

    let restart = server_restart::a_restart_of_the_server_with_runs_in_flight_double_books_nothing;
    let once = restart().run_seeds(1).runs[0].hash;
    assert_eq!(restart().run_seeds(1).runs[0].hash, once);
}

/// Asserts that two seeds give two hashes. Catches a seed that never reaches the bus or
/// the agents.
#[test]
fn different_seeds_make_different_traces() {
    assert_ne!(hash(1), hash(2));
}

fn golden() -> GoldenFile {
    golden_of("js-sim-cell")
}

/// The lines of `family` in the golden file.
fn golden_of(family: &str) -> GoldenFile {
    GoldenFile::new(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/trace-hashes.txt"),
        family,
        "cargo test -p js-sim-cell --test replay -- --ignored regenerate_golden_hashes",
    )
}

/// The hash of each pinned seed of the smoke scenario, which must pass.
fn smoke_runs() -> Vec<(String, TraceHash)> {
    let scenario = lossy_wire::a_lossy_agent_wire_settles_every_run();
    let passed = scenario.run_seeds(SMOKE_SEEDS);
    let each = |run: &js_sim_cell::SeedRun| {
        let name = format!("a_lossy_agent_wire_settles_every_run/seed-{}", run.seed);
        (name, run.hash)
    };
    passed.runs.iter().map(each).collect()
}

/// The hash of each pinned seed of the scenario with real agents, which must pass.
fn real_runs() -> Vec<(String, TraceHash)> {
    let passed = real_agents::without_faults(Side::Real).run_seeds(REAL_SEEDS);
    let each = |run: &js_sim_cell::SeedRun| (format!("without_faults/seed-{}", run.seed), run.hash);
    passed.runs.iter().map(each).collect()
}

/// The hash of each pinned seed of the fault scenario, which must pass.
fn faults_runs() -> Vec<(String, TraceHash)> {
    let scenario =
        server_restart::a_restart_of_the_server_with_runs_in_flight_double_books_nothing();
    let passed = scenario.run_seeds(FAULTS_SEEDS);
    let each = |run: &js_sim_cell::SeedRun| {
        let name = format!(
            "a_restart_of_the_server_with_runs_in_flight_double_books_nothing/seed-{}",
            run.seed
        );
        (name, run.hash)
    };
    passed.runs.iter().map(each).collect()
}

fn golden_runs() -> Vec<(String, TraceHash)> {
    let each = |seed| (format!("{SCENARIO}/seed-{seed}"), hash(seed));
    (0..SEEDS).map(each).collect()
}

/// Asserts that the hashes of the pinned seeds equal the golden file's, those of the
/// replay scenario and those of the fault scenario of a server restart (family
/// `faults`). Catches any change to a decision of the server or the agents, or to the
/// trace text, a restart whose run depends on anything but its seed (the files it opens
/// again, the order it reads them in), and, because the file was written by another
/// process, a run that differs between processes.
#[test]
fn pinned_seeds_match_the_golden_file() {
    golden().assert_matches(&golden_runs());
    golden_of(FAULTS_FAMILY).assert_matches(&faults_runs());
}

/// Asserts that the hashes of the pinned seeds of the lossy-wire smoke scenario equal the
/// golden file's (family `smoke`). Catches any change to a decision of the client driver,
/// of the scenario builder or of the server under it, or to the trace text, and a
/// scenario whose run differs between processes.
#[test]
fn the_smoke_scenario_matches_the_golden_file() {
    golden_of(SMOKE_FAMILY).assert_matches(&smoke_runs());
}

/// Asserts that the hashes of the pinned seeds of the scenario with real agents equal the
/// golden file's (family `real_agent`). Catches any change to a decision of the real agent
/// lifecycle or the real agent node, or to the trace text, and a run with real agents that
/// differs between processes.
#[test]
fn the_real_agent_scenario_matches_the_golden_file() {
    golden_of(REAL_FAMILY).assert_matches(&real_runs());
}

#[test]
#[ignore = "regenerates the golden trace hashes; run it only on purpose"]
fn regenerate_golden_hashes() {
    golden().regenerate(&golden_runs()).unwrap();
    golden_of(SMOKE_FAMILY).regenerate(&smoke_runs()).unwrap();
    golden_of(WHOLE_FAMILY).regenerate(&whole_runs()).unwrap();
    golden_of(WHOLE_FAULTS_FAMILY)
        .regenerate(&whole_faults_runs())
        .unwrap();
    golden_of(REAL_FAMILY).regenerate(&real_runs()).unwrap();
    golden_of(FAULTS_FAMILY).regenerate(&faults_runs()).unwrap();
}

/// The hash of each pinned seed of the whole-host scenario under each placement, which
/// must pass.
fn whole_runs() -> Vec<(String, TraceHash)> {
    let mut runs = Vec::new();
    for placement in Placement::value_variants() {
        let scenario = whole_host::fifty_whole_host_requests(*placement);
        let placement = whole_host::spelled(*placement);
        for run in scenario.run_seeds(WHOLE_SEEDS).runs {
            runs.push((format!("{placement}/seed-{}", run.seed), run.hash));
        }
    }
    runs
}

/// Asserts that the hashes of the pinned seeds of the whole-host scenario, under every
/// placement, equal the golden file's (family `whole_host`). Catches any change to a
/// decision of the agents' whole-host handshake, of the client's whole-host path or of
/// the server's whole-host path under a placement, or to the trace text, and a run that
/// differs between processes.
#[test]
fn the_whole_host_scenario_matches_the_golden_file_under_every_placement() {
    golden_of(WHOLE_FAMILY).assert_matches(&whole_runs());
}

/// The hash of each pinned seed of the cancelled whole-host request under each placement,
/// which must pass.
fn whole_faults_runs() -> Vec<(String, TraceHash)> {
    let mut runs = Vec::new();
    for placement in Placement::value_variants() {
        let scenario = whole_host_cancel::a_cancelled_request(*placement);
        let placement = whole_host::spelled(*placement);
        for run in scenario.run_seeds(WHOLE_FAULTS_SEEDS).runs {
            runs.push((format!("{placement}/seed-{}", run.seed), run.hash));
        }
    }
    runs
}

/// Asserts that the hashes of the pinned seeds of the cancelled whole-host request, under
/// every placement, equal the golden file's (family `whole_host_faults`). Catches any
/// change to how the server drops a cancelled whole-host request's place and mark under
/// a placement, to the client's cancel of a pending lease, or to the trace text, and a
/// run that differs between processes.
#[test]
fn the_whole_host_fault_scenario_matches_the_golden_file_under_every_placement() {
    golden_of(WHOLE_FAULTS_FAMILY).assert_matches(&whole_faults_runs());
}

/// Asserts that a run stopped by a violation reports the scenario, the seed, the trace
/// hash, the violation and the last forty trace lines, the last of them the violation;
/// that the run again with trace lines kept has the first run's hash; and that the report
/// as text holds every part. Catches a re-run that diverges from the first (a source of
/// nondeterminism the seed does not cover) and a report that loses a part.
#[test]
fn a_failure_report_names_the_scenario_the_seed_the_hash_and_the_last_forty_trace_lines() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let report = report(5, &seen);
    assert_eq!(report.scenario, SCENARIO);
    assert_eq!(report.seed, 5);
    assert_eq!(report.replay_hash, report.hash);
    assert_eq!(report.violation.invariant, Invariant::AtMostOneLaunch);
    assert_eq!(report.violation.detail, "made by the test");
    assert_eq!(report.check, FAIL_AT_CHECK);
    assert_eq!(report.last_lines.len(), REPORT_LINES);
    let last = report.last_lines.last().expect("forty lines");
    assert!(
        last.ends_with(" cell violation at-most-one-launch"),
        "{last}"
    );
    let text = report.to_string();
    let heading = format!(
        "scenario {SCENARIO} seed 5 failed at check {FAIL_AT_CHECK}: {}",
        report.violation
    );
    assert!(text.starts_with(&heading), "{text}");
    let hashes = format!("trace hash {} (replayed: {})", report.hash, report.hash);
    assert!(text.contains(&hashes), "{text}");
    assert!(
        text.contains(&format!("last {REPORT_LINES} trace lines:\n")),
        "{text}"
    );
    assert!(text.ends_with(&format!("  {last}\n")), "{text}");
}

/// Asserts that a violation made at check N stops the run at check N, with every check
/// from 1 to N made in order (one per step), that the cell's clock stopped there, and that
/// running on returns the same violation without another check. Catches a check that is
/// batched or skipped, and a run that goes on past its first violation.
#[test]
fn the_run_loop_stops_at_the_first_violation_and_checks_after_every_step() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut cell = failing(Setup::new(3), &seen).expect("the cell starts");
    let violation = cell
        .run_until(ms(HORIZON_MS))
        .expect_err("the made violation");
    assert_eq!(violation.detail, "made by the test");
    assert_eq!(cell.checks(), FAIL_AT_CHECK);
    let every: Vec<u64> = (1..=FAIL_AT_CHECK).collect();
    assert_eq!(*seen.borrow(), every);
    let stopped = cell.now();
    assert_eq!(violation.at, stopped);
    assert!(
        stopped < ms(LEASE_AT_MS),
        "stopped at {}",
        stopped.as_millis()
    );
    assert_eq!(cell.run_until(ms(HORIZON_MS)), Err(violation));
    assert_eq!(cell.checks(), FAIL_AT_CHECK);
    assert_eq!(cell.now(), stopped);
    assert_eq!(seen.borrow().len(), every.len());
}

/// Asserts that a cell whose server refuses its configuration (a pool of no machines) is
/// reported as a failure to start, naming the pool file, and that a shape that is not a
/// host (a GPU with no class) is refused before the server starts. Catches a refusal that
/// is lost or reported as a violation.
#[test]
fn a_server_that_refuses_its_configuration_is_a_failure_to_start() {
    let empty = |setup: Setup| {
        // A burst of one, so the flags parse and the pool file is what is refused.
        let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), 0).hello_burst(1);
        Cell::new(setup, spec, scratch("empty"), Faults::default())
    };
    match run(SCENARIO, 0, ms(HORIZON_MS), &empty) {
        Err(Failure::Start(err)) => assert_eq!(err.flag(), "--agent-pool-config", "{err}"),
        Err(other) => panic!("not a failure to start: {other:?}"),
        Ok(_) => panic!("a pool of no machines started"),
    }
    let shape = Shape {
        gpu_class: None,
        ..Shape::gpu_host()
    };
    let spec = ServerSpec::registered_pool("burst", shape, 1);
    let refused = Cell::new(Setup::new(0), spec, scratch("shape"), Faults::default());
    assert_eq!(refused.err().map(|err| err.flag()), Some("shape"));
}
