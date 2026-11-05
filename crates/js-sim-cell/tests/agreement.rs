//! The agreement family: the fake agent held to the real one. Each scenario runs over
//! [`SEEDS`] seeds twice, once with every agent the conformance suite's fake and once with
//! every agent the real lifecycle of `js-agent` over the in-memory runtime, and the
//! client's outcome of every item (the terminal it heard) is compared seed by seed
//! (`js_sim_cell::compare`). Both runs of a seed must also pass every invariant, expect
//! and witness of the scenario. The traces are not compared: the two agents send different
//! bodies.
//!
//! The world is `cell/real_agents.rs`: six hosts of one GPU, twelve runs of 60 s. The
//! stale start is the bus configuration of the harness's own negative case
//! (`broken_agent.rs`): eight hosts, sixteen runs, every item cancelled while its start is
//! on a bus that delays a message by more than the heartbeat interval and lets a later one
//! overtake it.
//!
//! The family is shown able to fail by a fake that forgets an unacknowledged result when
//! its connection drops: on the lossy wire it disagrees with the real agent.

#[path = "cell/real_agents.rs"]
mod real_agents;

use std::collections::BTreeMap;
use std::time::Duration;

use js_conformance::agent::Defect;
use js_sim::{Chance, Faults};
use js_sim_cell::actions::{
    agents, cancel_work, items, kill_agents, partition_agents, replace_agents, set_bus_faults,
};
use js_sim_cell::{
    Counter, Disagreement, Expect, HowEnded, ItemId, Outcome, RunId, Scenario, ServerSpec, Shape,
    Side, Work, compare, differences,
};

use crate::real_agents::{GIVE_UP, HOSTS, POOL, RUN, RUNS, SETTLE, with_hosts, world};

/// How many seeds each scenario runs on each side.
const SEEDS: u64 = 8;

/// The lossy wire: this share of the messages lost for the first [`LOSSY_FOR`].
const DROP_PERCENT: u32 = 15;
const LOSSY_FOR: Duration = Duration::from_secs(300);

/// The partition: a third of the agents, the first two, cut off for 30 s while their runs
/// run (longer than the server's grace of 15 s), and healed. They are named, not drawn:
/// a draw from the seed follows every earlier draw of the bus, so the two sides would cut
/// different agents as soon as their agents sent different numbers of messages.
const CUT_AGENTS: usize = HOSTS / 3;
const CUT_AT: Duration = Duration::from_secs(30);
const CUT_FOR: Duration = Duration::from_secs(30);

/// The replacement: two agents killed while their runs run, and replaced ten seconds
/// later.
const KILLED: std::ops::Range<usize> = 0..2;
const KILL_AT: Duration = Duration::from_secs(30);
const REPLACE_AT: Duration = Duration::from_secs(40);

/// The stale start, as `broken_agent.rs` sets it.
const STALE_HOSTS: usize = 8;
const STALE_RUNS: u32 = 16;
const STALE_RUN: Duration = Duration::from_secs(30);
const STALE_GIVE_UP: Duration = Duration::from_secs(300);
const STALE_INTERVAL: Duration = Duration::from_secs(1);
const STALE_GRACE: Duration = Duration::from_secs(6);
const STALE_FAULTS_AT: Duration = Duration::from_secs(2);
const STALE_MIN_DELAY: Duration = Duration::from_millis(1_000);
const STALE_MAX_DELAY: Duration = Duration::from_millis(4_000);
const STALE_REORDER_PERCENT: u32 = 100;
const STALE_CANCEL_AT: Duration = Duration::from_millis(6_500);
const STALE_QUIET_AT: Duration = Duration::from_secs(30);
const STALE_SETTLE: Duration = Duration::from_secs(120);

/// Asserts that the fake and the real side of `make` agree on every item of every seed,
/// printing each disagreement (with the trace lines of both runs) when they do not.
fn agree(make: impl Fn(Side) -> Scenario) {
    let found = compare(&make(Side::Fake), &make(Side::Real), SEEDS);
    let report: Vec<String> = found.iter().map(ToString::to_string).collect();
    assert!(found.is_empty(), "{}", report.join("\n"));
}

/// The lossy wire's scenario, on `scenario` with its hosts already added.
fn lossy_on(scenario: Scenario) -> Scenario {
    let lossy = Faults {
        drop: Chance::percent(DROP_PERCENT),
        ..Faults::default()
    };
    scenario
        .bus(lossy)
        .at(LOSSY_FOR, set_bus_faults(Faults::default()))
        .witness("result_resent", |c| c.get(Counter::ResultResent) >= 1)
}

fn lossy(side: Side) -> Scenario {
    lossy_on(world("on_a_lossy_wire", side))
}

fn partition(side: Side) -> Scenario {
    let cut = partition_agents(agents(0..CUT_AGENTS), CUT_FOR);
    world("across_a_partition", side).at(CUT_AT, cut)
}

fn replaced(side: Side) -> Scenario {
    world("when_an_agent_is_replaced_mid_run", side)
        .at(KILL_AT, kill_agents(agents(KILLED)))
        .at(REPLACE_AT, replace_agents(agents(KILLED)))
}

fn stale_start(side: Side) -> Scenario {
    let spec = ServerSpec::registered_pool(POOL, Shape::gpu_host(), STALE_HOSTS)
        .heartbeat(STALE_INTERVAL, STALE_GRACE);
    let late = Faults {
        min_delay: STALE_MIN_DELAY,
        max_delay: STALE_MAX_DELAY,
        reorder: Chance::percent(STALE_REORDER_PERCENT),
        ..Faults::default()
    };
    let work = Work::run_for(STALE_RUN).give_up_after(STALE_GIVE_UP);
    let scenario = Scenario::new("for_a_stale_start_after_a_cancel")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .work(STALE_RUNS, work)
        .at(STALE_FAULTS_AT, set_bus_faults(late))
        .at(STALE_CANCEL_AT, cancel_work(items(0..STALE_RUNS)))
        .at(STALE_QUIET_AT, set_bus_faults(Faults::default()))
        .expect(Expect::all_items_terminal())
        .witness("start_after_stop", |c| c.get(Counter::StartAfterStop) >= 1)
        .settle_within(STALE_SETTLE);
    with_hosts(scenario, side, STALE_HOSTS)
}

/// Asserts that with no fault every item ends the same way with fake agents and with real
/// ones, over eight seeds. Catches a fake that runs, reports or ends a run differently from
/// the agent it stands for when nothing goes wrong.
#[test]
fn fake_and_real_agents_give_the_client_the_same_outcomes_without_faults() {
    agree(real_agents::without_faults);
}

/// Asserts that while 15% of the agent wire's messages are lost for 300 s every item ends
/// the same way with fake agents and with real ones, over eight seeds, and on each side a
/// result was sent again because its acknowledgement was lost. Catches a fake that drops,
/// or keeps past its acknowledgement, a result the real agent resends until acknowledged
/// (design section 7, "Result lost").
#[test]
fn fake_and_real_agents_give_the_client_the_same_outcomes_on_a_lossy_wire() {
    agree(lossy);
}

/// Asserts that when a third of the agents are cut off for 30 s while their runs run, and
/// the cut heals, every item ends the same way with fake agents and with real ones, over
/// eight seeds. Catches a fake that answers a host gone lost and back differently from
/// the real agent (it keeps or drops the run, or reports it).
#[test]
fn fake_and_real_agents_give_the_client_the_same_outcomes_across_a_partition() {
    agree(partition);
}

/// Asserts that when two agents are killed while their runs run and replaced ten seconds
/// later, every item ends the same way with fake agents and with real ones, over eight
/// seeds. Catches a fake whose replacement remembers what the real agent's replacement
/// does not, or forgets what it keeps (design section 8, rows 4 and 31).
#[test]
fn fake_and_real_agents_give_the_client_the_same_outcomes_when_an_agent_is_replaced_mid_run() {
    agree(replaced);
}

/// Asserts that when every item is cancelled while its starts are on a bus that delays a
/// message past the heartbeat interval and lets a later one overtake it, every item ends
/// the same way with fake agents and with real ones, over eight seeds, and on each side
/// some start arrived after the stop that follows it. Catches a fake that runs a start the
/// real agent refuses after a stop (design section 7, "Stop of an unknown job" and
/// "Duplicate start"), or refuses one the real agent runs.
#[test]
fn fake_and_real_agents_give_the_client_the_same_outcomes_for_a_stale_start_after_a_cancel() {
    agree(stale_start);
}

/// Asserts that the fake that forgets an unacknowledged result when its connection drops
/// (`Defect::ForgetsUnackedResult`) disagrees with the real agent on the lossy wire: the
/// comparison reports at least one disagreement. This is the check that the family can
/// fail. Catches a comparison too weak to see a lost result.
#[test]
fn a_fake_agent_with_a_defect_disagrees_with_the_real_agent_on_a_lossy_wire() {
    let broken = Scenario::new("on_a_lossy_wire_with_a_broken_fake")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(POOL, Shape::gpu_host(), HOSTS))
        .work(RUNS, Work::run_for(RUN).give_up_after(GIVE_UP))
        .expect(Expect::all_items_terminal())
        .settle_within(SETTLE)
        .broken_hosts(POOL, HOSTS, Defect::ForgetsUnackedResult);
    let found = compare(&lossy_on(broken), &lossy(Side::Real), SEEDS);
    assert!(
        !found.is_empty(),
        "the broken fake agrees with the real agent"
    );
}

fn outcome(terminal: Option<HowEnded>, run: &str) -> Outcome {
    Outcome {
        terminal,
        run: Some(RunId::new(run)),
    }
}

/// Asserts that the comparison, given two outcome maps that differ in one item's terminal
/// (and in the run ids the server gave, which are not compared), returns that item and
/// nothing else; and that an item one side lacks counts as one that never ended there.
/// Catches a comparison that ignores a difference or reports an item that agrees.
#[test]
fn an_agreement_check_reports_a_run_whose_outcomes_differ() {
    let fake = BTreeMap::from([
        (ItemId(0), outcome(Some(HowEnded::Succeeded), "f0")),
        (ItemId(1), outcome(Some(HowEnded::Succeeded), "f1")),
        (ItemId(2), outcome(None, "f2")),
    ]);
    let mut real = BTreeMap::from([
        (ItemId(0), outcome(Some(HowEnded::Succeeded), "r0")),
        (ItemId(1), outcome(Some(HowEnded::Lost), "r1")),
        (ItemId(2), outcome(None, "r2")),
    ]);
    assert_eq!(differences(&fake, &real), [ItemId(1)]);
    assert!(differences(&fake, &fake).is_empty());
    real.remove(&ItemId(2));
    assert_eq!(
        differences(&fake, &real),
        [ItemId(1)],
        "never ended either way"
    );
    real.remove(&ItemId(0));
    assert_eq!(differences(&fake, &real), [ItemId(0), ItemId(1)]);
}

/// Asserts that a seed whose items end differently is reported once, naming the scenario,
/// the seed, each item with its outcome on each side (an item one side never had reads
/// `terminal none (run none)`) and the last forty trace lines of both runs. The two sides
/// here are two scenarios of fake agents, one of which cancels its items at once and
/// submits one more. Catches a report that loses the item, the seed or the traces a
/// finding needs.
#[test]
fn a_comparison_names_each_item_that_ended_differently_with_both_traces() {
    let runs = |name: &str, count: u32| {
        Scenario::new(name)
            .scratch(env!("CARGO_TARGET_TMPDIR"))
            .server(ServerSpec::registered_pool(POOL, Shape::gpu_host(), 2))
            .hosts(POOL, 2)
            .work(count, Work::run_for(Duration::from_secs(10)))
            .expect(Expect::all_items_terminal())
            .settle_within(Duration::from_secs(60))
    };
    let ran = runs("two_runs", 2);
    let cancelling = runs("two_runs", 3).at(Duration::ZERO, cancel_work(items(0..3)));
    let found = compare(&ran, &cancelling, 1);
    assert_eq!(found.len(), 1, "{found:?}");
    let Disagreement::Outcomes {
        scenario,
        seed,
        items,
        fake_lines,
        real_lines,
    } = &found[0]
    else {
        panic!("not an outcome disagreement: {}", found[0]);
    };
    assert_eq!((scenario.as_str(), *seed), ("two_runs", 0));
    let named: Vec<(u32, Option<HowEnded>, Option<HowEnded>)> = items
        .iter()
        .map(|d| (d.item.0, d.fake.terminal, d.real.terminal))
        .collect();
    let succeeded = Some(HowEnded::Succeeded);
    let cancelled = Some(HowEnded::Cancelled);
    let wanted = [
        (0, succeeded, cancelled),
        (1, succeeded, cancelled),
        (2, None, cancelled),
    ];
    assert_eq!(named, wanted);
    assert_eq!((fake_lines.len(), real_lines.len()), (40, 40));
    let text = found[0].to_string();
    assert!(
        text.starts_with("scenario two_runs seed 0: the agents disagree\n"),
        "{text}"
    );
    assert!(
        text.contains("\n  item 2: fake terminal none (run none), real terminal cancelled"),
        "{text}"
    );
    assert!(
        text.contains("last 40 trace lines, fake agents:\n"),
        "{text}"
    );
    assert!(
        text.contains("last 40 trace lines, real agents:\n"),
        "{text}"
    );
    assert!(text.ends_with(&format!("  {}\n", real_lines[39])), "{text}");
}

/// Asserts that when a side's scenario fails (here neither names a scratch directory, so
/// no cell starts) the comparison reports each failed side and compares nothing. Catches a
/// comparison that reads a failed side as one that agrees.
#[test]
fn a_comparison_reports_a_side_whose_scenario_fails() {
    let unstarted = Scenario::new("no_scratch").hosts(POOL, 1);
    let found = compare(&unstarted, &unstarted, 1);
    let sides: Vec<Side> = found
        .iter()
        .map(|found| match found {
            Disagreement::Failed { side, .. } => *side,
            other => panic!("not a failed side: {other}"),
        })
        .collect();
    assert_eq!(sides, [Side::Fake, Side::Real]);
    let text = found[0].to_string();
    let heading = "with fake agents the scenario failed: scenario no_scratch seed 0 did not start";
    assert!(text.starts_with(heading), "{text}");
    assert!(found[1].to_string().starts_with("with real agents"));
}
