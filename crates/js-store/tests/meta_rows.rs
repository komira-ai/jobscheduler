//! The bytes of the `meta/` rows a frontend writes, as a file
//! (`tests/golden/meta-rows.txt`).
//!
//! The file holds, for one store a frontend opened and reserved its first blocks in, every
//! row: its key, when the store expires it and its value as hex. They are the lines the farm
//! server's golden file of scheduler rows holds for the same calls, so a store written by
//! either is read by the other. This test requires the rows the calls write to be the rows of
//! the file, byte for byte, and no other row to be written.

use std::fmt::Write as _;
use std::sync::Arc;

use js_state::fake::{Fake, Flaw};
use js_state::{ManualClock, Store, share};
use js_store::test_support::{TestCodec, TestExt};
use js_store::{EpochPolicy, Rows};
use js_types::sched::OperationId;

const GOLDEN: &str = include_str!("golden/meta-rows.txt");

/// The term the first frontend opened its store under.
const TERM: u64 = 0x0102_0304_0506_0708;

/// Every row of `store`, a line each: key, expiry and value as hex.
fn dump(store: &Fake) -> String {
    let mut out = String::new();
    for (key, row) in store.scan_prefix("").expect("scan") {
        let expiry = row
            .expires_at
            .map_or_else(|| "-".to_owned(), |at| format!("{at:?}"));
        let value: String = row.value.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
        let _ = writeln!(out, "{key} expires_ns={expiry} value={value}");
    }
    out
}

/// Asserts that a frontend that opens a shared, empty store under a term, reserves operation
/// ids from 4 and lease sequences from 0, leaves exactly the four rows of the golden file:
/// the epoch (the term), the shared-writers marker, and the counters at the end of each
/// block (4 and 0, 1024 past).
#[test]
fn the_meta_rows_are_the_rows_of_the_golden_file() {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    share(&*store).expect("the shared marker");
    let codec = Arc::new(TestCodec::new());
    let ext = Arc::new(TestExt);
    let (rows, epoch) =
        Rows::open(store.clone(), EpochPolicy::Keep, TERM, codec, ext).expect("open");
    assert_eq!(epoch, TERM);
    rows.reserve_operation(OperationId(4))
        .expect("operation ids");
    rows.reserve_seqs(0).expect("lease sequences");

    let golden: String = GOLDEN
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(dump(&store), golden);
}
