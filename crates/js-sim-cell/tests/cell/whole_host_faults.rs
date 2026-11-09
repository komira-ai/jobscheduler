//! The scenarios of the whole-host fault family (`whole_host_faults.rs`) but the cancelled
//! job, which the replay tests also run (`whole_host_cancel.rs`).
//!
//! Sizes, in agent requests a seed (heartbeats every 5 s, the server's default, with the
//! hellos besides), each far under the 30,000 a seed of the cell's sizing rule:
//! - the drain: [`DRAIN_HOSTS`] for [`DRAIN_ASK_AT`] plus [`DRAIN_SETTLE`], 960 s, make
//!   768;
//! - the restart of a held host's agent: [`HELD_HOSTS`] for [`HELD_RESTART_AT`] plus
//!   [`HELD_SETTLE`], 960 s, make 384.

use std::time::Duration;

use js_sim_cell::actions::{agents, replace_agents, submit_work};
use js_sim_cell::{Expect, HowEnded, ItemId, Scenario, ServerSpec, Shape, Work};

/// A whole-node pool of `hosts` hosts.
fn pool(hosts: usize) -> ServerSpec {
    ServerSpec::whole_node_pool("burst", Shape::cpu_host(), hosts)
}

/// The drain: four hosts held by four jobs' long runs, and a fifth job.
const DRAIN_HOSTS: usize = 4;
const DRAIN_BUSY: Duration = Duration::from_secs(300);
const DRAIN_RUN: Duration = Duration::from_secs(60);
const DRAIN_ASK_AT: Duration = Duration::from_secs(60);
const DRAIN_SETTLE: Duration = Duration::from_secs(900);

/// Proves design section 6.4, "drain never cuts work": a whole-host job that finds every
/// host busy waits for a host's work to end, and nothing on a host is stopped for it. Four
/// hosts, each held by a job whose run lasts [`DRAIN_BUSY`]; at [`DRAIN_ASK_AT`] a fifth
/// job asks for a whole host. It waits and is placed on a host only after every run
/// launched there before its placement has completed. Every run succeeds: none is cut. Red
/// for a server that stops or cancels a run to make room (the run would not succeed;
/// `drain-kills-nothing` while the host is set aside), places the job before its host's run
/// ended (`held-after-the-runs-on-its-host`, `whole-host-is-exclusive`), or drops the
/// waiting job.
pub fn a_drain() -> Scenario {
    let requests = DRAIN_HOSTS as u32;
    let waiting = ItemId(requests);
    Scenario::new("drain_waits_for_a_busy_host_and_cuts_no_run")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(DRAIN_HOSTS))
        .hosts("burst", DRAIN_HOSTS)
        .work(requests, Work::whole_host(DRAIN_BUSY))
        .at(DRAIN_ASK_AT, submit_work(1, Work::whole_host(DRAIN_RUN)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(DRAIN_HOSTS + 1, HowEnded::Succeeded))
        .expect(Expect::held_and_released(DRAIN_HOSTS + 1))
        .expect(Expect::held_after_the_runs_on_its_host(waiting))
        .expect(Expect::every_host_serving())
        .settle_within(DRAIN_SETTLE)
}

/// The restart: two hosts held by two jobs' long runs, a third job waiting, and the first
/// host's agent started again while its job runs.
const HELD_HOSTS: usize = 2;
const HELD_BUSY: Duration = Duration::from_secs(300);
const HELD_RUN: Duration = Duration::from_secs(60);
const HELD_ASK_AT: Duration = Duration::from_secs(10);
const HELD_RESTART_AT: Duration = Duration::from_secs(60);
const HELD_SETTLE: Duration = Duration::from_secs(900);

/// Proves that a placed whole-host job whose host's agent is started again with empty state
/// does not leave the host booked to two jobs. Two hosts held by two jobs whose runs last
/// [`HELD_BUSY`]; a third job waits from [`HELD_ASK_AT`]; at [`HELD_RESTART_AT`] the first
/// host's agent is started again (`replace_agents`), says hello once the server holds the
/// host lost, holding no run. The hello ends the first job's run (the item ends lost or
/// failed, which is not pinned); the waiting job is then placed on the restarted host and
/// launches its run there (`launched-on`), and the other host's run is not touched (the
/// other job succeeds). Red for a host booked to the waiting job while the restarted
/// host's first job still holds it (`whole-host-is-exclusive`, `no-double-booking`), a
/// booking left behind, or the other host's run cut.
pub fn held_job_restart() -> Scenario {
    let requests = HELD_HOSTS as u32;
    Scenario::new("a_whole_host_job_whose_agent_restarts_after_it_is_placed")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(pool(HELD_HOSTS))
        .hosts("burst", HELD_HOSTS)
        .work(requests, Work::whole_host(HELD_BUSY))
        .at(HELD_ASK_AT, submit_work(1, Work::whole_host(HELD_RUN)))
        .at(HELD_RESTART_AT, replace_agents(agents(0..1)))
        .expect(Expect::all_items_terminal())
        .expect(Expect::launched_on(&[0], HELD_RESTART_AT))
        .expect(Expect::every_host_serving())
        .settle_within(HELD_SETTLE)
}
