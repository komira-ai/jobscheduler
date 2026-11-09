//! The cordon family: an operator cordons a host (`POST /v1/hosts/{id}:cordon`, through the
//! job service's router) and the server places no new job on it while the cordon holds and
//! goes on with the jobs already there. Each scenario runs under every invariant of
//! `Invariant::ALL`, `cordon-never-kills-running-job` among them, and states the rest with
//! `expect_that`; the calls the cell makes (cordon, uncordon, and what it writes down of
//! them) are shown on a cell of their own.
//!
//! Sizes, in agent requests a seed: two hosts beating every 5 s for [`SETTLE`] after the
//! last of the work is submitted: about 150 a seed.

use std::collections::BTreeSet;
use std::time::Duration;

use js_jobs::{AllocationStep, JobState};
use js_sim::Faults;
use js_sim_cell::actions::{
    cancel_work, cordon_host, items, restart_server, submit_work, uncordon_host,
};
use js_sim_cell::{
    Action, Cell, Cordon, Expect, HowEnded, ItemId, Scenario, ServerSpec, Setup, Shape, Uncordon,
    Work,
};
use js_types::HostId;
use js_types::sched::FarmTime;

use js_testkit::dir::TestDir;
use std::path::Path;

/// Virtual time `n` milliseconds after the start.
fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A scratch directory of its own for one server, removed when the server is dropped.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell");
    TestDir::under(root, name)
}

/// Two hosts of 8 cores each.
const HOSTS: usize = 2;

/// One job of 100 s at the start, and twelve of 30 s once the cordon is set: more than the
/// other host's eight cores hold at once, so four would be placed on the cordoned host
/// were it open.
const LONG: Duration = Duration::from_secs(100);
const SHORT: Duration = Duration::from_secs(30);
const MORE: u32 = 12;
const CORDON_AT: Duration = Duration::from_secs(20);
const MORE_AT: Duration = Duration::from_secs(25);
const SETTLE: Duration = Duration::from_secs(300);

/// How many seeds each scenario runs.
const SEEDS: u64 = 3;

/// Cordons the host the job of item 0 is placed on, for an hour, as an operator reads the
/// host off the job and cordons it.
#[derive(Clone)]
struct CordonItemsHost(ItemId);

impl Action for CordonItemsHost {
    fn run(&self, cell: &mut Cell) {
        let job = format!("sim-job-{}", self.0.0);
        let reads = &cell.observations().allocation_reads;
        let host = reads.iter().find(|read| read.row.job.as_str() == job);
        let host = host.map(|read| read.row.host.clone());
        let index =
            (0..cell.agent_count()).find(|i| cell.agent(*i).map(|a| a.host()) == host.as_ref());
        // An item whose job was never read placed cordons nothing, which `cordoned_once`
        // states.
        index
            .into_iter()
            .for_each(|index| cordon_host(index, 3_600).run(cell));
    }
}

/// The host item 0 was placed on, and when the cell cordoned a host: what a statement over
/// a settled cell reads.
fn cordoned_host(cell: &Cell) -> Option<Cordon> {
    cell.observations().cordons.first().cloned()
}

/// A scenario of two hosts and the long job of item 0, the cordon of its host at 20 s, and
/// the twelve short jobs at 25 s, as `build` adds to it, named `name`.
fn world(name: &str) -> Scenario {
    let spec = ServerSpec::whole_node_pool("burst", Shape::cpu_host(), HOSTS);
    Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", HOSTS)
        .work(1, Work::run_for(LONG))
        .at(CORDON_AT, CordonItemsHost(ItemId(0)))
}

/// Proves that a cordon takes a host out of placement and does nothing else: item 0's job
/// runs on a host that is cordoned at 20 s and succeeds there; twelve more jobs submitted
/// at 25 s are all placed on the other host (four of them wait for its cores, where they
/// would have gone to the cordoned host), and every one succeeds. States that the cell
/// cordoned once, that item 0 and its host are the cordon's, that item 0 has one
/// allocation, on the cordoned host, and that no allocation was created on the cordoned host
/// after the cordon (`cordon-never-kills-running-job` states it at every step; the
/// statement reads it from the table). Red for a server that places a job on a cordoned host,
/// ends a running job, or sends it a stop because of the cordon (the invariant), and for one
/// that never places the waiting jobs elsewhere (the counts of successes).
#[test]
fn a_cordoned_host_keeps_its_running_job_and_takes_no_new_one() {
    world("a_cordoned_host_keeps_its_running_job_and_takes_no_new_one")
        .at(MORE_AT, submit_work(MORE, Work::run_for(SHORT)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            1 + MORE as usize,
            HowEnded::Succeeded,
        ))
        .expect(Expect::job_state(ItemId(0), JobState::Succeeded))
        .expect(Expect::allocations_of(ItemId(0), 1))
        .expect(Expect::allocation_step(ItemId(0), AllocationStep::Freed))
        .expect(Expect::no_allocation_live())
        .expect_that("cordoned_once", |cell| {
            let count = cell.observations().cordons.len();
            (count != 1).then(|| format!("wanted one cordon, got {count}"))
        })
        .expect_that("no_placement_on_the_cordoned_host", |cell| {
            let cordon = cordoned_host(cell)?;
            let reads = &cell.observations().allocation_reads;
            let after = reads.iter().filter(|read| read.row.created_at > cordon.at);
            let placed: BTreeSet<_> = after.clone().map(|read| read.row.allocation).collect();
            let there = after.filter(|read| read.row.host == cordon.host);
            let on = there.map(|read| read.row.allocation).collect::<BTreeSet<_>>().len();
            let wanted = MORE as usize;
            let met = (on == 0) & (placed.len() >= wanted);
            (!met).then(|| {
                let (host, placed) = (&cordon.host, placed.len());
                format!("wanted {wanted} allocations after the cordon and none on {host}, got {placed} and {on}")
            })
        })
        .settle_within(SETTLE)
        .run_seeds(SEEDS);
}

/// Proves that the client's own cancel of the job of a cordoned host is its own end: item 0
/// is cancelled at 40 s, after its host is cordoned at 20 s, and ends cancelled, its
/// allocation freed, with no violation of `cordon-never-kills-running-job`. Red for a
/// check that reads the client's cancel as the cordon's kill (the invariant), and for a
/// server that does not end the job (the expects).
#[test]
fn a_cancel_the_client_asks_for_on_a_cordoned_host_ends_the_job_without_a_violation() {
    // The page's check finds a scenario by `Scenario::new("<name>")` on one line.
    #[rustfmt::skip]
    let scenario = world("a_cancel_the_client_asks_for_on_a_cordoned_host_ends_the_job_without_a_violation");
    scenario
        .at(Duration::from_secs(40), cancel_work(items(0..1)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(1, HowEnded::Cancelled))
        .expect(Expect::job_state(ItemId(0), JobState::Cancelled))
        .expect(Expect::no_allocation_live())
        .settle_within(SETTLE)
        .run_seeds(SEEDS);
}

/// Proves that a cordon survives a restart of the server, and that an uncordon opens the
/// host again: the host of item 0 is cordoned at 20 s for an hour, the server is restarted
/// at 40 s, and twelve jobs submitted at 60 s are placed on the other host only; the host is
/// uncordoned at 120 s and the cell wrote both calls down. Item 0 succeeds. Red for a server
/// that forgets the cordon over a restart (`cordon-never-kills-running-job`, from the
/// placement on the cordoned host) and for an uncordon the server refuses (the statement).
#[test]
fn a_cordon_holds_over_a_restart_and_ends_at_the_uncordon() {
    // The page's check finds a scenario by `Scenario::new("<name>")` on one line.
    #[rustfmt::skip]
    let scenario = world("a_cordon_holds_over_a_restart_and_ends_at_the_uncordon");
    scenario
        .at(Duration::from_secs(40), restart_server())
        .at(
            Duration::from_secs(60),
            submit_work(MORE, Work::run_for(SHORT)),
        )
        .at(Duration::from_secs(120), UncordonItemsHost)
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            1 + MORE as usize,
            HowEnded::Succeeded,
        ))
        .expect_that("uncordoned_once", |cell| {
            let lifts = &cell.observations().uncordons;
            let cordon = cordoned_host(cell)?;
            let lifted = lifts.first().is_some_and(|lift| lift.host == cordon.host);
            (!lifted).then(|| "wanted the cordoned host uncordoned once, got none".to_owned())
        })
        .expect_that(
            "no_placement_on_the_cordoned_host_over_the_restart",
            |cell| {
                let cordon = cordoned_host(cell)?;
                let reads = &cell.observations().allocation_reads;
                let on = reads
                    .iter()
                    .filter(|read| read.row.created_at > cordon.at)
                    .filter(|read| read.row.host == cordon.host)
                    .count();
                (on != 0).then(|| format!("wanted none, got {on} allocations on {}", cordon.host))
            },
        )
        .settle_within(SETTLE)
        .run_seeds(SEEDS);
}

/// Lifts the cordon of the host the cell cordoned.
#[derive(Clone)]
struct UncordonItemsHost;

impl Action for UncordonItemsHost {
    fn run(&self, cell: &mut Cell) {
        let host = cordoned_host(cell).map(|cordon| cordon.host);
        let index =
            (0..cell.agent_count()).find(|i| cell.agent(*i).map(|a| a.host()) == host.as_ref());
        index
            .into_iter()
            .for_each(|index| uncordon_host(index).run(cell));
    }
}

/// A cell of two agents that have said hello, at 12 s.
fn two_agents(name: &str) -> Cell {
    let spec = ServerSpec::whole_node_pool("burst", Shape::cpu_host(), HOSTS);
    let mut cell =
        Cell::new(Setup::new(5), spec, scratch(name), Faults::default()).expect("the cell starts");
    for _ in 0..HOSTS {
        cell.add_agent().expect("an agent name is a host id");
    }
    cell.run_until(ms(12_000)).expect("no violation");
    cell
}

fn host(name: &str) -> HostId {
    HostId::new(name).expect("a host id")
}

/// Asserts what the cell writes down of its operator calls: a cordon the server answers 200
/// is a [`Cordon`] ending its length after the call, a length past the server's longest
/// (an hour) cut to it, a renewal a second entry, an uncordon an [`Uncordon`]; a cordon of no
/// length is refused 400 and written down as nothing, and a call for an agent the cell has
/// not made is not made at all (status 0). Catches a cordon written down that the server
/// refused, one written down at its asked length when the server cuts it, and a call made
/// for no host.
#[test]
fn the_cell_writes_down_the_cordons_and_uncordons_the_server_answered() {
    let mut cell = two_agents("cordon-calls");
    assert_eq!(cell.cordon_host(0, 0), 400);
    assert_eq!(cell.cordon_host(9, 60), 0);
    assert_eq!(cell.uncordon_host(9), 0);
    assert_eq!(cell.observations().cordons, []);
    assert_eq!(cell.cordon_host(0, 60), 200);
    assert_eq!(cell.cordon_host(1, 7_200), 200);
    assert_eq!(cell.cordon_host(0, 120), 200);
    assert_eq!(cell.uncordon_host(1), 200);
    let at = FarmTime::default().saturating_add(Duration::from_secs(12));
    let until = |secs: u64| at.saturating_add(Duration::from_secs(secs));
    let made = |name: &str, until| Cordon {
        host: host(name),
        at,
        until,
    };
    assert_eq!(
        cell.observations().cordons,
        [
            made("h000", until(60)),
            made("h001", until(3_600)),
            made("h000", until(120)),
        ]
    );
    assert_eq!(
        cell.observations().uncordons,
        [Uncordon {
            host: host("h001"),
            at
        }]
    );
}
