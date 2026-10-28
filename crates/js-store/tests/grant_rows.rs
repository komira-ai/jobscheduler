//! The bytes of the `lease/`, `lease-by-op/` and `done/` rows a frontend writes when it grants
//! and ends operations, as a file (`tests/golden/grant-rows.txt`), and the order of the
//! operations in the transaction of each transition.
//!
//! The file holds, for one store a frontend opened and used (the calls the farm server's own
//! golden file of scheduler rows is written by, up to the operations that end), the rows of
//! those three kinds: key, expiry and value as hex. Its `lease-by-op/` and `done/` lines
//! are those of the farm server's golden file, copied unchanged: the value of a `done/` row is
//! the farm server's, which this crate keeps as it is. The `lease/` line has that file's key
//! and expiry, and the value of the test codec.
//!
//! The order of the operations in a transaction is the order the farm server writes them in
//! (its trace of the store, `check`, `del` and `put` for each, with the condition and the
//! lifetime), including where the embedder's rows are put and removed: the order is part of
//! what a store written by either server holds to.

mod common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use js_caps::NodeCaps;
use js_sched::{DaemonInstance, Request};
use js_state::fake::{Fake, Flaw};
use js_state::{Cond, ManualClock, Op, Store, Txn, share};
use js_store::test_support::{TestCodec, TestExt};
use js_store::{Ending, EpochPolicy, Rows, StoredCordon, WorkerUp};
use js_types::sched::{
    FarmTime, LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkKey, WorkerId,
};

use common::{Racing, frontend_with};

const GOLDEN: &str = include_str!("golden/grant-rows.txt");

/// The term the frontend opened its store under.
const TERM: u64 = 0x0102_0304_0506_0708;

/// The `done/` values of the farm server's golden file: a failure, a run and a time-out, as
/// hex.
const RESULTS: [&str; 3] = [
    "011a0908031205656e646564",
    "000a052a036f75741a00",
    "020a053a036375741a0d0804120974696d6564206f7574",
];

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex digits"))
        .collect()
}

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
    }
}

fn grant_of(operation: u64, seq: u64) -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new("node-a"),
    }
}

/// Asserts that a frontend which grants and ends operations as the farm server's golden file
/// does leaves exactly the `lease/`, `lease-by-op/` and `done/` rows of the golden file, byte
/// for byte: operations 5 to 7 ended each in its way (the value is kept as given, for the
/// finished retention and at least the floor, so every expiry is 600 seconds after the
/// store's clock), and operation 8 holding lease 4 of term 5, whose index row is the term
/// and the sequence, eight bytes each.
#[test]
fn the_lease_index_and_done_rows_are_the_rows_of_the_golden_file() {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    share(&*store).expect("the shared marker");
    let codec = Arc::new(TestCodec::new());
    let (rows, _) = Rows::open(
        store.clone(),
        EpochPolicy::Keep,
        TERM,
        codec,
        Arc::new(TestExt),
    )
    .expect("open");
    rows.reserve_operation(OperationId(4))
        .expect("operation ids");
    rows.reserve_seqs(0).expect("lease sequences");
    let node = WorkerId::new("node-a");
    rows.arrive(&node, FarmTime::from_millis(0))
        .expect("a Hello");
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    let up = WorkerUp {
        worker: node.clone(),
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

    // Operation 4 was granted, then its lease given up (a stand-in for the give-up, which
    // belongs to a later row of this crate): one transaction, as the give-up is.
    rows.submit(OperationId(4), &action(1), &[], Some(7))
        .expect("submit");
    rows.granted(&grant_of(4, 0)).expect("grant");
    let given_up = Txn::new()
        .delete(js_store::keys::lease(LeaseId::new(5, 0)), Cond::Any)
        .delete(js_store::keys::lease_by_op(OperationId(4)), Cond::Any)
        .put(
            js_store::keys::queue(OperationId(4)),
            Vec::new(),
            Cond::Absent,
        );
    store.txn(given_up).expect("store").expect("given up");

    // Operations 5 to 7 ended, one in each way a result is kept.
    for (i, result) in (0..).zip(RESULTS) {
        let (operation, seq, tag) = (5 + i, 1 + i, 2 + u8::try_from(i).expect("small"));
        let submitted = rows.submit(OperationId(operation), &action(tag), &[], None);
        submitted.expect("submit");
        rows.granted(&grant_of(operation, seq)).expect("grant");
        let ending = Ending::new(unhex(result), Duration::from_secs(1));
        let lease = LeaseId::new(5, seq);
        rows.finished(OperationId(operation), lease, &ending)
            .expect("a result");
    }

    // Operation 8 holds its lease.
    rows.submit(OperationId(8), &action(9), &[], Some(7))
        .expect("submit");
    rows.granted(&grant_of(8, 4)).expect("grant");

    let golden: String = GOLDEN
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(dump(&store, &["lease/", "lease-by-op/", "done/"]), golden);
}

/// The transaction `txn` as the farm server's trace of the store writes it, a line for each
/// operation: the kind, the key, the condition (`any`, `absent` or `version`, whichever
/// version) and, for a put, the lifetime.
fn shape(txn: &Txn) -> Vec<String> {
    let cond = |cond: Cond| match cond {
        Cond::Any => "any",
        Cond::Absent => "absent",
        Cond::Version(_) => "version",
    };
    txn.ops()
        .iter()
        .map(|op| match op {
            Op::Put {
                key,
                ttl,
                cond: held,
                ..
            } => {
                let ttl = ttl.map_or_else(|| "-".to_owned(), |t| format!("{}s", t.as_secs()));
                format!("put {key} cond={} ttl={ttl}", cond(*held))
            }
            Op::Delete { key, cond: held } => format!("del {key} cond={}", cond(*held)),
            Op::Check { key, cond: held } => format!("check {key} cond={}", cond(*held)),
            Op::Next { counter } => format!("next {counter}"),
        })
        .collect()
}

/// The only transaction `store` was asked for since the last look, as [`shape`] shows it.
fn only(store: &Racing) -> Vec<String> {
    let asked = store.asked();
    let [txn] = &asked[..] else {
        panic!("one transaction expected, found {}", asked.len());
    };
    shape(txn)
}

/// A frontend over a fresh store, which owns node-a, with operation 4 submitted (of the work
/// `action(4)`, with the parcel `[7]` and a size key). The transactions so far are not
/// counted.
fn owner_of_node_a() -> (Arc<Racing>, Rows) {
    let store = Racing::new();
    let rows = frontend_with(&store, 1, &Arc::new(TestCodec::new()));
    rows.arrive(&WorkerId::new("node-a"), FarmTime::from_millis(0))
        .expect("a Hello");
    let submitted = rows.submit(OperationId(4), &action(4), &[7], Some(3));
    submitted.expect("submit");
    store.asked();
    (store, rows)
}

fn ending() -> Ending {
    Ending::new(b"done".to_vec(), Duration::from_secs(1)).with_parcel(b"main".to_vec())
}

const HASH_4: &str = "0404040404040404040404040404040404040404040404040404040404040404";

/// The rows of operation 4 that ending it removes, in the order the farm server removes them:
/// `loss/`, `fate/`, the embedder's rows, `opkey/` and `lease-by-op/`, and the result last.
fn removed(done: &str) -> Vec<String> {
    [
        "del loss/0000000000000004 cond=any",
        "del fate/0000000000000004 cond=any",
        "del parcel/0000000000000004 cond=any",
        "del opkey/0000000000000004 cond=any",
        "del lease-by-op/0000000000000004 cond=any",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([done.to_owned()])
    .collect()
}

/// Asserts the order of the operations of a grant: the check of the node's owner first, then
/// the queue row's delete at the version read, the lease row put where there is none, and
/// the index row put over whatever is there.
#[test]
fn a_grant_writes_its_operations_in_this_order() {
    let (store, rows) = owner_of_node_a();

    rows.granted(&grant_of(4, 0)).expect("grant");

    assert_eq!(
        only(&store),
        [
            "check owner/node-a cond=version",
            "del queue/0000000000000004 cond=version",
            "put lease/0000000000000005.0000000000000000 cond=absent ttl=-",
            "put lease-by-op/0000000000000004 cond=any ttl=-",
        ]
    );
}

/// Asserts that a grant for a node this frontend never claimed has no owner check, and is
/// otherwise the same.
#[test]
fn a_grant_for_a_node_never_claimed_has_no_owner_check() {
    let (store, rows) = owner_of_node_a();
    let elsewhere = LeaseGrant {
        worker: WorkerId::new("node-z"),
        ..grant_of(4, 0)
    };

    rows.granted(&elsewhere).expect("grant");

    let written = only(&store);
    assert_eq!(written[0], "del queue/0000000000000004 cond=version");
    assert_eq!(written.len(), 3);
}

/// Asserts the order of the operations of a result under a lease: the owner check, the lease
/// row's delete at the version read, the in-flight row's delete at the version read, the
/// embedder's row for the result (kept as long as the result), then the operation's rows, the
/// embedder's rows among them between `fate/` and `opkey/`, and the result put over whatever is
/// there.
#[test]
fn a_result_under_a_lease_writes_its_operations_in_this_order() {
    let (store, rows) = owner_of_node_a();
    rows.granted(&grant_of(4, 0)).expect("grant");
    store.asked();

    rows.finished(OperationId(4), LeaseId::new(5, 0), &ending())
        .expect("a result");

    let mut want = vec![
        "check owner/node-a cond=version".to_owned(),
        "del lease/0000000000000005.0000000000000000 cond=version".to_owned(),
        format!("del inflight/6d61696e/{HASH_4}/4 cond=version"),
        "put ended-parcel/0000000000000004 cond=any ttl=600s".to_owned(),
        "del op/0000000000000004 cond=any".to_owned(),
        "del queue/0000000000000004 cond=any".to_owned(),
    ];
    want.extend(removed("put done/0000000000000004 cond=any ttl=600s"));
    assert_eq!(only(&store), want);
}

/// Asserts the order of the operations of a result for a lease that was given up in the same
/// feed, and of a refusal: the queue and operation rows deleted at the versions read, the
/// in-flight row, the embedder's row for the result, the rest of the rows, and the result put
/// only where there is none.
#[test]
fn a_result_for_a_given_up_lease_and_a_refusal_write_their_operations_in_this_order() {
    let mut want = vec![
        "del queue/0000000000000004 cond=version".to_owned(),
        "del op/0000000000000004 cond=version".to_owned(),
        format!("del inflight/6d61696e/{HASH_4}/4 cond=version"),
        "put ended-parcel/0000000000000004 cond=any ttl=600s".to_owned(),
    ];
    want.extend(removed("put done/0000000000000004 cond=absent ttl=600s"));

    let (store, rows) = owner_of_node_a();
    rows.refused(OperationId(4), &ending()).expect("refused");
    assert_eq!(only(&store), want);

    let (store, rows) = owner_of_node_a();
    rows.granted(&grant_of(4, 0)).expect("grant");
    let queue = js_store::keys::queue(OperationId(4));
    let given_up = Txn::new()
        .delete(js_store::keys::lease(LeaseId::new(5, 0)), Cond::Any)
        .put(queue, Vec::new(), Cond::Absent);
    store.txn(given_up).expect("store").expect("given up");
    store.asked();
    rows.finished(OperationId(4), LeaseId::new(5, 0), &ending())
        .expect("a result");
    assert_eq!(only(&store), want);
}

/// Asserts that an ending that carries no parcel puts no row for it, and that an operation
/// with no `op/` row (so no in-flight row to take, and no work the parcel is about) puts none
/// either: the rest of the transaction is as it was.
#[test]
fn the_embedders_row_for_a_result_is_put_only_for_a_parcel_and_a_known_operation() {
    let (store, rows) = owner_of_node_a();
    let bare = Ending::new(b"done".to_vec(), Duration::from_secs(1));
    rows.refused(OperationId(4), &bare).expect("refused");
    let written = only(&store);
    assert!(written.iter().all(|line| !line.contains("ended-parcel")));
    assert_eq!(written.len(), 9);

    let store = Racing::new();
    let rows = frontend_with(&store, 1, &Arc::new(TestCodec::new()));
    let queued = Txn::new().put("queue/0000000000000004", Vec::new(), Cond::Absent);
    store.txn(queued).expect("store").expect("queued");
    rows.granted(&grant_of(4, 0)).expect("grant");
    store.asked();

    rows.finished(OperationId(4), LeaseId::new(5, 0), &ending())
        .expect("a result");

    let mut want = vec![
        "del lease/0000000000000005.0000000000000000 cond=version".to_owned(),
        "del op/0000000000000004 cond=any".to_owned(),
        "del queue/0000000000000004 cond=any".to_owned(),
    ];
    want.extend(removed("put done/0000000000000004 cond=any ttl=600s"));
    assert_eq!(only(&store), want);
}
