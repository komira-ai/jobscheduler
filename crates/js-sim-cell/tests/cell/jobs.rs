//! The job smoke scenario the replay tests run twice and pin in the golden file (family
//! `jobs`): small, so a check that runs on every pull request can afford it.
//!
//! Size: [`HOSTS`] agents beating every 5 s (the server's default) until [`LOSSY_FOR`] plus
//! [`SETTLE`], 600 s, make about 960 heartbeats a seed with the hellos besides.

use std::time::Duration;

use js_sim::{Chance, Faults};
use js_sim_cell::actions::set_bus_faults;
use js_sim_cell::{Expect, Scenario, ServerSpec, Shape, Work};

/// How many hosts.
pub const HOSTS: usize = 8;

/// How many jobs the client submits at time zero, and how long each runs.
pub const JOBS: u32 = 32;
pub const RUN: Duration = Duration::from_secs(60);

/// When the client gives a job up.
pub const GIVE_UP: Duration = Duration::from_secs(900);

/// The share of the agent wire's messages lost, and for how long from the start.
pub const DROP_PERCENT: u32 = 10;
pub const LOSSY_FOR: Duration = Duration::from_secs(300);

/// How long the cell runs after the loss ends, before the end check.
pub const SETTLE: Duration = Duration::from_secs(300);

/// Proves that jobs settle over the job front while one message in ten between the agents
/// and the server is lost for the first 300 s: every job ends exactly once, under every
/// invariant, the five job invariants too. Its trace hash pins every decision of the job
/// front, the per-job agents and the server's job service and job pass, and the job and
/// allocation ids, which follow the seed.
pub fn jobs_on_a_lossy_wire() -> Scenario {
    let lossy = Faults {
        drop: Chance::percent(DROP_PERCENT),
        ..Faults::default()
    };
    Scenario::new("jobs_on_a_lossy_wire")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            HOSTS,
        ))
        .hosts("burst", HOSTS)
        .bus(lossy)
        .work(JOBS, Work::run_for(RUN).give_up_after(GIVE_UP))
        .at(LOSSY_FOR, set_bus_faults(Faults::default()))
        .expect(Expect::all_items_terminal())
        .settle_within(SETTLE)
        .over_jobs()
}
