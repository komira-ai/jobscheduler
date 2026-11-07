//! The first whole-host scenario, shared by the whole-host family (`whole_host.rs`) and
//! the replay tests, which pin its trace hashes in the
//! golden file (family `whole_host`).
//!
//! Size: [`HOSTS`] agents beating every 5 s (the server's default) for [`SETTLE`], which
//! is how long a seed runs after the requests are made at time zero: `HOSTS * 1_200 / 5`
//! = 4,800 heartbeats a seed, with the hellos besides. The fifty requests go in
//! [`REQUESTS`] / [`HOSTS`], three, waves; a wave (the commit at a tick, the agent told
//! and reporting ready at its next two heartbeats, the lease held at the tick after, the
//! run of [`RUN`], the release and the two heartbeats of the restore) takes under 150 s,
//! so the work ends well inside [`SETTLE`].

use std::time::Duration;

use js_sim_cell::{Expect, HowEnded, Scenario, ServerSpec, Shape, Work};

/// How many hosts.
pub const HOSTS: usize = 20;

/// How many whole-host requests the client makes at time zero, and how long each one's
/// run lasts once its host is held.
pub const REQUESTS: u32 = 50;
pub const RUN: Duration = Duration::from_secs(60);

/// How long the cell runs after the requests are made, before the end check.
pub const SETTLE: Duration = Duration::from_secs(1_200);

/// The name of the scenario.
const NAME: &str = "fifty_whole_host_requests_over_twenty_hosts_are_each_held_once_and_released";

/// Proves the whole-host path end to end over many hosts (design section 5.4 as built; it
/// runs through the wait list of section 8, rows 11 and 12, and pins neither): fifty
/// whole-node requests, each under a request id of
/// its own, on twenty hosts whose agents prepare at once. Thirty of them wait on the
/// server's wait list while the first twenty hold their hosts, are presented again at
/// every tick, and are each committed on a host once it is quiet; every agent is told to
/// prepare, reports ready for exactly its lease, runs that lease's one run and restores.
/// The order in which waiting requests are served is not pinned (row 11: advisory as
/// built). Under every invariant, and expecting that all fifty succeeded, were held and
/// released, and that every host is serving at the end. Red for a server that books a
/// held host to a second lease (`whole-host-is-exclusive`, `no-double-booking`), grants
/// a lease before its host reported ready (`held-means-ready`), drops a waiting request
/// or leaves a host marked, held or restoring (the expects).
pub fn fifty_whole_host_requests() -> Scenario {
    let spec = ServerSpec::whole_node_pool("burst", Shape::gpu_host(), HOSTS);
    Scenario::new(NAME)
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
