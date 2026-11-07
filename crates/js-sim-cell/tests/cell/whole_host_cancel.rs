//! The cancelled whole-host job of the whole-host fault family, shared by the family
//! (`whole_host_faults.rs`) and the replay tests, which pin its trace hashes in the golden
//! file (family `whole_host_faults`).
//!
//! Size: [`CANCEL_HOSTS`] agent beating every 5 s (the server's default) for
//! [`CANCEL_LAST`] plus [`CANCEL_SETTLE`], 900 s: 180 heartbeats a seed, with the hellos
//! besides.

use std::time::Duration;

use js_sim_cell::actions::{cancel_work, items, submit_work};
use js_sim_cell::{Expect, HowEnded, Scenario, ServerSpec, Shape, Work};

/// One host, so every whole-host job waits for the same one.
const CANCEL_HOSTS: usize = 1;

/// How long each held item's run lasts.
const CANCEL_RUN: Duration = Duration::from_secs(60);

/// When the two jobs that wait are submitted, while the first holds the host.
const CANCEL_WAIT_AT: Duration = Duration::from_secs(10);

/// When the first waiting job is cancelled (queued, its host held by another).
const CANCEL_QUEUED_AT: Duration = Duration::from_secs(30);

/// When the job that shows the host serving again is submitted, and how long the cell runs
/// after it.
const CANCEL_LAST: Duration = Duration::from_secs(300);
const CANCEL_SETTLE: Duration = Duration::from_secs(600);

/// Proves that a whole-host job cancelled while it is queued loses its place at once, and
/// the host serves again. One host: item 0 holds it for [`CANCEL_RUN`]; items 1 and 2 ask
/// for it at [`CANCEL_WAIT_AT`] and wait. Item 1 is cancelled at [`CANCEL_QUEUED_AT`],
/// queued behind the held host; item 2 is placed once item 0's host is quiet. Item 3,
/// submitted at [`CANCEL_LAST`], is placed on the host, runs and is released. Under every
/// invariant, and expecting the one cancel, the three runs, the host launching item 3's run
/// after [`CANCEL_LAST`] and serving at the end. Red for a cancel that leaves the job in the
/// queue (it would be placed: no cancel), a cancel that blocks the queue (items 2 and 3
/// would never be placed), and a cancel that cuts a run.
pub fn a_cancelled_request() -> Scenario {
    let spec = ServerSpec::whole_node_pool("burst", Shape::cpu_host(), CANCEL_HOSTS);
    let run = Work::whole_host(CANCEL_RUN);
    Scenario::new("a_cancelled_whole_host_request_drops_its_place_and_the_host_serves_again")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", CANCEL_HOSTS)
        .work(1, run.clone())
        .at(CANCEL_WAIT_AT, submit_work(2, run.clone()))
        .at(CANCEL_QUEUED_AT, cancel_work(items(1..2)))
        .at(CANCEL_LAST, submit_work(1, run))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(3, HowEnded::Succeeded))
        .expect(Expect::terminal_count(1, HowEnded::Cancelled))
        .expect(Expect::held_and_released(3))
        .expect(Expect::launched_on(&[0], CANCEL_LAST))
        .expect(Expect::every_host_serving())
        .settle_within(CANCEL_SETTLE)
}
