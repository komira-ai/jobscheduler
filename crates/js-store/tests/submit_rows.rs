//! The bytes of the `op/`, `queue/`, `inflight/`, `opkey/` and `opwall/` rows a frontend writes when it
//! queues operations, as a file (`tests/golden/submit-rows.txt`), and the bytes of the
//! `loss/` and `fate/` rows a start reads (`tests/golden/load-rows.txt`).
//!
//! The first file holds, for one store a frontend opened and queued two operations in (each
//! with a size key, the second also with a longest run), and one of which it then took out of
//! the queue, the rows of those five kinds: key, expiry and value as hex. Its `inflight/`,
//! `opkey/`, `opwall/` and `queue/` lines are those
//! of the farm server's golden file of scheduler rows, so a store written by either is read
//! by the other; the `op/` lines have that file's keys and the value of the test codec. The
//! second file holds rows copied from the same file, which a start reads back as the values
//! they were written from.

mod common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use js_sched::{Fate, Request};
use js_state::{Cond, Store, Txn};
use js_store::keys;
use js_store::test_support::{TestCodec, TestExt};
use js_store::{EpochPolicy, Rows, Submitted};
use js_types::sched::{
    FarmFault, FarmRun, LeaseKind, LostWhy, MemoryKill, MemoryRun, OperationId, Qos, Resources,
    WorkKey, WorkerId,
};

use common::Racing;

const SUBMITTED: &str = include_str!("golden/submit-rows.txt");
const LOADED: &str = include_str!("golden/load-rows.txt");

/// The term the frontend opened its store under.
const TERM: u64 = 0x0102_0304_0506_0708;

/// The rows of the golden file `text`, without its comments.
fn rows_of(text: &str) -> Vec<&str> {
    text.lines().filter(|line| !line.starts_with('#')).collect()
}

/// The rows of `store` under `prefixes`, a line each: key, expiry and value as hex.
fn dump(store: &Racing, prefixes: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
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
        out.push(format!("{key} expires_ns={expiry} value={value}"));
    }
    out
}

/// The request for the action whose digest is `tag` thirty-two times, of size `tag`: the
/// dedup text is the digest as `<hash>/<size>`, as the farm server's front names an action.
fn action(tag: u8) -> Request {
    let hash = format!("{tag:02x}").repeat(32);
    Request {
        key: WorkKey::new("main", format!("{hash}/{tag}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
    }
}

fn opened(store: &Arc<Racing>) -> Rows {
    let handle: Arc<dyn Store> = store.clone();
    let codec = Arc::new(TestCodec::new());
    let (rows, _) =
        Rows::open(handle, EpochPolicy::Keep, TERM, codec, Arc::new(TestExt)).expect("open");
    rows
}

/// Asserts that a frontend which queues operation 4 and operation 8, each with the size key
/// 7, and takes the second out of the queue (a grant's part of the rows), leaves exactly the
/// `op/`, `queue/`, `inflight/`, `opkey/` and `opwall/` rows of the golden file, byte for
/// byte: the in-flight rows holding the operation's id, the size key rows holding a version
/// byte and the key, the one wall time row (operation 8's, 3600.25 seconds) holding a version
/// byte, the seconds and the nanoseconds, and an empty queue row.
#[test]
fn the_op_queue_inflight_opkey_and_opwall_rows_are_the_rows_of_the_golden_file() {
    let store = Racing::new();
    let rows = opened(&store);
    for (id, tag, wall) in [
        (4, 1, None),
        (8, 9, Some(Duration::new(3_600, 250_000_000))),
    ] {
        let id = rows.reserve_operation(OperationId(id)).expect("an id");
        let queued = rows
            .submit_walled(id, &action(tag), &[], Some(7), wall)
            .expect("submit");
        assert!(matches!(queued, Submitted::Queued(queued) if queued == id));
    }
    let taken = Txn::new().delete(keys::queue(OperationId(8)), Cond::Any);
    store.txn(taken).expect("store").expect("the grant's part");

    let kinds = ["op/", "queue/", "inflight/", "opkey/", "opwall/"];
    assert_eq!(dump(&store, &kinds), rows_of(SUBMITTED));
}

/// The fate the golden file's `fate/` row holds, as the farm server wrote it from.
fn golden_fate() -> Fate {
    let run = |worker: &str, fault| FarmRun {
        worker: WorkerId::new(worker),
        fault,
    };
    Fate {
        booked_bytes: 16 << 30,
        memory_runs: vec![
            MemoryRun {
                worker: WorkerId::new("node-a"),
                booked: 4 << 30,
                kill: MemoryKill::OwnLimit { cap: 64 << 30 },
            },
            MemoryRun {
                worker: WorkerId::new("node-b"),
                booked: 16 << 30,
                kill: MemoryKill::NodePressure,
            },
        ],
        farm_runs: vec![
            run("node-a", FarmFault::NodeMemoryPressure),
            run("node-b", FarmFault::Lost(LostWhy::DaemonRestarted)),
            run("node-c", FarmFault::Lost(LostWhy::Fenced)),
            run("node-c", FarmFault::Infra),
        ],
        avoid: ["node-a", "node-b"]
            .map(WorkerId::new)
            .into_iter()
            .collect(),
        not_started: 3,
    }
}

/// Asserts that a start reads the rows of the second golden file back as the values the farm
/// server wrote them from: a loss count of two, and a version 2 fate with its memory runs,
/// farm runs, avoided workers and not-started count.
#[test]
fn the_loss_and_fate_rows_of_the_golden_file_load_as_the_values_they_were_written_from() {
    let store = Racing::new();
    let rows = opened(&store);
    let id = rows.reserve_operation(OperationId(4)).expect("an id");
    rows.submit(id, &action(1), &[], None).expect("submit");
    let mut put = Txn::new();
    for line in rows_of(LOADED) {
        let (key, value) = line
            .split_once(" expires_ns=- value=")
            .expect("a golden line");
        let bytes: Vec<u8> = (0..value.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&value[at..at + 2], 16).expect("hex"))
            .collect();
        put = put.put(key, bytes, Cond::Any);
    }
    store.txn(put).expect("store").expect("the rows");

    let stored = rows.load().expect("load");

    let [operation] = &stored.operations[..] else {
        panic!("one operation expected");
    };
    assert_eq!(operation.losses, 2);
    assert_eq!(operation.fate.as_ref(), Some(&golden_fate()));
}
