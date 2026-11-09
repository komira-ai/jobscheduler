//! The smoke scenario of a hundred hosts, shared by the smoke family, which runs it over its
//! few seeds, and the sweep (`tests/sweep.rs`), which runs it over many.
//!
//! Size: [`HOSTS`] hosts beating every [`INTERVAL`] for [`SETTLE`] make 2,000 heartbeats a
//! seed, with the hellos besides.

use std::time::Duration;

use js_sim_cell::{Expect, HowEnded, Scenario, ServerSpec, Shape, Work};

/// A hundred hosts, each of which runs eight of these jobs at once.
pub const HOSTS: usize = 100;
pub const RUNS: u32 = 400;
pub const RUN: Duration = Duration::from_secs(300);
pub const GIVE_UP: Duration = Duration::from_secs(2_400);
pub const INTERVAL: Duration = Duration::from_secs(30);
pub const GRACE: Duration = Duration::from_secs(90);
pub const SETTLE: Duration = Duration::from_secs(600);

/// 400 runs of 300 s on a hundred hosts that beat every 30 s, with no fault: every item
/// ends, and every one succeeds.
pub fn a_hundred_hosts_settle_four_hundred_runs() -> Scenario {
    Scenario::new("a_hundred_hosts_settle_four_hundred_runs")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(
            ServerSpec::registered_pool("burst", Shape::gpu_host(), HOSTS)
                .heartbeat(INTERVAL, GRACE),
        )
        .hosts("burst", HOSTS)
        .work(RUNS, Work::run_for(RUN).give_up_after(GIVE_UP))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(RUNS as usize, HowEnded::Succeeded))
        .settle_within(SETTLE)
}
