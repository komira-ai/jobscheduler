//! The smoke scenario of a partition that heals, shared by the smoke family, which runs it
//! over its few seeds, and the sweep (`tests/sweep.rs`), which runs it over many.
//!
//! Size: [`HOSTS`] hosts beating every 5 s (the server's default) until [`CUT_AT`],
//! [`CUT_FOR`] and [`SETTLE`], 1,920 s, make about 7,700 heartbeats a seed.

use std::time::Duration;

use js_sim_cell::actions::{drawn_agents, partition_agents};
use js_sim_cell::{Expect, Scenario, ServerSpec, Shape, Work};

/// Ten of twenty agents, drawn from the seed, cut for longer than the grace (15 s, the
/// server's default) and healed.
pub const HOSTS: usize = 20;
pub const RUNS: u32 = 60;
pub const RUN: Duration = Duration::from_secs(120);
pub const GIVE_UP: Duration = Duration::from_secs(1_800);
pub const CUT_AGENTS: usize = 10;
pub const CUT_AT: Duration = Duration::from_secs(60);
pub const CUT_FOR: Duration = Duration::from_secs(60);
pub const SETTLE: Duration = Duration::from_secs(1_800);

/// Sixty runs of 120 s on twenty hosts, ten of them cut off at 60 s for 60 s while they run
/// and healed: every item ends.
pub fn a_partition_of_ten_agents_that_heals_settles_every_run() -> Scenario {
    Scenario::new("a_partition_of_ten_agents_that_heals_settles_every_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            HOSTS,
        ))
        .hosts("burst", HOSTS)
        .work(RUNS, Work::run_for(RUN).give_up_after(GIVE_UP))
        .at(CUT_AT, partition_agents(drawn_agents(CUT_AGENTS), CUT_FOR))
        .expect(Expect::all_items_terminal())
        .settle_within(SETTLE)
}
