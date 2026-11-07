//! The first whole-host scenario, shared by the whole-host family (`whole_host.rs`) and the
//! replay tests, which pin its trace hashes in the golden file (family `whole_host`).
//!
//! Size: [`HOSTS`] agents beating every 5 s (the server's default) for [`SETTLE`], which
//! is how long a seed runs after the jobs are submitted at time zero: `HOSTS * 1_200 / 5`
//! = 4,800 heartbeats a seed, with the hellos besides. The fifty jobs go in [`REQUESTS`] /
//! [`HOSTS`], three, waves; a wave (the placement at a tick, the start, the run of [`RUN`]
//! and the result) takes under 150 s, so the work ends well inside [`SETTLE`].

use std::time::Duration;

use js_sim_cell::{Expect, HowEnded, Scenario, ServerSpec, Shape, Work};

/// How many hosts.
pub const HOSTS: usize = 20;

/// How many whole-host jobs the client submits at time zero, and how long each one's run
/// lasts once its host is held.
pub const REQUESTS: u32 = 50;
pub const RUN: Duration = Duration::from_secs(60);

/// How long the cell runs after the jobs are submitted, before the end check.
pub const SETTLE: Duration = Duration::from_secs(1_200);

/// Proves the whole-host path end to end over many hosts: fifty jobs that each ask for a
/// whole host, on twenty hosts whose agents report at once. Thirty of them wait queued
/// while the first twenty hold their hosts, and each is placed on a host once it is quiet;
/// every agent is told to start its job, runs it and reports its result. The order in
/// which waiting jobs are served is not pinned (design section 8, row 11: advisory as
/// built). Under every invariant, and expecting that all fifty succeeded, were placed and
/// released, and that every host is serving at the end. Red for a server that places a
/// second job on a held host (`whole-host-is-exclusive`, `no-double-booking`), and for
/// one that drops a waiting job or leaves a host marked, held or restoring (the expects).
pub fn fifty_whole_host_requests() -> Scenario {
    let spec = ServerSpec::whole_node_pool("burst", Shape::cpu_host(), HOSTS);
    Scenario::new("fifty_whole_host_requests_over_twenty_hosts_are_each_held_once_and_released")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", HOSTS)
        .work(REQUESTS, Work::whole_host(RUN))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(
            REQUESTS as usize,
            HowEnded::Succeeded,
        ))
        .expect(Expect::held_and_released(REQUESTS as usize))
        .expect(Expect::every_host_serving())
        .settle_within(SETTLE)
}
