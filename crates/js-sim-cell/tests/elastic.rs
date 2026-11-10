//! The elastic family: a pool whose machines the server's scaler launches on an elastic
//! fake cloud, which boot into the cell's agents and are released once idle, under every
//! invariant of `Invariant::ALL`.
//!
//! The scenarios (in `cell/elastic.rs`, shared with the replay tests that pin their trace
//! hashes) are the two that need no scripted fault of the cloud: the demand of queued
//! whole-host jobs scales a pool up to its instance limit and back to no machine, billed as
//! EC2 bills and as Lambda bills. Each expects only what the server as built and the
//! design both promise. The other tests drive one cell
//! by hand: how a launched machine boots and joins, how the recording wrapper times its
//! calls, and what the cell does with a machine that never joins, that the cloud preempts
//! or drops, or that the scheduler did not launch.
//!
//! Sizes, in agent requests a seed: the scenarios about 19,200 heartbeats
//! (`cell/elastic.rs`), over [`SEEDS`] seeds each; a cell driven by hand, one or two machines for at most [`LOOK_AT`].

#[path = "cell/elastic.rs"]
mod elastic;

use std::path::Path;
use std::time::Duration;

use js_agent_wire::InstanceIdentity;
use js_elastic_fake::{Billing, Knobs};
use js_sim::Faults;
use js_sim_cell::elastic::offer;
use js_sim_cell::{
    AcquireAnswer, Cell, CloudEvent, ElasticPool, Killed, Scenario, ScenarioFailure, ServerSpec,
    Setup, Shape, Work,
};
use js_testkit::dir::TestDir;
use js_types::HostId;
use js_types::sched::FarmTime;
use js_types::{ManualClock, Timestamp};

/// How many seeds each scenario runs.
const SEEDS: u64 = 2;

/// The two scenarios are the family's largest by far (about a quarter hour each under the
/// coverage job's unoptimised, instrumented build): each runs its seeds on this many
/// threads, a quarter each, so that neither outlasts the rest of the job's tests.
const THREADS: u64 = 4;

/// How long a cell driven by hand runs: past the first launch (the second tick, at 10 s),
/// the machine's boot ([`Knobs::boot`], 120 s, later) and its hello.
const LOOK_AT: FarmTime = FarmTime::from_millis(200_000);

/// The server's tick: when the scaler acts.
const TICK_MS: u64 = 5_000;

/// The run of the one item of a cell driven by hand: longer than [`LOOK_AT`].
const LONG_RUN: Duration = Duration::from_secs(600);

/// Proves the elastic pool end to end at size, billed as EC2 bills (per second, at least
/// a minute): a hundred whole-host jobs against an empty pool of at most forty machines all
/// succeed, the cloud never runs more than forty of the scheduler's machines
/// (`instances-within-limits`), every machine is released once idle and none runs at the
/// end (`launched-machines-settle`), and the cloud's own bill equals the bill recomputed
/// from the launch and release times of the recording wrapper's calls. The pool's floor
/// names no architecture, so each machine becomes a node of the scheduler by the `arch` its
/// agent reports at hello, and every job, which asks for that `arch`, is placed on one.
/// Covers design section 7, "Scaler double launch" (each waiting job covered by one machine,
/// never two), and section 6.5's idle release; the bill check is a check of the cell's one
/// clock, not a claim about the scheduler's cost. Red for a scaler that launches past
/// `max_instances`, one that leaves an idle machine running, a server that builds each
/// host's node from the pool's floor (no node, so the jobs queue for ever and the seed
/// never settles), and a cell whose cloud clock drifts from the server's.
#[test]
fn an_ec2_like_pool_scales_up_to_demand_and_back_to_zero() {
    on_threads(elastic::an_ec2_like_pool);
}

/// Proves the same shape over a cloud billed as Lambda bills (each started minute whole,
/// at least one), with the same invariants and the same bill check: every machine is
/// released once idle, so none bills on past the minute its release fell in. Covers
/// section 6.5's idle release and section 7's "Scaler double launch". Red as the EC2-like
/// scenario is, and for a recomputation that bills a part of a minute.
#[test]
fn a_lambda_like_pool_bills_whole_minutes_and_releases_idle_machines() {
    on_threads(elastic::a_lambda_like_pool);
}

/// Runs seeds `0..SEEDS` of the scenario `make` builds, [`THREADS`] contiguous ranges at
/// once, each on a thread of its own with a scenario built there (a scenario is built
/// and run on one thread), and fails with the first failure of any range. The scenarios
/// have no witness, so judging each range alone judges what one run of every seed would;
/// a witness added to either would be judged per range, and these tests would then run
/// the seeds as one range.
fn on_threads(make: fn() -> Scenario) {
    let parts: Vec<_> = (0..THREADS)
        .map(|part| {
            let seeds = part * SEEDS / THREADS..(part + 1) * SEEDS / THREADS;
            std::thread::spawn(move || {
                let ran = make().try_run_seed_range(seeds);
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

/// A cell of seed `seed` with an elastic pool of at most two machines of `pool`'s kind,
/// keeping its trace lines, one whole-host item of [`LONG_RUN`] submitted at the start.
fn cell_of(name: &str, seed: u64, pool: ElasticPool) -> Cell {
    cell_running(name, seed, pool, LONG_RUN)
}

/// [`cell_of`], its first item running for `run`.
fn cell_running(name: &str, seed: u64, pool: ElasticPool, run: Duration) -> Cell {
    let spec = ServerSpec::elastic_pool(pool, elastic::shape());
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell");
    let dir = TestDir::under(root, &format!("{name}-{seed}"));
    let setup = Setup::new(seed).with_trace_lines();
    let mut cell = Cell::new(setup, spec, dir, Faults::default()).expect("the cell starts");
    cell.submit(&Work::whole_host(run));
    cell
}

/// The EC2-like pool of the cells driven by hand.
fn small_pool() -> ElasticPool {
    ElasticPool::ec2_like(elastic::POOL, elastic::offers(), 2)
}

/// The cell's trace lines that start with `<t> cell cloud`.
fn cloud_lines(cell: &mut Cell) -> Vec<String> {
    let lines = cell.trace_lines().expect("lines are kept");
    let cloud = lines.iter().filter(|line| line.contains(" cell cloud "));
    cloud.cloned().collect()
}

/// The join token a boot script writes, read as the server's own tests read it: the line
/// that writes the token file, unquoted.
fn token_in(script: &str) -> String {
    let head = "printf '%s' '";
    let line = script
        .lines()
        .find(|line| line.starts_with(head) && line.ends_with("/etc/jobscheduler/token"));
    let line = line.expect("the script writes the token");
    let rest = &line[head.len()..];
    rest.split("' > ")
        .next()
        .expect("a quoted token")
        .to_owned()
}

/// The host id a boot script starts the agent with.
fn host_id_in(script: &str) -> String {
    let head = "--host-id \"";
    let at = script.find(head).expect("the unit names the host id") + head.len();
    script[at..]
        .split('"')
        .next()
        .expect("a quoted host id")
        .to_owned()
}

/// Asserts that the machine the scaler launched boots into an agent whose bearer is the
/// one-time join token of its launch payload (`ElasticFake::user_data`, read as the
/// server's tests read it), whose host id is the name the payload starts the agent with,
/// and whose hello names the instance `<pool>:<name>` the token is bound to; that the
/// server then lists the host (the hello was welcomed); and that when the cloud drops the
/// machine its agent is killed in that same step, with no cloud trace line holding the
/// token or the name. Catches a boot that reads the token from anywhere but the payload,
/// an agent that says hello without the instance (refused: the token is bound to it),
/// and a dropped machine whose agent goes on beating.
#[test]
fn a_booted_machine_joins_with_the_token_in_its_launch_payload() {
    let mut cell = cell_of("a_booted_machine_joins", 1, small_pool());
    cell.run_until(LOOK_AT).expect("no invariant fails");
    let cloud = cell.cloud().expect("an elastic pool").clone();
    assert_eq!(cloud.fake().launched(), 1);
    let script = cloud.fake().user_data(0);
    let (token, name) = (token_in(&script), host_id_in(&script));
    let machine = cell.machine(0).expect("the machine booted");
    let spec = machine.spec();
    assert!(
        spec.join_token == token,
        "the bearer is the payload's token"
    );
    assert_eq!(spec.host.as_str(), name);
    let instance = InstanceIdentity {
        provider: elastic::POOL.to_owned(),
        instance_id: name.clone(),
    };
    assert_eq!(spec.instance, Some(instance));
    let host = HostId::new(&name).expect("a host id");
    let listed = cell.observations().server.hosts.get(&host);
    assert!(listed.is_some_and(|listed| !listed.lost), "the host joined");
    let facts = cell.observations().cloud().cloned().expect("cloud facts");
    let joined = facts.machines.values().map(|machine| machine.joined);
    assert_eq!(joined.collect::<Vec<_>>(), [true]);

    let at = cell.now();
    cell.drop_machine(0);
    assert_eq!(cell.now(), at);
    let machine = cell.machine(0).expect("the node stays");
    assert!(machine.truth().is_none(), "the agent is killed");
    assert!(!cell.observations().agents.contains_key(&host));
    let killed = Killed { host, at };
    assert_eq!(cell.observations().kills.last(), Some(&killed));
    let lines = cloud_lines(&mut cell);
    let drop = format!("{} cell cloud drop m000", at.as_millis());
    assert_eq!(lines.last(), Some(&drop));
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with(" cell cloud launch m000"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with(" cell cloud boot m000"))
    );
    let leaked = lines
        .iter()
        .any(|line| line.contains(&token) || line.contains(&name));
    assert!(!leaked, "no cloud line holds the token or the name");
}

/// Asserts that the recording wrapper records the one acquire of a cell with the virtual
/// time of the step it was made in (a tick of the server, and the step at which the cell
/// first saw the cloud run the machine), its offer, its zone, an acquire id the seed
/// names alike on every run and differently from another seed, and the machine it was
/// answered with; and that every list is recorded at a tick. Catches a wrapper that stamps
/// a call with the cloud's own clock off the cell's (the cloud clock not advanced before
/// the server's input), one that drops an argument or the answer, and acquire ids drawn
/// from the operating system.
#[test]
fn the_recording_wrapper_records_each_acquire_with_the_virtual_time_of_its_step() {
    let acquire_of = |seed: u64| {
        let mut cell = cell_of("the_recording_wrapper", seed, small_pool());
        cell.run_until(FarmTime::from_millis(20_000))
            .expect("no invariant fails");
        let cloud = cell.cloud().expect("an elastic pool").clone();
        let calls = cloud.calls();
        let acquires: Vec<_> = calls
            .iter()
            .filter(|call| matches!(call.event, CloudEvent::Acquire { .. }))
            .cloned()
            .collect();
        assert_eq!(acquires.len(), 1, "one waiting item, one acquire");
        let lists = calls
            .iter()
            .filter(|call| matches!(call.event, CloudEvent::List { .. }));
        assert!(lists.clone().count() >= 2, "a list at every tick");
        assert!(lists.clone().all(|call| call.at.as_millis() % TICK_MS == 0));
        let call = acquires[0].clone();
        assert_eq!(call.at.as_millis() % TICK_MS, 0, "made in a tick");
        let launch = format!("{} cell cloud launch m000", call.at.as_millis());
        assert!(
            cloud_lines(&mut cell).contains(&launch),
            "seen in the same step"
        );
        let CloudEvent::Acquire {
            offer,
            zone,
            acquire,
            answer,
        } = call.event
        else {
            unreachable!("an acquire");
        };
        assert_eq!(offer, "sim_1x_a100");
        assert_eq!(zone.as_deref(), Some("zone-a"));
        assert!(acquire.starts_with("burst-"), "{acquire}");
        let running = cloud.fake().running();
        let machine = running[0].as_str().to_owned();
        assert_eq!(answer, AcquireAnswer::Accepted { machine });
        acquire
    };
    let first = acquire_of(3);
    assert_eq!(acquire_of(3), first, "a seed names its machines alike");
    assert_ne!(acquire_of(4), first, "another seed names them otherwise");
}

/// Asserts that a pool whose machines do not join (`ElasticPool::joins(false)`) boots no
/// agent for its launched machine, writing `cloud boot m000 silent`, while the cloud runs
/// the machine. Catches a cell that boots an agent for every launch whatever the pool
/// says.
#[test]
fn a_machine_of_a_pool_that_does_not_join_boots_no_agent() {
    let pool = small_pool().joins(false);
    assert!(!pool.joined());
    let mut cell = cell_of("a_machine_that_does_not_join", 1, pool);
    cell.run_until(LOOK_AT).expect("no invariant fails");
    assert!(cell.machine(0).is_none());
    let lines = cloud_lines(&mut cell);
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with(" cell cloud boot m000 silent"))
    );
    let cloud = cell.cloud().expect("an elastic pool");
    assert_eq!(cloud.fake().running().len(), 1);
}

/// Asserts that a machine the cloud drops before its boot time never boots, the drop is
/// written (`cloud drop m000`) and no kill is noted (it had no agent); and that a
/// preempted machine, once booted, has its agent killed in the step of the preemption
/// (`cloud preempt m000`). Catches a cell that boots a machine the cloud no longer runs,
/// one that notes a kill of an agent that never ran, and one that misnames how the cloud
/// stopped a machine.
#[test]
fn machines_the_cloud_stops_have_no_agent_from_that_step() {
    let mut early = cell_of("a_machine_dropped_before_boot", 1, small_pool());
    early
        .run_until(FarmTime::from_millis(20_000))
        .expect("no invariant fails");
    early.drop_machine(0);
    let at = early.now();
    early.run_until(LOOK_AT).expect("no invariant fails");
    assert!(early.machine(0).is_none());
    assert!(early.observations().kills.is_empty());
    let lines = cloud_lines(&mut early);
    let drop = format!("{} cell cloud drop m000", at.as_millis());
    assert!(lines.contains(&drop), "{lines:?}");
    assert!(!lines.iter().any(|line| line.contains("boot m000")));

    let mut late = cell_of("a_preempted_machine", 1, small_pool());
    late.run_until(LOOK_AT).expect("no invariant fails");
    let host = late.machine(0).expect("booted").host().clone();
    let at = late.now();
    late.preempt_machine(0);
    assert!(late.machine(0).expect("the node stays").truth().is_none());
    assert_eq!(late.observations().kills.last(), Some(&Killed { host, at }));
    let lines = cloud_lines(&mut late);
    assert_eq!(
        lines.last(),
        Some(&format!("{} cell cloud preempt m000", at.as_millis()))
    );
}

/// Asserts that a machine the scheduler did not launch (a foreign machine the cloud
/// holds) boots no agent and is neither numbered nor counted as the scheduler's, and that
/// asking the cell to drop a machine it does not have changes nothing. Catches a cell
/// that boots a foreign machine and one that counts it against the pool's limit.
#[test]
fn a_foreign_machine_is_not_the_schedulers() {
    let mut cell = cell_of("a_foreign_machine", 1, small_pool());
    let cloud = cell.cloud().expect("an elastic pool").clone();
    let foreign = offer(&elastic::offers()[0], &elastic::shape());
    cloud.fake().add_foreign(Some("someone-else"), foreign);
    cell.drop_machine(7);
    cell.run_until(LOOK_AT).expect("no invariant fails");
    assert_eq!(
        cloud.fake().launched(),
        2,
        "the foreign machine and the scheduler's"
    );
    assert!(cell.machine(1).is_none());
    let facts = cell.observations().cloud().cloned().expect("cloud facts");
    assert_eq!(facts.machines.len(), 1);
    assert_eq!(facts.running.len(), 1);
    assert_eq!(cloud.fake().running().len(), 2);
}

/// Asserts that an operator's reset of the brake goes to the server's scaler of an
/// elastic pool and is recorded with the cloud's calls, and that a cell without an
/// elastic pool resets nothing. Catches a reset that is not recorded, which would leave
/// `no-acquire-while-plan-only` holding a pool tripped after the operator released it.
#[test]
fn an_operators_brake_reset_is_recorded_with_the_clouds_calls() {
    let mut cell = cell_of("a_brake_reset", 1, small_pool());
    cell.run_until(FarmTime::from_millis(20_000))
        .expect("no invariant fails");
    assert!(cell.reset_brake());
    let calls = cell.cloud().expect("an elastic pool").calls();
    assert_eq!(
        calls.last().map(|call| &call.event),
        Some(&CloudEvent::BrakeReset)
    );
    let facts = cell.observations().cloud().cloned().expect("cloud facts");
    assert_eq!(
        facts.calls.last().map(|call| &call.event),
        Some(&CloudEvent::BrakeReset)
    );

    let spec = ServerSpec::whole_node_pool("burst", Shape::gpu_host(), 1);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell");
    let dir = TestDir::under(root, "a_brake_reset_without_a_pool");
    let mut plain = Cell::new(Setup::new(1), spec, dir, Faults::default()).expect("starts");
    assert!(!plain.reset_brake());
    assert!(plain.cloud().is_none());
    assert!(plain.observations().cloud().is_none());
}

/// Asserts what an elastic pool's spec holds (its billing, offers, limit, idle time,
/// knobs and whether it joins) and that the server's command line cannot configure its
/// server, which the server node configures over its cloud. Catches a spec that drops a
/// key or bills the wrong way, and an elastic spec read as a pool of hosts that register
/// themselves.
#[test]
fn an_elastic_pool_spec_holds_its_keys_and_is_configured_by_its_node() {
    let knobs = Knobs {
        boot: Duration::from_secs(30),
        ..Knobs::default()
    };
    let pool = ElasticPool::lambda_like("pool", elastic::offers(), 3)
        .idle(Duration::from_secs(90))
        .knobs(knobs.clone());
    assert_eq!(pool.name(), "pool");
    assert_eq!(pool.billing(), Billing::Lambda);
    assert_eq!(pool.offers(), elastic::offers().as_slice());
    assert_eq!(pool.max_instances(), 3);
    assert_eq!(pool.idle_time(), Duration::from_secs(90));
    assert_eq!(pool.cloud_knobs(), &knobs);
    assert!(pool.joined());
    let ec2 = ElasticPool::ec2_like("pool", Vec::new(), 1);
    assert_eq!(ec2.billing(), Billing::Aws);
    assert_eq!(ec2.idle_time(), js_sim_cell::elastic::DEFAULT_IDLE);

    let spec = ServerSpec::elastic_pool(pool.clone(), elastic::shape());
    assert_eq!(spec.elastic(), Some(&pool));
    assert_eq!(spec.pool(), "pool");
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell");
    let dir = TestDir::under(root, "an_elastic_spec");
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let refused = spec
        .config(dir.path(), &clock)
        .err()
        .expect("not from the command line");
    assert_eq!(refused.flag(), "elastic");
    assert!(
        ServerSpec::whole_node_pool("pool", Shape::gpu_host(), 1)
            .elastic()
            .is_none()
    );
}

/// Asserts that a scenario's own statement (`Scenario::expect_that`) that finds something
/// wrong fails the seed as unmet, naming the statement and what it found, and that one
/// that finds nothing passes. Catches a statement whose answer is dropped, which would let
/// the elastic scenarios' bill check pass whatever it found.
#[test]
fn a_statement_of_the_scenarios_own_fails_the_seed_with_what_it_found() {
    let scenario = |name: &str| {
        Scenario::new(name)
            .scratch(env!("CARGO_TARGET_TMPDIR"))
            .server(ServerSpec::registered_pool("burst", Shape::gpu_host(), 1))
            .hosts("burst", 1)
            .settle_within(Duration::from_secs(1))
    };
    let found = scenario("a_statement_that_finds")
        .expect_that("nothing is wrong", |_| None)
        .expect_that("the bill", |cell| {
            Some(format!("off at {}", cell.now().as_millis()))
        })
        .try_run_seeds(1);
    match found {
        Err(ScenarioFailure::Unmet { seed, detail, .. }) => {
            assert_eq!(seed, 0);
            assert_eq!(detail, "the bill: off at 1000");
        }
        other => panic!("not unmet: {other:?}"),
    }
    let passed = scenario("a_statement_that_finds_nothing")
        .expect_that("nothing is wrong", |_| None)
        .try_run_seeds(1);
    assert!(passed.is_ok(), "{passed:?}");
}

/// Asserts that a restart of the server of an elastic pool keeps the pool's cloud: the
/// calls recorded before it stay first in the record, the machine launched before it
/// joins the restarted server, and when two whole hosts are asked for after it (the
/// restart forgets the run in flight, so the first machine is free for one of them) the
/// other is met by a second machine, launched under an acquire id of its own (the
/// scaler's entropy goes on from where it was, rather than starting the seed's again).
/// Catches a restart that starts a second cloud (the first machine and its record lost
/// to the invariants), and one that draws the scaler's entropy afresh, which could name
/// a new machine as an earlier one.
#[test]
fn a_restarted_server_keeps_its_cloud_and_launches_under_new_acquire_ids() {
    let mut cell = cell_of("a_restarted_elastic_server", 1, small_pool());
    cell.run_until(LOOK_AT).expect("no invariant fails");
    let cloud = cell.cloud().expect("an elastic pool").clone();
    let before = cloud.calls();
    assert_eq!(cloud.fake().launched(), 1);
    cell.restart_server();
    assert!(cell.take_refused().is_none(), "the server starts again");
    let after = cell.cloud().expect("the same pool");
    assert!(after.calls().starts_with(&before), "the record is kept");
    cell.submit(&Work::whole_host(LONG_RUN));
    cell.submit(&Work::whole_host(LONG_RUN));
    // The horizon leaves the restarted scaler its wait for the hosts it restored (a
    // machine of before the restart covers demand while it is awaited, up to the pool's
    // boot timeout of 15 minutes), the boot of the second machine, and its hello.
    cell.run_until(FarmTime::from_millis(2_400_000))
        .expect("no invariant fails");
    let calls = cell.cloud().expect("the same pool").calls();
    let accepted: Vec<String> = calls
        .iter()
        .filter_map(|call| match &call.event {
            CloudEvent::Acquire {
                acquire,
                answer: AcquireAnswer::Accepted { .. },
                ..
            } => Some(acquire.clone()),
            _ => None,
        })
        .collect();
    let distinct: std::collections::BTreeSet<&String> = accepted.iter().collect();
    let tail = |cell: &mut Cell| {
        let lines = cell.trace_lines().expect("lines are kept");
        let cloud = lines.iter().filter(|line| line.contains(" cell c"));
        let kept: Vec<String> = cloud.cloned().collect();
        kept[kept.len().saturating_sub(40)..].join("\n")
    };
    if distinct.len() != 2 {
        panic!(
            "acquires {accepted:?}; last cloud and client lines:\n{}",
            tail(&mut cell)
        );
    }
    assert_eq!(cloud.fake().launched(), 2);
    let facts = cell.observations().cloud().cloned().expect("cloud facts");
    let joined = facts.machines.values().filter(|machine| machine.joined);
    assert_eq!(joined.count(), 2, "both machines joined");
    assert!(cell.machine(1).is_some(), "the second machine booted");
}

/// How many acquires the cloud's recording wrapper holds for `cell`.
fn acquires(cell: &Cell) -> usize {
    let calls = cell.cloud().expect("an elastic pool").calls();
    let acquire = |call: &&js_sim_cell::CloudCall| matches!(call.event, CloudEvent::Acquire { .. });
    calls.iter().filter(acquire).count()
}

/// Whether the cell's server runs.
fn server_up(cell: &Cell) -> bool {
    cell.server().is_some_and(js_sim_cell::ServerNode::is_up)
}

/// Asserts that an acquire armed to crash the server stops it in the tick the acquire is
/// made in, and that it stays down until it is restarted. The cell is stepped one server
/// tick at a time. At every tick before the acquire the server is up. At the tick of the
/// acquire the call has reached the cloud (one machine launched), the wrapper has recorded
/// the crash after it, and the server is down. Six ticks later it is still down and has
/// made no other acquire. `restart_server` brings it up again over its store, and the
/// restarted scaler finds the machine and makes no second acquire. Catches a crash that is
/// recorded but not acted on (the server still up and ticking), one that stops the server
/// before the call reaches the cloud, a server that comes back up by itself, and a restart
/// that cannot reopen its store.
#[test]
fn an_acquire_armed_to_crash_stops_the_server_until_it_is_restarted() {
    let mut cell = cell_of("an_acquire_armed_to_crash", 1, small_pool());
    cell.cloud()
        .expect("an elastic pool")
        .crash_at_next_acquire();
    let mut at = 0;
    while acquires(&cell) == 0 {
        assert!(server_up(&cell), "up before the acquire, at {at} ms");
        assert!(at < LOOK_AT.as_millis(), "no acquire by {at} ms");
        at += TICK_MS;
        cell.run_until(FarmTime::from_millis(at))
            .expect("no invariant fails");
    }
    assert!(
        !server_up(&cell),
        "down in the tick of the acquire, at {at} ms"
    );
    let cloud = cell.cloud().expect("an elastic pool").clone();
    assert_eq!(cloud.fake().launched(), 1, "the call reached the cloud");
    let events: Vec<CloudEvent> = cloud.calls().into_iter().map(|call| call.event).collect();
    let crash = events.iter().position(|event| *event == CloudEvent::Crash);
    let acquire = events
        .iter()
        .position(|event| matches!(event, CloudEvent::Acquire { .. }));
    assert_eq!(crash, acquire.map(|acquire| acquire + 1), "{events:?}");
    let later = at + 6 * TICK_MS;
    cell.run_until(FarmTime::from_millis(later))
        .expect("no invariant fails");
    assert!(!server_up(&cell), "still down at {later} ms");
    assert_eq!(acquires(&cell), 1, "a down server makes no call");
    cell.restart_server();
    assert!(cell.take_refused().is_none(), "the server starts again");
    assert!(server_up(&cell));
    cell.run_until(FarmTime::from_millis(later + LOOK_AT.as_millis()))
        .expect("no invariant fails");
    assert!(server_up(&cell));
    assert_eq!(acquires(&cell), 1, "the restarted scaler finds its machine");
    assert_eq!(cloud.fake().launched(), 1);
}
