//! The cancelled whole-host request of the whole-host fault family, shared by the family
//! (`whole_host_faults.rs`), which runs it under every placement, and the replay tests,
//! which pin its trace hashes in the golden file (family `whole_host_faults`).
//!
//! Size: [`CANCEL_HOSTS`] agent beating every 5 s (the server's default) for
//! [`CANCEL_LAST`] plus [`CANCEL_SETTLE`], 900 s: 180 heartbeats a seed, with the hellos
//! besides.

use std::time::Duration;

use js_lease_server::Placement;
use js_sim_cell::actions::{cancel_when_set_aside, cancel_work, items, submit_work};
use js_sim_cell::{Expect, HowEnded, ItemId, Scenario, ServerSpec, Shape, Work};

/// One host, so every whole-host request waits for the same one.
const CANCEL_HOSTS: usize = 1;

/// How long each held item's run lasts.
const CANCEL_RUN: Duration = Duration::from_secs(60);

/// When the two requests that wait are made, while the first holds the host.
const CANCEL_WAIT_AT: Duration = Duration::from_secs(10);

/// When the first waiting request is cancelled (queued, its host held by another), and
/// the second starts to be watched for its mark, and for how long.
const CANCEL_QUEUED_AT: Duration = Duration::from_secs(30);
const CANCEL_WATCH: Duration = Duration::from_secs(600);

/// When the request that shows the host serving again is made, and how long the cell runs
/// after it.
const CANCEL_LAST: Duration = Duration::from_secs(300);
const CANCEL_SETTLE: Duration = Duration::from_secs(600);

/// Proves design section 8, row 39, queued and marked: a whole-host request cancelled
/// while queued, and one cancelled while the server sets a host aside for it, each lose
/// their place and their mark at once, and the host serves again. One host: item 0 holds
/// it for [`CANCEL_RUN`]; items 1 and 2 ask for it at [`CANCEL_WAIT_AT`] and wait. Item 1
/// is cancelled at [`CANCEL_QUEUED_AT`], queued behind the held host. Item 2 is
/// cancelled at the first read of the listing that shows the host set aside for it (a
/// mark, which the server makes once item 0 has released the host and it restores), by
/// `cancel_when_set_aside`. Item 3, asked for at [`CANCEL_LAST`], is held on the host,
/// runs and is released. Under every invariant (`drain-kills-nothing` reads the mark),
/// and expecting the two cancels, the two runs, the host launching item 3's run after
/// [`CANCEL_LAST`] and serving at the end, with no mark left. When the allocation of a
/// cancel while preparing is freed (row 39's second half: after the agent leaves
/// `preparing` in the design, at once as built) is not pinned: no item here is cancelled
/// while preparing. Red for a cancel that leaves the mark (the host would stay set aside,
/// `every-host-serving`, and item 3 would never be held), a cancel that is not acted on
/// while the request is marked (item 2 would be held: two cancels expected), and a
/// cancel that cuts a run.
pub fn a_cancelled_request(placement: Placement) -> Scenario {
    let spec = ServerSpec::whole_node_pool("burst", Shape::gpu_host(), CANCEL_HOSTS);
    let run = Work::whole_host(CANCEL_RUN);
    Scenario::new("a_cancelled_whole_host_request_drops_its_mark_and_the_host_serves_again")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .under(placement)
        .hosts("burst", CANCEL_HOSTS)
        .work(1, run.clone())
        .at(CANCEL_WAIT_AT, submit_work(2, run.clone()))
        .at(CANCEL_QUEUED_AT, cancel_work(items(1..2)))
        .at(
            CANCEL_QUEUED_AT,
            cancel_when_set_aside(ItemId(2), CANCEL_WATCH),
        )
        .at(CANCEL_LAST, submit_work(1, run))
        .expect(Expect::all_items_terminal())
        .expect(Expect::terminal_count(2, HowEnded::Succeeded))
        .expect(Expect::terminal_count(2, HowEnded::Cancelled))
        .expect(Expect::held_and_released(2))
        .expect(Expect::launched_on(&[0], CANCEL_LAST))
        .expect(Expect::every_host_serving())
        .settle_within(CANCEL_SETTLE)
}
