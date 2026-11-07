//! The parts of the whole-host machinery, each shown to work and to fail when it should:
//! the whole-host job on the real server (placed in turn on one host), the whole-host
//! expectations (those over what the cell observed, at the end or at a virtual time), the
//! record of a client cancel, and what the server spec adds for a whole-node pool (the
//! class, the placement, the job service's address and the architecture).
//!
//! Sizes: one host beating every 5 s for at most 1,800 s of virtual time, under 400 agent
//! requests a seed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use js_sim::Faults;
use js_sim_cell::actions::submit_work;
use js_sim_cell::server::{ARCH, ARCH_KEY, virtual_time};
use js_sim_cell::{
    Cell, EPOCH_MS, Expect, HowEnded, ItemId, Scenario, ScenarioFailure, ServerSpec, Setup, Shape,
    Work,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A scratch directory of its own, removed when dropped.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell"),
        name,
    )
}

/// A whole-node pool of `hosts` hosts.
fn whole_pool(hosts: usize) -> ServerSpec {
    ServerSpec::whole_node_pool("burst", Shape::cpu_host(), hosts)
}

/// Asserts `Expect::held_and_released` failing on the real server: two whole-host jobs
/// submitted together on one host are each placed in turn, run and released (every item
/// succeeds), and an expectation of three held and released fails naming what it wanted
/// and what it got. Catches a front that submits anything but a whole host (the second job
/// would not wait for the first), and an expectation that counts items that were never
/// placed.
#[test]
fn whole_host_jobs_are_placed_in_turn_and_a_wrong_count_fails() {
    let failure = Scenario::new("whole_host_jobs_are_placed_in_turn_and_a_wrong_count_fails")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(whole_pool(1))
        .hosts("burst", 1)
        .work(2, Work::whole_host(secs(100)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(2, HowEnded::Succeeded))
        .expect(Expect::held_and_released(3))
        .settle_within(secs(600))
        .try_run_seeds(1);
    match failure {
        Err(ScenarioFailure::Unmet { detail, .. }) => {
            assert_eq!(detail, "held-and-released: wanted 3, got 2");
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
}

/// Asserts what the server spec adds: a whole-node pool's file names the class
/// `whole-node` and an ordinary pool's `gpu-host`, both are started with `--placement
/// js-sched` and a `--job-listen`, a shape with no architecture is given `arch = x86_64`
/// while one that names an architecture keeps it; and the virtual time of a wall time,
/// from the cell's epoch and floored at zero. Catches a placement that never reaches the
/// server, a job service that is not turned on, an architecture written over, and a time
/// off by the epoch.
#[test]
fn the_server_spec_passes_the_placement_the_class_and_an_arch() {
    let flag = |spec: &ServerSpec, name: &str, flag: &str| {
        let dir = scratch(name);
        let args = spec.args(dir.path()).expect("the files are written");
        let at = args.iter().position(|arg| arg == flag);
        args[at.expect("the flag is passed") + 1].clone()
    };
    let whole = whole_pool(1);
    assert_eq!(
        flag(&whole, "spec-placement", "--placement"),
        OsString::from("js-sched")
    );
    assert_eq!(
        flag(&whole, "spec-job-listen", "--job-listen"),
        OsString::from("127.0.0.1:0")
    );
    assert!(whole.pool_file().contains("class = \"whole-node\""));
    let arch = whole.shape().capabilities.get(ARCH_KEY);
    assert_eq!(arch.map(String::as_str), Some(ARCH));
    let plain = ServerSpec::registered_pool("burst", Shape::cpu_host(), 1);
    assert!(plain.pool_file().contains("class = \"gpu-host\""));
    let arm = Shape {
        capabilities: BTreeMap::from([(ARCH_KEY.to_owned(), "arm64".to_owned())]),
        ..Shape::cpu_host()
    };
    let kept = ServerSpec::registered_pool("burst", arm, 1);
    let arch = kept.shape().capabilities.get(ARCH_KEY);
    assert_eq!(arch.map(String::as_str), Some("arm64"));
    assert_eq!(virtual_time(EPOCH_MS + 5_000), ms(5_000));
    assert_eq!(virtual_time(0), ms(0));
}

/// Asserts that `Cell::cancel_item` records, in the observations, the item and the
/// virtual time the client first asked to cancel it: a second ask later keeps the first
/// time. Catches a cancel the cell never records (every `drain-kills-nothing` check would
/// then read the client as never having asked) and one recorded at its latest ask.
#[test]
fn a_client_cancel_is_recorded_at_the_time_it_was_first_asked() {
    let dir = scratch("cancel-record");
    let mut cell = Cell::new(Setup::new(2), whole_pool(1), dir, Faults::default()).expect("starts");
    cell.add_agent().expect("an agent name is a host id");
    let item = cell.submit(&Work::whole_host(secs(600)));
    assert!(cell.observations().cancels.is_empty());
    cell.run_until(ms(3_000)).expect("no invariant fails");
    cell.cancel_item(item);
    cell.run_until(ms(7_000)).expect("no invariant fails");
    cell.cancel_item(item);
    let cancels: Vec<(ItemId, FarmTime)> = cell
        .observations()
        .cancels
        .iter()
        .map(|(item, at)| (*item, *at))
        .collect();
    assert_eq!(cancels, [(item, ms(3_000))]);
}

/// Asserts that an expectation at a virtual time is judged then and fails its seed: a job
/// submitted at 5 s is not yet placed when `held_and_released(1)` is judged at 6 s (it
/// fails the seed as `at 6000 ms` and the statement); the same statement judged at the end
/// is met; and with every timed statement met, a statement judged at the end that is not
/// fails the seed. Catches a timed statement judged at the end, and one whose failure is
/// dropped.
#[test]
fn an_expectation_at_a_virtual_time_is_judged_then_and_fails_its_seed() {
    let at = |name: &str, when: Duration, timed: Expect, last: Expect| {
        Scenario::new(name)
            .scratch(env!("CARGO_TARGET_TMPDIR"))
            .server(whole_pool(1))
            .hosts("burst", 1)
            .at(secs(5), submit_work(1, Work::whole_host(secs(1))))
            .expect_at(when, timed)
            .expect(last)
            .settle_within(secs(120))
            .try_run_seeds(1)
    };
    let released = Expect::held_and_released(1);
    let met = Expect::all_items_terminal();
    let early = at(
        "expect_at_before_the_end",
        secs(6),
        released.clone(),
        met.clone(),
    );
    let wanted = "at 6000 ms held-and-released: wanted 1, got 0";
    match early {
        Err(ScenarioFailure::Unmet { detail, seed, .. }) => {
            assert_eq!((detail.as_str(), seed), (wanted, 0));
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
    assert!(at("expect_at_after_the_end", secs(100), released, met).is_ok());
    let second = Expect::held_and_released(2);
    match at(
        "expect_at_then_at_the_end",
        secs(100),
        Expect::all_items_terminal(),
        second,
    ) {
        Err(ScenarioFailure::Unmet { detail, .. }) => {
            assert_eq!(detail, "held-and-released: wanted 2, got 1");
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
}

/// Asserts the whole-host expectations on the real server, met and failing: one host, one
/// whole-host job. Met: the placement after the runs on its host, every host serving but
/// none. Failing, each naming what it wanted and got: the host out of service, and a
/// placement of an item never submitted. Catches an expectation that is met by any run (a
/// vacuous one), and one whose statement says another thing than it judges.
#[test]
fn the_whole_host_expectations_are_met_and_fail_on_the_real_server() {
    let base = || {
        Scenario::new("whole_host_expectations")
            .scratch(env!("CARGO_TARGET_TMPDIR"))
            .server(whole_pool(1))
            .hosts("burst", 1)
            .work(1, Work::whole_host(secs(10)))
            .settle_within(secs(300))
    };
    base()
        .expect(Expect::held_after_the_runs_on_its_host(ItemId(0)))
        .expect(Expect::serving_but(&[]))
        .run_seeds(1);
    let unmet = [
        (
            Expect::serving_but(&[0]),
            "hosts-not-serving: wanted [h000], got []",
        ),
        (
            Expect::held_after_the_runs_on_its_host(ItemId(4)),
            "held-after-the-runs-on-its-host(item4): wanted [], got not held",
        ),
    ];
    for (expect, wanted) in unmet {
        match base().expect(expect).try_run_seeds(1) {
            Err(ScenarioFailure::Unmet { detail, .. }) => assert_eq!(detail, wanted),
            other => panic!("not an unmet expectation: {other:?}"),
        }
    }
}
