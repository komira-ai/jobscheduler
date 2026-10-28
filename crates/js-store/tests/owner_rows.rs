//! The bytes of the `owner/`, `session/` and `cordon/` rows a frontend writes, as a file
//! (`tests/golden/owner-rows.txt`).
//!
//! The file holds, for one store a frontend opened and used (reserved ids, took a node by its
//! `Hello`, registered the node's daemon, and cordoned two nodes), the rows of those three
//! kinds: key, expiry and value as hex. The `owner/` and `cordon/` lines are those of the farm
//! server's golden file of scheduler rows, so a store written by either is read by the other;
//! the `session/` line has that file's key and expiry, and the value of the test codec.

use std::fmt::Write as _;
use std::sync::Arc;

use js_caps::NodeCaps;
use js_sched::DaemonInstance;
use js_state::fake::{Fake, Flaw};
use js_state::{ManualClock, Store, share};
use js_store::test_support::TestCodec;
use js_store::{EpochPolicy, Rows, StoredCordon, WorkerUp};
use js_types::sched::{FarmTime, OperationId, Resources, WorkerId};

const GOLDEN: &str = include_str!("golden/owner-rows.txt");

/// The term the frontend opened its store under.
const TERM: u64 = 0x0102_0304_0506_0708;

/// The rows of `store` under `prefixes`, a line each: key, expiry and value as hex.
fn dump(store: &Fake, prefixes: &[&str]) -> String {
    let mut out = String::new();
    for (key, row) in store.scan_prefix("").expect("scan") {
        if !prefixes.iter().any(|prefix| key.starts_with(prefix)) {
            continue;
        }
        let expiry = row.expires_at.map_or_else(
            || "-".to_owned(),
            |at| at.as_duration().as_nanos().to_string(),
        );
        let value: String = row.value.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
        let _ = writeln!(out, "{key} expires_ns={expiry} value={value}");
    }
    out
}

/// Asserts that a frontend which opens a shared store, reserves ids, takes node-a by a
/// `Hello`, registers its daemon and cordons node-b (a drain until 1234567 ms) and node-c,
/// leaves exactly the `owner/`, `session/` and `cordon/` rows of the golden file, byte for
/// byte: the owner (the term and counter 1), the registration expiring a lease grace after
/// the store's clock, the drain's tag and deadline, and the cordon's tag.
#[test]
fn the_owner_session_and_cordon_rows_are_the_rows_of_the_golden_file() {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    share(&*store).expect("the shared marker");
    let codec = Arc::new(TestCodec::new());
    let (rows, _) = Rows::open(store.clone(), EpochPolicy::Keep, TERM, codec).expect("open");
    rows.reserve_operation(OperationId(4))
        .expect("operation ids");
    rows.reserve_seqs(0).expect("lease sequences");
    let node = WorkerId::new("node-a");
    rows.arrive(&node, FarmTime::from_millis(0))
        .expect("a Hello");
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    let up = WorkerUp {
        worker: node,
        instance: DaemonInstance::new("daemon-1"),
        predecessor_stopped: true,
        capacity: Resources::new(2_000, 1 << 30),
        caps,
    };
    rows.session(&up).expect("a session");
    let drain = StoredCordon::Draining {
        deadline_unix_ms: 1_234_567,
    };
    rows.cordoned(&WorkerId::new("node-b"), Some(drain))
        .expect("a drain");
    rows.cordoned(&WorkerId::new("node-c"), Some(StoredCordon::Cordoned))
        .expect("a cordon");

    let golden: String = GOLDEN
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(dump(&store, &["owner/", "session/", "cordon/"]), golden);
}
