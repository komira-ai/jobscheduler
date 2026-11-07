//! The whole-node class over HTTP: a lease file that carries a reservation is resumed, and the
//! host the reservation names is listed as set aside. (`gpu-host` has not opted into this gate;
//! its tests are the other agent tests.)

use crate::agents_support::{POOL, agent_args, join};
use crate::support::{Harness, Scratch, T0, replace};

/// The pool file of a pool whose class is `whole-node`.
fn whole_pool() -> String {
    POOL.replace("gpu-host", "whole-node")
}

/// The flags of a server whose registered pool serves `whole-node`.
fn args(scratch: &Scratch) -> Vec<std::ffi::OsString> {
    let args = agent_args(scratch, false);
    scratch.write("agent-pool.toml", &whole_pool(), 0o644);
    args
}

/// Asserts the whole path of a resumed reservation over HTTP: a lease file whose ledger
/// holds a reservation of `r9` on `burst-1`, due thirty seconds after the boot's clock
/// reading, is resumed with `--boot resume`, and once the agent says hello the host
/// lists as reserved for `r9`, with its deadline raised to the boot plus
/// `RESERVATION_BOOT_GRACE` (not the stored deadline, which is earlier, and not the
/// stored plus the grace). The row carries an older build's give-up count of one, which
/// is read and ignored. It catches a server that opens the table and never gives the
/// ledger to the registry, and a restart grace that never reaches the table's settings.
#[tokio::test]
async fn a_resumed_lease_file_marks_the_host_with_the_deadline_floored() {
    let scratch = Scratch::new("whole_resume_rows");
    let args = replace(args(&scratch), "--boot", "resume");
    let text = format!(
        "version = 1\nterm = 3\nnext_sequence = 1\nlease_count = 0\nreservation_count = 1\n\n\
         [[reservation]]\nrequest_id = \"r9\"\nhost = \"burst-1\"\nreserved_at_ms = {T0}\n\
         deadline_ms = {}\nqueue_place = 0\nqueued_since_ms = {T0}\ngive_ups = 1\n",
        T0 + 30_000
    );
    std::fs::write(scratch.state(), text).unwrap();
    let h = Harness::with(scratch, args, None).await;
    join(&h, "burst-1").await;

    let listing = crate::agents_support::hosts(&h).await;
    let host = crate::agents_support::entry(&listing, "burst-1");
    assert_eq!(host["mark"], "reserved", "{host}");
    assert_eq!(host["reserved_for"], "r9", "{host}");
    let floor = T0 + js_lease_server::RESERVATION_BOOT_GRACE.as_secs() * 1000;
    assert_eq!(host["drain_deadline_ms"], floor, "{host}");
}
