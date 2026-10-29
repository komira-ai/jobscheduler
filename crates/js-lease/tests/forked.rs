//! The lock beside the lease file is free again the moment the table is dropped,
//! whatever other threads of the process are doing.
//!
//! An `flock` belongs to the open file description, which lives until every copy of the
//! descriptor is closed, and a process forked while the lock is held (any other test of a
//! binary starting a program) keeps a copy until it execs. A lock released only by
//! closing its file is then still held after the table is dropped, and the next open of
//! the same file is refused. The child here is held before exec until the second open
//! has been tried, so the interleaving is the failing one every time. It is a binary of
//! its own, since a held child carries a copy of every descriptor the process has open.

use js_lease::{Boot, LeaseTable, ManualClock, Settings, Timestamp};
use js_provider_fake::FakeProvider;
use js_testkit::dir::TestDir;
use js_testkit::forked::ForkedChild;
use js_types::ProviderId;

fn open(dir: &TestDir) -> LeaseTable<FakeProvider, ManualClock> {
    let provider = FakeProvider::new(ProviderId::new("fake").unwrap(), Vec::new());
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let ttl = std::time::Duration::from_secs(60);
    let settings = Settings::new(ttl, 2 * ttl).unwrap();
    let file = dir.join("leases.toml");
    LeaseTable::open(provider, clock, settings, file, Boot::Initialise)
        .unwrap_or_else(|err| panic!("open: {err}"))
}

// Catches a lease table whose drop releases the file lock only by closing its
// descriptor: the restart of a server in a process that also spawns children (a test
// binary) is refused with "cannot lock: operation would block".
#[test]
fn a_child_forked_while_the_table_was_open_does_not_keep_the_file_locked_after_the_drop() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-lease-forked-drop");
    let first = open(&dir);
    let child = ForkedChild::before_exec();
    drop(first);
    let second = open(&dir);
    drop(second);
    child.finish();
}
