//! The fault scenario of a server restarted with runs in flight, shared by the fault
//! family, which runs it, and the replay tests, which pin its trace hashes in the golden
//! file.
//!
//! Size: [`HOSTS`] agents beating every 5 s (the server's default) until [`RESTART_AT`]
//! plus [`SETTLE`], 960 s, make about 3,840 heartbeats a seed, with no hello after the
//! restart (the hosts' sessions are rows in the store).

use std::time::Duration;

use js_sim_cell::actions::restart_server;
use js_sim_cell::{Cell, Counter, Route, Scenario, ServerSpec, Shape, Work};

/// How many hosts, and so how many runs at once.
const HOSTS: usize = 20;

/// How many runs the client submits at time zero (two for each host), and how long each
/// lasts: longer than [`RESTART_AT`], so the first runs are in flight at the restart.
const RUNS: u32 = 40;
const RUN: Duration = Duration::from_secs(120);

/// When the server is stopped and started again over its files.
const RESTART_AT: Duration = Duration::from_secs(60);

/// How long the cell runs after the restart, before the end check.
const SETTLE: Duration = Duration::from_secs(900);

/// What a restart over the store must not cost an agent: a host's session is its row in the
/// store, so the restarted server serves the old session's beats. No heartbeat of any agent
/// was answered `unknown-host`, and no agent said hello more than the once it did at the
/// start.
fn sessions_survive_the_restart(cell: &mut Cell) -> Option<String> {
    let agents = cell.agent_count();
    let unknown: u64 = (0..agents)
        .filter_map(|index| cell.counters(index))
        .map(|counters| counters.get(Counter::UnknownHost))
        .sum();
    let hellos = cell
        .server()
        .map_or(0, |server| server.answered_total(Route::Hello));
    (unknown > 0 || hellos > agents as u64)
        .then(|| format!("{unknown} unknown-host answers and {hellos} hellos from {agents} agents"))
}

/// Proves that a restart of the server over the same files (`--boot resume`), with jobs
/// in flight on every host, double-books nothing (design section 8, row 3; section 6.7):
/// no job is launched twice, no host runs more than it holds once the hosts have said hello
/// again, no job is booked on two hosts or moves to another host, every item reaches one
/// terminal answer, and nothing is left booked once the clients are done. What becomes of
/// the jobs in flight is not stated: row 3 is Open, and every way it ends is terminal. The
/// statement shows the hosts keep their sessions: the restarted server serves their beats
/// from the host rows, so none is answered `unknown-host` and none says hello again. Red for a restarted server that books a job on another host than
/// the one it was on (`no-double-booking`), one that lets a host take a new job while it
/// still runs a job of before the restart (`no-double-booking`, `bookings-match-agents`),
/// and one that starts a job again (`at-most-one-launch`).
pub fn a_restart_of_the_server_with_runs_in_flight_double_books_nothing() -> Scenario {
    Scenario::new("a_restart_of_the_server_with_runs_in_flight_double_books_nothing")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool(
            "burst",
            Shape::gpu_host(),
            HOSTS,
        ))
        .hosts("burst", HOSTS)
        .work(RUNS, Work::run_for(RUN))
        .at(RESTART_AT, restart_server())
        .expect_that(
            "no host lost its session to the restart",
            sessions_survive_the_restart,
        )
        .settle_within(SETTLE)
}
