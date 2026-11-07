//! Whether a seed replays: the real server, twenty agents beating every five seconds, one
//! hand-made job, 10% of the agent wire's messages dropped, and ten agents cut off for
//! thirty seconds before the cut heals.
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
//! It also pins the first whole-host scenario, family `whole_host`: fifty whole-host
//! jobs over twenty hosts (`cell/whole_host.rs`, about 4,800 agent requests a seed) over
//! [`WHOLE_SEEDS`] seeds, so a change to the client's whole-host path or the server's
//! whole-host decisions moves a hash; the replay test runs one of its seeds twice.
//!
//! And one scenario of the whole-host fault family, family `whole_host_faults`: the
//! cancelled whole-host job (`cell/whole_host_cancel.rs`, one host, about 180 agent
//! requests a seed) over [`WHOLE_FAULTS_SEEDS`] seeds, so a change to how the server drops
//! a cancelled job's place moves a hash; the replay test runs one of its seeds twice.
//!
//! And one scenario with real agents, family `real_agent`: the agreement family's world
//! without faults (`cell/real_agents.rs`, six real agents, about 1,200 agent requests a
//! seed) over [`REAL_SEEDS`] seeds, so a change to a decision of the real agent lifecycle
//! as the cell runs it, or a source of nondeterminism in it, moves a hash.
//!
//! And one fault scenario, family `faults`: the restart of the server with runs in flight
//! (`cell/server_restart.rs`, about 3,840 agent requests a seed) over [`FAULTS_SEEDS`]
//! seeds, so a restart, which drops the server and opens its files again, is shown to
//! replay from its seed.
//!
//! And one job smoke scenario, family `jobs`: jobs on a lossy agent wire
//! (`cell/jobs.rs`, eight hosts, about 960 agent requests a seed) over [`JOBS_SEEDS`] seed,
//! so a change to a decision of the job front, the per-job agents or the server's job
//! service and job pass, or a job or allocation id that does not follow the seed, moves a
//! hash. The ids themselves are compared between two runs of one seed, and between two
//! seeds, in the cell's ledger, where each job's launch is named by its job and allocation
//! ids.
//!
//! And one elastic scenario, family `elastic`: an EC2-like pool scaled up to forty machines
//! by a hundred whole-host runs and back to none (`cell/elastic.rs`, about 19,200 agent
//! requests a seed) over [`ELASTIC_SEEDS`] seed, so a change to a decision of the scaler,
//! to how the cell boots, joins or ends a launched machine, or a machine name that does
//! not follow the seed, moves a hash.
//!
//! And one store scenario, family `store`: the restart over the store, over SQLite
//! (`cell/store.rs`, nine hosts, about 600 agent requests a seed) over two seeds, so a
//! change to what the server keeps in its store across a restart, to how the cell mints
//! tokens and joins hosts with them, or a store that reaches the trace, moves a hash.
//! `tests/store.rs` holds the same seeds over Postgres to these lines.

#[path = "cell/elastic.rs"]
#[allow(
    dead_code,
    reason = "the elastic scenarios are shared with tests/elastic.rs, which runs the Lambda-like one this binary does not pin"
)]
mod elastic;
#[path = "cell/jobs.rs"]
mod jobs;
#[path = "cell/lossy_wire.rs"]
mod lossy_wire;
#[path = "cell/real_agents.rs"]
mod real_agents;
#[path = "cell/server_restart.rs"]
mod server_restart;
#[path = "cell/store.rs"]
#[allow(
    dead_code,
    reason = "the store scenarios are shared with tests/store.rs, which runs the ones this binary does not pin"
)]
mod store;
#[path = "cell/whole_host.rs"]
mod whole_host;
#[path = "cell/whole_host_cancel.rs"]
mod whole_host_cancel;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use js_jobs::{AllocationId, IdSource};
use js_lease_server::StartupError;
use js_sim::golden::GoldenFile;
use js_sim::{Chance, Faults, TraceHash};
use js_sim_cell::replay::REPORT_LINES;
use js_sim_cell::server::seeded;
use js_sim_cell::{
    Cell, Failure, FailureReport, Invariant, JOB_PREFIX, Observations, ServerSpec, Setup, Shape,
    Side, Store, Violation, Work, run,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;

/// Virtual time `n` milliseconds after the start.
fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A scratch directory of its own for one server, removed when the server is dropped.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell"),
        name,
    )
}

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

/// When the client makes its job, and how long the job takes.
const JOB_AT_MS: u64 = 20_000;
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

/// How many seeds of the whole-host scenario the golden file pins, and the family they are
/// kept under.
const WHOLE_SEEDS: u64 = 2;
const WHOLE_FAMILY: &str = "whole_host";

/// How many seeds of the cancelled whole-host job the golden file pins, and the family
/// they are kept under.
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

/// How many seeds of the job smoke scenario the golden file pins, and the family they are
/// kept under.
const JOBS_SEEDS: u64 = 1;
const JOBS_FAMILY: &str = "jobs";

/// How many seeds of the elastic scenario the golden file pins, and the family they are
/// kept under.
const ELASTIC_SEEDS: u64 = 1;
const ELASTIC_FAMILY: &str = "elastic";

/// The jobs of the cell whose ids are compared: four hosts, eight jobs of [`JOB_RUN`].
const JOB_HOSTS: usize = 4;
const JOBS: usize = 8;
const JOB_RUN: Duration = Duration::from_secs(10);
const JOB_HORIZON_MS: u64 = 60_000;

/// The cell restarted between two batches: [`BATCH`] jobs in each, the restart at
/// [`RESTART_AT_MS`], and its end.
const BATCH: usize = 4;
const RESTART_AT_MS: u64 = 30_000;
const RESTART_HORIZON_MS: u64 = 120_000;

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
    cell.at(ms(JOB_AT_MS), |cell| {
        cell.submit(&Work::run_for(Duration::from_millis(RUN_MS)));
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
/// run did what the scenario asks (the job launched once, its agent listed live at
/// the end), so the hash covers a run that reached the server. The same for a seed of the
/// whole-host scenario and one of the cancelled whole-host job, each run twice, and for
/// one seed of the job smoke scenario (`cell/jobs.rs`), run twice,
/// whose items end alike, and for one seed of the EC2-like elastic scenario
/// (`cell/elastic.rs`), run twice, whose items end alike, and for one seed of the restart
/// over a SQLite store (`cell/store.rs`), run twice. Catches a handler path that reads a
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
    assert!(lines.iter().any(|line| line.contains(" cell launch h")));
    assert_eq!(first.ledger().launches().len(), 1);
    let listed = &first.observations().server.hosts;
    assert_eq!(listed.len(), HOSTS);
    // The agent gives a call 10 s, longer than the server's grace, so on a wire that loses
    // 10% of its messages a host whose last calls were lost can read lost at the horizon.
    // Every such host lost a message in the last 30 s of the run.
    let lately = |line: &String, host: &str| {
        let at = line.split(' ').next().and_then(|t| t.parse::<u64>().ok());
        let late = at.is_some_and(|at| at >= HORIZON_MS - 30_000);
        line.contains("dropped") && line.contains(host) && late
    };
    for (host, state) in listed.iter().filter(|(_, host)| host.lost) {
        let name = host.to_string();
        let explained = lines.iter().any(|line| lately(line, &name));
        assert!(
            explained,
            "{host} is lost at the end and lost no message lately: {state:?}"
        );
    }
    let scenario = whole_host::fifty_whole_host_requests();
    let once = scenario.run_seeds(1).runs[0].hash;
    assert_eq!(once, scenario.run_seeds(1).runs[0].hash);
    let cancel = whole_host_cancel::a_cancelled_request();
    let once = cancel.run_seeds(1).runs[0].hash;
    assert_eq!(once, cancel.run_seeds(1).runs[0].hash);

    let restart = server_restart::a_restart_of_the_server_with_runs_in_flight_double_books_nothing;
    let once = restart().run_seeds(1).runs[0].hash;
    assert_eq!(restart().run_seeds(1).runs[0].hash, once);

    let once = jobs::jobs_on_a_lossy_wire().run_seeds(1).runs.remove(0);
    let again = jobs::jobs_on_a_lossy_wire().run_seeds(1).runs.remove(0);
    assert_eq!(once.hash, again.hash);
    assert_eq!(once.outcomes, again.outcomes);

    let once = elastic::an_ec2_like_pool().run_seeds(1).runs.remove(0);
    let again = elastic::an_ec2_like_pool().run_seeds(1).runs.remove(0);
    assert_eq!(once.hash, again.hash);
    assert_eq!(once.outcomes, again.outcomes);
    assert_eq!(once.outcomes.len(), elastic::RUNS as usize);

    let once = store::restart(Store::Sqlite).run_seeds(1).runs[0].hash;
    assert_eq!(
        store::restart(Store::Sqlite).run_seeds(1).runs[0].hash,
        once
    );
}

/// Asserts that one seed of the scenario with real agents (`cell/real_agents.rs`), run twice,
/// gives one hash and ends every item succeeded. Catches a source of nondeterminism in the
/// real agent lifecycle as the cell runs it.
#[test]
fn a_seed_with_real_agents_replays_to_the_same_trace_hash() {
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
}

/// The cell whose job and allocation ids are compared, for `setup`: [`JOB_HOSTS`] hosts
/// and [`JOBS`] jobs submitted at once through the job front.
fn job_cell(setup: Setup) -> Result<Cell, StartupError> {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), JOB_HOSTS);
    let mut cell = Cell::new(setup, spec, scratch("job-ids"), Faults::default())?;
    for _ in 0..JOB_HOSTS {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell.at(ms(0), |cell| {
        for _ in 0..JOBS {
            cell.submit(&Work::run_for(JOB_RUN));
        }
    });
    Ok(cell)
}

/// Every launch of the job cell's run of `seed`, by name: `<job>/<allocation>`, in the
/// order of the cell's ledger.
fn job_launches(seed: u64) -> Vec<String> {
    let cell = run("job-ids", seed, ms(JOB_HORIZON_MS), &job_cell).expect("the job cell runs");
    let launches = cell.ledger().launches().into_iter();
    launches
        .map(|launch| launch.run.as_str().to_owned())
        .collect()
}

/// Asserts that two runs of one seed launch every job under the same job and allocation
/// ids, in the same order, as the cell's ledger (which no trace line holds) names them; that
/// each launch names a job of the front's and an allocation of 32 hex digits; and that
/// another seed launches the same jobs under other allocation ids. Catches allocation ids
/// minted from the system's random source, and a source that does not follow the seed.
#[test]
fn job_and_allocation_ids_are_a_function_of_the_seed() {
    let once = job_launches(3);
    assert_eq!(once.len(), JOBS, "{once:?}");
    let named = |run: &String| {
        let (job, allocation) = run.split_once('/').expect("a job and an allocation");
        let hex = allocation
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        assert!(
            job.starts_with(JOB_PREFIX) && allocation.len() == 32 && hex,
            "{run}"
        );
        job.to_owned()
    };
    let jobs: BTreeSet<String> = once.iter().map(named).collect();
    assert_eq!(jobs.len(), JOBS);
    assert_eq!(job_launches(3), once);
    let other = job_launches(4);
    let other_jobs: BTreeSet<String> = other.iter().map(named).collect();
    assert_eq!(other_jobs, jobs);
    let shared = other.iter().filter(|run| once.contains(run)).count();
    assert_eq!(shared, 0, "another seed mints other allocation ids");
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

/// The cell whose server is restarted between two batches of jobs, for `setup`:
/// [`JOB_HOSTS`] hosts, [`BATCH`] jobs at time zero, a restart of the server at
/// [`RESTART_AT_MS`] and [`BATCH`] more jobs a second after it.
fn restarted_job_cell(setup: Setup) -> Result<Cell, StartupError> {
    let spec = ServerSpec::registered_pool("burst", Shape::gpu_host(), JOB_HOSTS);
    let mut cell = Cell::new(setup, spec, scratch("job-restart"), Faults::default())?;
    for _ in 0..JOB_HOSTS {
        cell.add_agent().expect("an agent name is a host id");
    }
    let batch = |cell: &mut Cell| {
        for _ in 0..BATCH {
            cell.submit(&Work::run_for(JOB_RUN));
        }
    };
    cell.at(ms(0), batch);
    cell.at(ms(RESTART_AT_MS), Cell::restart_server);
    cell.at(ms(RESTART_AT_MS + 1_000), batch);
    Ok(cell)
}

/// Asserts that a server restarted between two batches of jobs goes on minting the seed's
/// allocation ids where it stopped: the two batches' launches name exactly the first
/// `2 * BATCH` ids of the seed's script, none twice. Catches a restart that starts the id
/// source over (the second batch would reuse the first batch's ids), and one that gives
/// the restarted server another source.
#[test]
fn a_restarted_server_goes_on_minting_the_seeds_ids() {
    let seed = 5;
    let end = ms(RESTART_HORIZON_MS);
    let cell = run("job-restart", seed, end, &restarted_job_cell).expect("the job cell runs");
    let launches = cell.ledger().launches();
    assert_eq!(
        launches.len(),
        2 * BATCH,
        "every job of both batches launched once"
    );
    let allocation = |run: &str| {
        let (_, allocation) = run.split_once('/').expect("a job and an allocation");
        allocation.to_owned()
    };
    let launched: BTreeSet<String> = launches
        .iter()
        .map(|launch| allocation(launch.run.as_str()))
        .collect();
    let script = seeded(seed);
    let mint = |_| AllocationId::from_bits(script.mint().expect("a scripted id")).to_string();
    let minted: BTreeSet<String> = (0..2 * BATCH).map(mint).collect();
    assert_eq!(launched, minted);
}

/// The hash of each pinned seed of the job smoke scenario, which must pass.
fn jobs_runs() -> Vec<(String, TraceHash)> {
    let passed = jobs::jobs_on_a_lossy_wire().run_seeds(JOBS_SEEDS);
    let each = |run: &js_sim_cell::SeedRun| {
        let name = format!("jobs_on_a_lossy_wire/seed-{}", run.seed);
        (name, run.hash)
    };
    passed.runs.iter().map(each).collect()
}

/// Asserts that the hashes of the pinned seeds of the job smoke scenario equal the golden
/// file's (family `jobs`). Catches any change to a decision of the job front, the per-job
/// agents, the server's job service or job pass, or to the trace text, and a job run that
/// differs between processes (a job or allocation id, or a stored time, that does not
/// follow the seed).
#[test]
fn the_job_scenario_matches_the_golden_file() {
    golden_of(JOBS_FAMILY).assert_matches(&jobs_runs());
}

/// The hash of each pinned seed of the elastic scenario, which must pass.
fn elastic_runs() -> Vec<(String, TraceHash)> {
    let passed = elastic::an_ec2_like_pool().run_seeds(ELASTIC_SEEDS);
    let each = |run: &js_sim_cell::SeedRun| {
        let name = format!(
            "an_ec2_like_pool_scales_up_to_demand_and_back_to_zero/seed-{}",
            run.seed
        );
        (name, run.hash)
    };
    passed.runs.iter().map(each).collect()
}

/// Asserts that the hashes of the pinned seeds of the elastic scenario equal the golden
/// file's (family `elastic`). Catches any change to a decision of the scaler as the cell
/// drives it, to how the cell boots, joins and ends a launched machine, or to the trace
/// text, and an elastic run that differs between processes (a machine name drawn from
/// the operating system's random source rather than the seed).
#[test]
fn the_elastic_scenario_matches_the_golden_file() {
    golden_of(ELASTIC_FAMILY).assert_matches(&elastic_runs());
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

/// The hash of each pinned seed of the restart over a SQLite store, which must pass.
fn store_runs() -> Vec<(String, TraceHash)> {
    let passed = store::restart(Store::Sqlite).run_seeds(store::GOLDEN_SEEDS);
    store::golden_names(&passed.runs)
}

/// Asserts that the hashes of the pinned seeds of the restart over a SQLite store equal the
/// golden file's (family `store`). Catches any change to what the server keeps in its
/// store and reads back at a restart, to how the cell mints join tokens and joins hosts
/// with them, or to the trace text, and a run over the store that differs between
/// processes (a store file, a token or a digest that reaches the trace).
#[test]
fn the_store_scenario_matches_the_golden_file() {
    golden_of(store::FAMILY).assert_matches(&store_runs());
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
    golden_of(FAULTS_FAMILY).regenerate(&faults_runs()).unwrap();
    golden_of(JOBS_FAMILY).regenerate(&jobs_runs()).unwrap();
    golden_of(ELASTIC_FAMILY)
        .regenerate(&elastic_runs())
        .unwrap();
    golden_of(store::FAMILY).regenerate(&store_runs()).unwrap();
    golden_of(REAL_FAMILY).regenerate(&real_runs()).unwrap();
}

/// The hash of each pinned seed of the whole-host scenario, which must pass.
fn whole_runs() -> Vec<(String, TraceHash)> {
    let scenario = whole_host::fifty_whole_host_requests();
    let passed = scenario.run_seeds(WHOLE_SEEDS);
    let each = |run: &js_sim_cell::SeedRun| {
        let name = format!("fifty_whole_host_requests/seed-{}", run.seed);
        (name, run.hash)
    };
    passed.runs.iter().map(each).collect()
}

/// Asserts that the hashes of the pinned seeds of the whole-host scenario equal the golden
/// file's (family `whole_host`). Catches any change to a decision of the client's
/// whole-host path or of the server's whole-host path, or to the trace text, and a run
/// that differs between processes.
#[test]
fn the_whole_host_scenario_matches_the_golden_file() {
    golden_of(WHOLE_FAMILY).assert_matches(&whole_runs());
}

/// The hash of each pinned seed of the cancelled whole-host job, which must pass.
fn whole_faults_runs() -> Vec<(String, TraceHash)> {
    let scenario = whole_host_cancel::a_cancelled_request();
    let passed = scenario.run_seeds(WHOLE_FAULTS_SEEDS);
    let each = |run: &js_sim_cell::SeedRun| {
        let name = format!("a_cancelled_whole_host_request/seed-{}", run.seed);
        (name, run.hash)
    };
    passed.runs.iter().map(each).collect()
}

/// Asserts that the hashes of the pinned seeds of the cancelled whole-host job equal the
/// golden file's (family `whole_host_faults`). Catches any change to how the server drops
/// a cancelled whole-host job's place, to the client's cancel of a queued job, or to the
/// trace text, and a run that differs between processes.
#[test]
fn the_cancelled_whole_host_scenario_matches_the_golden_file() {
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
        stopped < ms(JOB_AT_MS),
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
