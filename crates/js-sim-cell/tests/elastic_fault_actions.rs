//! The machinery the elastic fault family added, each part shown to change what it says
//! and nothing else: the actions that script the elastic fake's calls, set its knobs,
//! preempt, drop and add machines, switch whether machines join and reset the brake through
//! the operator's route; the two expectations about the cloud; and the cloud's counts
//! reaching a scenario's witness.
//!
//! Sizes: one cell driven by hand, one machine for at most [`LOOK_AT`] of virtual time.

#[path = "cell/elastic.rs"]
#[allow(
    dead_code,
    reason = "the elastic family's scenarios are not run here: these tests take only its pool's name, offers and shape"
)]
mod elastic;

use std::path::Path;
use std::time::Duration;

use js_elastic::RefusalCode;
use js_elastic_fake::{Call, Knobs, Step, THROTTLE_WAIT};
use js_sim::Faults;
use js_sim_cell::actions::{
    add_foreign_machine, drop_machine, machines_never_join, preempt_machine, reset_brake,
    script_provider, script_provider_forever, set_knobs,
};
use js_sim_cell::{
    AcquireAnswer, Action, Cell, CloudEvent, Counter, ElasticPool, Expect, ServerSpec, Setup,
    Shape, Work,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;

/// How long a cell runs to have its one machine booted and joined: past the first launch,
/// the machine's boot (120 s) and its hello.
const LOOK_AT: FarmTime = FarmTime::from_millis(200_000);

/// Past the first acquire, made in one of the first ticks.
const FIRST_ACQUIRE: FarmTime = FarmTime::from_millis(20_000);

/// A cell of seed 1 with an elastic pool of at most two machines, keeping its trace lines,
/// one whole-host item of 600 s submitted at the start.
fn cell_of(name: &str) -> Cell {
    let pool = ElasticPool::ec2_like(elastic::POOL, elastic::offers(), 2);
    let spec = ServerSpec::elastic_pool(pool, elastic::shape());
    let mut cell = Cell::new(
        Setup::new(1).with_trace_lines(),
        spec,
        scratch(name),
        Faults::default(),
    )
    .expect("the cell starts");
    cell.submit(&Work::whole_host(Duration::from_secs(600)));
    cell
}

/// A cell of a pool of hosts that register themselves: no elastic pool.
fn plain_cell(name: &str) -> Cell {
    let spec = ServerSpec::whole_node_pool("burst", Shape::gpu_host(), 1);
    let setup = Setup::new(1).with_trace_lines();
    Cell::new(setup, spec, scratch(name), Faults::default()).expect("the cell starts")
}

/// A scratch directory of its own.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell");
    TestDir::under(root, name)
}

/// The cell's trace lines that start `<t> cell cloud `, without the time and `cell`.
fn lines(cell: &mut Cell) -> Vec<String> {
    lines_of(cell, &["cloud "])
}

/// The cell's trace lines whose text after `<t> cell ` starts with one of `heads`, without
/// the time and `cell`.
fn lines_of(cell: &mut Cell, heads: &[&str]) -> Vec<String> {
    let lines = cell.trace_lines().expect("lines are kept");
    let cell_lines = lines.iter().filter_map(|line| line.split_once(" cell "));
    let kept = cell_lines.filter(|(_, rest)| heads.iter().any(|head| rest.starts_with(head)));
    kept.map(|(_, rest)| rest.to_owned()).collect()
}

/// The answers of the acquires the cloud's recording wrapper holds, in order.
fn acquires(cell: &Cell) -> Vec<AcquireAnswer> {
    let calls = cell.cloud().expect("an elastic pool").calls();
    let answers = calls.into_iter().filter_map(|call| match call.event {
        CloudEvent::Acquire { answer, .. } => Some(answer),
        _ => None,
    });
    answers.collect()
}

/// Asserts that `script_provider` queues its steps for the call it names: the first
/// acquire is refused for capacity, as scripted, the line names the call and the step, and
/// a cell without an elastic pool writes the line and changes nothing. Catches steps queued
/// for another call, and a line that names a step it did not queue.
#[test]
fn script_provider_queues_steps_for_the_named_call() {
    let mut cell = cell_of("script_provider_queues_steps");
    let capacity = Step::Refuse(RefusalCode::Capacity);
    script_provider(Call::Acquire, &[capacity]).run(&mut cell);
    cell.run_until(FIRST_ACQUIRE).expect("no invariant fails");
    assert_eq!(
        acquires(&cell).first(),
        Some(&AcquireAnswer::Refused(RefusalCode::Capacity))
    );
    assert_eq!(
        lines(&mut cell).first().map(String::as_str),
        Some("cloud script acquire refuse-capacity")
    );
    let mut plain = plain_cell("script_provider_without_a_pool");
    script_provider(Call::List, &[Step::Throttled, Step::Pass]).run(&mut plain);
    assert_eq!(lines(&mut plain), ["cloud script list throttled,pass"]);
}

/// Asserts that `script_provider_forever` makes every acquire take its step until it is set
/// again: every acquire answered 429 while the storm lasts, each the throttle wait after
/// the last, then the next acquire launches a machine. Catches a step taken once rather
/// than for good, and a storm that does not end.
#[test]
fn script_provider_forever_repeats_its_step_until_set_again() {
    let mut cell = cell_of("script_provider_forever_repeats");
    script_provider_forever(Call::Acquire, Step::Throttled).run(&mut cell);
    cell.run_until(FarmTime::from_millis(100_000))
        .expect("no invariant fails");
    let storm = acquires(&cell);
    assert!(storm.len() >= 2, "{storm:?}");
    assert!(
        storm
            .iter()
            .all(|answer| *answer == AcquireAnswer::Deferred(THROTTLE_WAIT)),
        "{storm:?}"
    );
    script_provider_forever(Call::Acquire, Step::Pass).run(&mut cell);
    cell.run_until(FarmTime::from_millis(150_000))
        .expect("no invariant fails");
    let after = acquires(&cell);
    assert!(
        matches!(after.last(), Some(AcquireAnswer::Accepted { .. })),
        "{after:?}"
    );
    let lines = lines(&mut cell);
    assert_eq!(lines[0], "cloud script acquire forever throttled");
    assert!(lines.contains(&"cloud script acquire forever pass".to_owned()));
    assert_eq!(
        cell.total_counters().get(Counter::AcquireThrottled),
        storm.len() as u64
    );
}

/// Asserts that `set_knobs` replaces the cloud's knobs as a whole and writes each of them.
/// Catches knobs merged rather than replaced, and a line that leaves one out.
#[test]
fn set_knobs_replaces_the_clouds_knobs() {
    let mut cell = cell_of("set_knobs_replaces");
    let knobs = Knobs {
        quota: Some(3),
        capacity: Some(4),
        list_lag: Duration::from_secs(60),
        ..Knobs::default()
    };
    set_knobs(knobs.clone()).run(&mut cell);
    assert_eq!(cell.cloud().expect("an elastic pool").fake().knobs(), knobs);
    assert_eq!(
        lines(&mut cell),
        [
            "cloud knobs quota=3 capacity=4 boot=120000ms list-lag=60000ms remove-lag=20000ms \
          unknown-hold=1800000ms"
        ]
    );
}

/// Asserts that `preempt_machine` stops the machine it numbers in the step it runs in: the
/// cloud no longer runs it, its agent is killed, the cloud counts one preemption, and a
/// second preemption of the stopped machine counts nothing. Catches a preemption counted
/// for a machine the cloud was not running, and one that leaves the agent beating.
#[test]
fn preempt_machine_stops_the_machine_and_counts_it_once() {
    let mut cell = cell_of("preempt_machine_stops");
    cell.run_until(LOOK_AT).expect("no invariant fails");
    let at = cell.now();
    preempt_machine(0).run(&mut cell);
    preempt_machine(0).run(&mut cell);
    let cloud = cell.cloud().expect("an elastic pool");
    assert!(cloud.fake().running().is_empty());
    assert_eq!(cloud.counters().get(Counter::MachinePreempted), 1);
    assert_eq!(cell.total_counters().get(Counter::MachinePreempted), 1);
    assert!(cell.machine(0).expect("the node stays").truth().is_none());
    let lines = cell.trace_lines().expect("lines are kept");
    let preempt = format!("{} cell cloud preempt m000", at.as_millis());
    assert_eq!(lines.iter().filter(|line| **line == preempt).count(), 1);
}

/// Asserts that `drop_machine` drops the machine it numbers in the step it runs in, as the
/// cloud's own delete: it no longer runs, counts no preemption, and the line says drop.
/// Catches a drop written as a preemption or a release.
#[test]
fn drop_machine_drops_the_machine_it_numbers() {
    let mut cell = cell_of("drop_machine_drops");
    cell.run_until(LOOK_AT).expect("no invariant fails");
    drop_machine(0).run(&mut cell);
    let cloud = cell.cloud().expect("an elastic pool");
    assert!(cloud.fake().running().is_empty());
    assert_eq!(cloud.counters().get(Counter::MachinePreempted), 0);
    assert_eq!(
        lines(&mut cell).last().map(String::as_str),
        Some("cloud drop m000")
    );
}

/// Asserts that `add_foreign_machine` adds a running machine the scheduler did not launch,
/// named and unnamed, which `Expect::machines_launched` does not count as the
/// scheduler's, and that a cell without an elastic pool adds none. Catches a foreign
/// machine counted as launched, and a line that does not say whether it was added.
#[test]
fn add_foreign_machine_adds_a_machine_the_scheduler_did_not_launch() {
    let mut cell = cell_of("add_foreign_machine_adds");
    add_foreign_machine(Some("another-scheduler")).run(&mut cell);
    add_foreign_machine(None).run(&mut cell);
    let cloud = cell.cloud().expect("an elastic pool").clone();
    assert_eq!(cloud.fake().launched(), 2);
    assert_eq!(cloud.fake().running().len(), 2);
    assert_eq!(
        Expect::machines_launched(elastic::POOL, 0).unmet(&mut cell),
        None
    );
    assert_eq!(
        lines(&mut cell),
        ["cloud foreign named true", "cloud foreign unnamed true"]
    );
    let mut plain = plain_cell("add_foreign_machine_without_a_pool");
    assert!(!plain.add_foreign_machine(Some("another-scheduler")));
    assert_eq!(lines(&mut plain), ["cloud foreign named false"]);
}

/// Asserts that `machines_never_join` switches whether the machines of its pool that boot
/// from then on say hello: a machine booted after it is silent and has no agent, a pool the
/// cell does not have is not switched, and switching back makes the cloud's machines join.
/// Catches a switch read from the pool's spec rather than the cloud, and one applied to a
/// pool of another name.
#[test]
fn machines_never_join_switches_whether_booted_machines_say_hello() {
    let mut cell = cell_of("machines_never_join_switches");
    machines_never_join(elastic::POOL, true).run(&mut cell);
    cell.run_until(LOOK_AT).expect("no invariant fails");
    assert!(cell.machine(0).is_none());
    assert!(!cell.cloud().expect("an elastic pool").joins());
    assert!(!cell.machines_never_join("another-pool", false));
    assert!(!cell.cloud().expect("an elastic pool").joins());
    machines_never_join(elastic::POOL, false).run(&mut cell);
    assert!(cell.cloud().expect("an elastic pool").joins());
    let lines = lines(&mut cell);
    assert_eq!(lines[0], "cloud never-join burst true true");
    assert!(lines.contains(&"cloud boot m000 silent".to_owned()));
    assert_eq!(
        &lines[lines.len() - 2..],
        [
            "cloud never-join another-pool false false",
            "cloud never-join burst false true"
        ]
    );
}

/// Asserts that `reset_brake` goes through the server's operator route: the client call is
/// answered 202 and an accepted reset is recorded with the cloud's calls, while a pool the
/// server does not have is answered 404 and records nothing. Catches a reset that bypasses
/// the route, and one recorded though the server refused it (which would clear
/// `no-acquire-while-plan-only` for a pool still tripped).
#[test]
fn reset_brake_goes_through_the_operator_route() {
    let mut cell = cell_of("reset_brake_goes_through_the_route");
    cell.run_until(FIRST_ACQUIRE).expect("no invariant fails");
    reset_brake(elastic::POOL).run(&mut cell);
    let resets = |cell: &Cell| {
        let calls = cell.cloud().expect("an elastic pool").calls();
        let reset = calls
            .iter()
            .filter(|call| call.event == CloudEvent::BrakeReset);
        reset.count()
    };
    assert_eq!(resets(&cell), 1);
    assert_eq!(cell.reset_brake_by_route("another-pool"), 404);
    assert_eq!(resets(&cell), 1);
    let lines = lines_of(&mut cell, &["cloud brake", "client POST"]);
    assert_eq!(
        &lines[lines.len() - 4..],
        [
            "client POST 202",
            "cloud brake reset route 202",
            "client POST 404",
            "cloud brake reset route 404"
        ]
    );
}

/// Asserts what the two cloud expectations read: the brake from the server's scaler (not
/// tripped here), the scheduler's machines from the fake (one launched), and `no-pool` for
/// a pool the cell does not have, each unmet one written as wanted and got. Catches a
/// statement that reads another pool's brake or counts a machine twice.
#[test]
fn the_cloud_expectations_read_the_brake_and_the_launched_machines() {
    let mut cell = cell_of("the_cloud_expectations_read");
    cell.run_until(FIRST_ACQUIRE).expect("no invariant fails");
    let unmet = |cell: &mut Cell, expect: Expect| expect.unmet(cell);
    assert_eq!(
        unmet(&mut cell, Expect::pool_tripped(elastic::POOL, false)),
        None
    );
    assert_eq!(
        unmet(&mut cell, Expect::pool_tripped(elastic::POOL, true)).as_deref(),
        Some("pool-tripped(burst): wanted true, got false")
    );
    assert_eq!(
        unmet(&mut cell, Expect::pool_tripped("another-pool", false)).as_deref(),
        Some("pool-tripped(another-pool): wanted false, got no-pool")
    );
    assert_eq!(
        unmet(&mut cell, Expect::machines_launched(elastic::POOL, 1)),
        None
    );
    assert_eq!(
        unmet(&mut cell, Expect::machines_launched(elastic::POOL, 2)).as_deref(),
        Some("machines-launched(burst): wanted 2, got 1")
    );
    let mut plain = plain_cell("the_cloud_expectations_without_a_pool");
    assert_eq!(
        unmet(&mut plain, Expect::machines_launched("burst", 0)).as_deref(),
        Some("machines-launched(burst): wanted 0, got no-pool")
    );
}

/// Asserts that what the cloud counts reaches a scenario's witness: an acquire whose answer
/// the cloud lost counts `acquire_lost` in the cell's total, and a refused one
/// `acquire_refused`. Catches cloud counts kept apart from the agents' (a witness that
/// could never hold).
#[test]
fn the_clouds_counts_reach_the_cells_total() {
    let mut cell = cell_of("the_clouds_counts");
    let steps = [Step::Lost { landed: true }];
    script_provider(Call::Acquire, &steps).run(&mut cell);
    cell.run_until(FIRST_ACQUIRE).expect("no invariant fails");
    let total = cell.total_counters();
    assert_eq!(total.get(Counter::AcquireLost), 1);
    assert_eq!(total.get(Counter::AcquireRefused), 0);
    let mut refused = cell_of("the_clouds_counts_refused");
    let denied = [Step::Refuse(RefusalCode::Denied)];
    script_provider(Call::Acquire, &denied).run(&mut refused);
    refused
        .run_until(FIRST_ACQUIRE)
        .expect("no invariant fails");
    assert_eq!(refused.total_counters().get(Counter::AcquireRefused), 1);
}
