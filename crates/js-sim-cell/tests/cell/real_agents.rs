//! The agreement family's world, shared by `agreement.rs`, which runs each scenario with
//! fake agents and with real ones and compares the client's outcomes, and the replay tests,
//! which pin the trace hashes of the scenario without faults, with real agents, in the
//! golden file (family `real_agent`).
//!
//! Size: [`HOSTS`] agents beating every 5 s (the server's default) for the run and
//! [`SETTLE`], about 1,000 s, make about 1,200 heartbeats a seed with the hellos besides.
//! The real agent writes its held list at every step, so the family keeps to a handful of
//! hosts; the fake is the agent a cell runs by the hundred.

use std::time::Duration;

use js_sim_cell::{Expect, Scenario, ServerSpec, Shape, Side, Work};

/// The pool every host joins.
pub const POOL: &str = "burst";

/// How many hosts, one GPU each, and so how many runs at a time.
pub const HOSTS: usize = 6;

/// How many runs the client submits at time zero, and how long each lasts.
pub const RUNS: u32 = 12;
pub const RUN: Duration = Duration::from_secs(60);

/// When the client gives an item up.
pub const GIVE_UP: Duration = Duration::from_secs(1_200);

/// How long the cell runs after its last action, before the end check.
pub const SETTLE: Duration = Duration::from_secs(900);

/// A scenario named `name` of [`HOSTS`] hosts of `side`'s agents, with [`RUNS`] runs of
/// [`RUN`] submitted at time zero, every item expected to end.
pub fn world(name: &str, side: Side) -> Scenario {
    let scenario = Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(POOL, Shape::gpu_host(), HOSTS))
        .work(RUNS, Work::run_for(RUN).give_up_after(GIVE_UP))
        .expect(Expect::all_items_terminal())
        .settle_within(SETTLE);
    with_hosts(scenario, side, HOSTS)
}

/// `scenario` with `count` more hosts of [`POOL`], fake or real as `side` says.
pub fn with_hosts(scenario: Scenario, side: Side, count: usize) -> Scenario {
    match side {
        Side::Fake => scenario.hosts(POOL, count),
        Side::Real => scenario.real_hosts(POOL, count),
    }
}

/// The world with no fault: every run is placed, runs and ends.
pub fn without_faults(side: Side) -> Scenario {
    world("without_faults", side)
}
