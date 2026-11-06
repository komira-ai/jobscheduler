//! The smoke scenario of a lossy agent wire, shared by the smoke family, which runs it,
//! and the replay tests, which pin its trace hashes in the golden file.
//!
//! Size: [`HOSTS`] agents beating every 5 s (the server's default) until
//! [`LOSSY_FOR`] plus [`SETTLE`], 2,100 s, make about 8,400 heartbeats a seed with the
//! hellos besides.

use std::time::Duration;

use js_sim::{Chance, Faults};
use js_sim_cell::actions::set_bus_faults;
use js_sim_cell::{Counter, Expect, Scenario, ServerSpec, Shape, Work};

/// How many hosts, and so how many runs at once.
pub const HOSTS: usize = 20;

/// How many runs the client submits at time zero, and how long each lasts.
pub const RUNS: u32 = 60;
pub const RUN: Duration = Duration::from_secs(120);

/// When the client gives an item up.
pub const GIVE_UP: Duration = Duration::from_secs(1_800);

/// The share of the agent wire's messages lost, and for how long from the start.
pub const DROP_PERCENT: u32 = 10;
pub const LOSSY_FOR: Duration = Duration::from_secs(900);

/// How long the cell runs after the loss ends, before the end check.
pub const SETTLE: Duration = Duration::from_secs(1_200);

/// Proves that the client's work settles while one message in ten between the agents and
/// the server is lost: every item ends exactly once, no run is launched twice, no host
/// runs more than it holds, and once the loss has ended nothing is left booked or held.
/// The witness shows that loss reached a result: some result was sent again because the
/// answer that acknowledged it was lost. It turns red for a server that relaunches a run
/// whose start or result it hears again (`at-most-one-launch`), one that books a host
/// twice when an answer is lost (`no-double-booking`), and one that keeps a booking after
/// its release (`bookings-match-agents`, `settles-when-quiet`).
pub fn a_lossy_agent_wire_settles_every_run() -> Scenario {
    let lossy = Faults {
        drop: Chance::percent(DROP_PERCENT),
        ..Faults::default()
    };
    Scenario::new("a_lossy_agent_wire_settles_every_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            HOSTS,
        ))
        .hosts("burst", HOSTS)
        .bus(lossy)
        .work(RUNS, Work::run_for(RUN).give_up_after(GIVE_UP))
        .at(LOSSY_FOR, set_bus_faults(Faults::default()))
        .expect(Expect::all_items_terminal())
        .witness("result_resent", |c| c.get(Counter::ResultResent) >= 1)
        .settle_within(SETTLE)
}
