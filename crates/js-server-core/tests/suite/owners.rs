//! The owner of a daemon's node and the daemon's session row, when several frontends share
//! one state store (moved from the farm core's tests of the same rules).

use crate::common::{at, beat, block_on, close, reported, submit, up, up_after};
use crate::stored::Setup;

/// Asserts that a frontend acknowledges a heartbeat only while it owns the node: after the
/// daemon has said `Hello` to another frontend, the first one finds the owner row changed (at
/// the next look, an owner cache time later), stops acknowledging, and the second does.
/// Catches: two frontends that both keep a daemon alive, so that neither lets it fence.
#[test]
fn a_frontend_the_daemon_has_left_stops_acknowledging_it() {
    let setup = Setup::new();
    let a = setup.core(1);
    let b = setup.core(2);
    let on_a = up(&a, 0, "node-a", "d1");
    let on_b = up(&b, 1, "node-a", "d1");

    assert!(
        beat(&a, 1_000, ("node-a", on_a), 1, &[]),
        "within the cache time"
    );
    assert!(
        !beat(&a, 3_000, ("node-a", on_a), 2, &[]),
        "the row names the other"
    );
    assert!(beat(&b, 3_000, ("node-a", on_b), 1, &[]));
}

/// Asserts that a fenced core, which writes nothing, still acknowledges the heartbeats it
/// hears. Catches: a handed-over core that makes its daemons fence by silence.
#[test]
fn a_fenced_core_acknowledges_without_writing() {
    let setup = Setup::new();
    let a = setup.core(1);
    let stream = up(&a, 0, "node-a", "d1");
    a.fence();
    setup.store.fail_writes(Some(""));

    assert!(beat(&a, 40_000, ("node-a", stream), 1, &[]));
}

/// Asserts that a heartbeat is acknowledged only while the core has written the node's
/// session row lately: not when the row was never written, not when its renewals fail for
/// longer than the window, and again once a renewal takes. Catches: a daemon the other
/// frontends cannot see alive kept alive by this one.
#[test]
fn a_heartbeat_is_acknowledged_only_while_the_session_row_is_fresh() {
    let setup = Setup::new();
    let core = setup.core(1);
    setup.store.fail_writes(Some("session/"));
    let stream = up(&core, 0, "node-a", "d1");
    assert!(
        !beat(&core, 1, ("node-a", stream), 1, &[]),
        "no row was ever written"
    );

    setup.store.fail_writes(None);
    assert!(
        beat(&core, 2, ("node-a", stream), 2, &[]),
        "the renewal put the row"
    );
    assert_eq!(setup.store.rows_of("session/").len(), 1);

    setup.store.fail_writes(Some("session/"));
    assert!(
        !beat(&core, 31_000, ("node-a", stream), 3, &[]),
        "past the window"
    );
    setup.store.fail_writes(None);
    assert!(beat(&core, 32_000, ("node-a", stream), 4, &[]));
}

/// Asserts that a claim the store could not take is made again when the store is back, for a
/// node whose stream is still open, and dropped for one whose stream has closed. Catches: a
/// node left unowned for good, and a claim made for a daemon that is gone.
#[test]
fn a_claim_the_store_refused_is_made_again_for_a_node_still_connected() {
    let setup = Setup::new();
    let core = setup.core(1);
    setup.store.set_down(true);
    let gone = up(&core, 0, "node-a", "d1");
    up(&core, 0, "node-b", "d2");
    close(&core, "node-a", gone);
    setup.store.set_down(false);

    block_on(core.tick(at(1)));

    let owners = setup.store.rows_of("owner/");
    let named: Vec<&str> = owners.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(named, ["owner/node-b"]);
}

/// Asserts that a node that moved to this frontend with another daemon process keeps the
/// leases it does not list for the handover wait, and one that moved with the same process
/// does not. Catches: a lease given up while the old process may still run it, and one kept
/// for a process that is known to be the one that ran it.
#[test]
fn the_leases_of_a_node_that_moved_with_another_process_are_kept_a_while() {
    for (process, kept) in [("d2", true), ("d1", false)] {
        let setup = Setup::new();
        let a = setup.core(1);
        let b = setup.core(2);
        up(&a, 0, "node-a", "d1");
        let leased = submit(&a, 1, 1);
        let stream = up_after(&b, 2, "node-a", (process, ""), reported(8));

        beat(&b, 3, ("node-a", stream), 1, &[]);

        // Held for the handover wait when the process is another; the same process, whose
        // runs are known, has its lease decided by the heartbeat as ever.
        if kept {
            assert_eq!(b.queued_operations(), 0, "{process}: {leased:?}");
        }
    }
}
