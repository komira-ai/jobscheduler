//! The keys of the scheduler's rows, pinned as a file (`tests/golden/keys.txt`), and the
//! readers that take an id or a node name back out of a key.

use js_store::{StateError, keys};
use js_types::sched::{LeaseId, OperationId, WorkKey, WorkerId};

const GOLDEN: &str = include_str!("golden/keys.txt");

/// The digest text of the action whose hash is `tag` repeated, of size `size`.
fn digest(tag: u8, size: u64) -> String {
    format!("{}/{size}", format!("{tag:02x}").repeat(32))
}

fn node(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// Every line of the golden file, as the key functions build it, in the file's order.
fn built() -> Vec<String> {
    let op = |n: u64| OperationId(n);
    let main = |tag: u8, size: u64| WorkKey::new("main", digest(tag, size));
    let odd = |class: &str| WorkKey::new(class, "x/1");
    let prefixes = [
        ("OPERATION", keys::OPERATION),
        ("QUEUE", keys::QUEUE),
        ("LEASE", keys::LEASE),
        ("LEASE_BY_OP", keys::LEASE_BY_OP),
        ("SESSION", keys::SESSION),
        ("OWNER", keys::OWNER),
        ("CORDON", keys::CORDON),
        ("DONE", keys::DONE),
        ("LOSS", keys::LOSS),
        ("FATE", keys::FATE),
        ("FLOOR", keys::FLOOR),
        ("KEY_FLOOR", keys::KEY_FLOOR),
        ("OPKEY", keys::OPKEY),
        ("OPWALL", keys::OPWALL),
        ("INFLIGHT", keys::INFLIGHT),
        ("EPOCH", keys::EPOCH),
        ("ALIVE", keys::ALIVE),
        ("NEXT_OP", keys::NEXT_OP),
        ("NEXT_SEQ", keys::NEXT_SEQ),
    ]
    .map(|(name, prefix)| format!("prefix {name} {prefix}"));
    let keyed = [
        // The farm server's golden file.
        ("operation", keys::operation(op(4))),
        ("operation", keys::operation(op(8))),
        ("queue", keys::queue(op(4))),
        ("lease", keys::lease(LeaseId::new(5, 4))),
        ("lease_by_op", keys::lease_by_op(op(8))),
        ("session", keys::session(&node("node-a"))),
        ("owner", keys::owner(&node("node-a"))),
        ("cordon", keys::cordon(&node("node-b"))),
        ("cordon", keys::cordon(&node("node-c"))),
        ("done", keys::done(op(5))),
        ("done", keys::done(op(6))),
        ("done", keys::done(op(7))),
        ("loss", keys::loss(op(4))),
        ("fate", keys::fate(op(4))),
        ("opkey", keys::opkey(op(4))),
        ("opkey", keys::opkey(op(8))),
        ("opwall", keys::opwall(op(8))),
        ("floor", keys::floor(&main(1, 1))),
        ("key_floor", keys::key_floor(7)),
        ("inflight", keys::inflight(&main(1, 1))),
        ("inflight", keys::inflight(&main(9, 9))),
        // What that file leaves out.
        ("operation", keys::operation(op(0))),
        ("operation", keys::operation(op(u64::MAX))),
        ("lease", keys::lease(LeaseId::new(u64::MAX, 1))),
        ("key_floor", keys::key_floor(u64::MAX)),
        ("session", keys::session(&node("a/b"))),
        ("floor", keys::floor(&odd("a/b"))),
        ("floor", keys::floor(&odd("\u{e9}"))),
        ("floor", keys::floor(&odd(""))),
        ("floor", keys::floor(&odd("\u{1}\u{f}"))),
        ("inflight", keys::inflight(&odd("a/b"))),
    ]
    .map(|(name, key)| format!("{name} {key}"));
    prefixes.into_iter().chain(keyed).collect()
}

/// The lines of the golden file that are not comments.
fn golden() -> Vec<&'static str> {
    GOLDEN
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect()
}

/// Asserts that the key functions and prefix constants give, for the inputs of the farm
/// server's own golden file of scheduler rows and for the extremes it leaves out, exactly
/// the keys in `tests/golden/keys.txt`: a change to the layout of a key, to the width of an
/// id or to how a class is spelled fails here, before a store written by the earlier
/// release stops being found.
#[test]
fn the_keys_are_the_keys_of_the_golden_file() {
    let built = built();
    let golden = golden();
    assert_eq!(built.len(), golden.len(), "{built:#?}");
    for (built, golden) in built.iter().zip(&golden) {
        assert_eq!(built, golden);
    }
}

/// Asserts that no two rows of different kinds share a key: the prefix of every kind of row
/// ends in `/` (the meta rows are whole keys) and no prefix starts with another, so a scan of
/// one kind never returns the rows of another.
#[test]
fn no_kind_of_row_is_a_prefix_of_another() {
    let prefixes = [
        keys::OPERATION,
        keys::QUEUE,
        keys::LEASE,
        keys::LEASE_BY_OP,
        keys::SESSION,
        keys::OWNER,
        keys::CORDON,
        keys::DONE,
        keys::LOSS,
        keys::FATE,
        keys::FLOOR,
        keys::KEY_FLOOR,
        keys::OPKEY,
        keys::OPWALL,
        keys::INFLIGHT,
    ];
    for (i, prefix) in prefixes.iter().enumerate() {
        assert!(prefix.ends_with('/'), "{prefix}");
        for other in &prefixes[i + 1..] {
            assert!(!prefix.starts_with(other) && !other.starts_with(prefix));
        }
    }
}

/// Asserts that the id in a key is read back: from every key of an operation's row, for the
/// smallest and the largest id.
#[test]
fn the_operation_a_key_names_is_read_back() {
    for id in [0, 4, 0x0123_4567_89ab_cdef, u64::MAX] {
        let operation = OperationId(id);
        let rows = [
            (keys::OPERATION, keys::operation(operation)),
            (keys::QUEUE, keys::queue(operation)),
            (keys::LOSS, keys::loss(operation)),
            (keys::FATE, keys::fate(operation)),
            (keys::OPKEY, keys::opkey(operation)),
            (keys::OPWALL, keys::opwall(operation)),
            (keys::DONE, keys::done(operation)),
            (keys::LEASE_BY_OP, keys::lease_by_op(operation)),
        ];
        for (prefix, key) in rows {
            assert_eq!(keys::operation_of(prefix, &key).expect("an id"), operation);
        }
    }
}

/// Asserts that a key that does not name an operation under the prefix is corrupt, whether it
/// is under another prefix, has no digits, has a digit that is not hexadecimal or has more
/// than an id holds.
#[test]
fn a_key_that_does_not_name_an_operation_is_corrupt() {
    let bad = [
        ("op/", "queue/0000000000000004"),
        ("op/", "op/"),
        ("op/", "op/zz"),
        ("op/", "op/10000000000000000"),
        ("op/", "o"),
    ];
    for (prefix, key) in bad {
        let refused = keys::operation_of(prefix, key);
        assert!(
            matches!(&refused, Err(StateError::Corrupt { key: named, .. }) if named == key),
            "{key}: {refused:?}"
        );
    }
}

/// Asserts that the node a key names is read back as it was written, including a name with a
/// `/` in it, and that a key under another prefix names none.
#[test]
fn the_node_a_key_names_is_read_back() {
    for name in ["node-a", "a/b", ""] {
        let node = node(name);
        assert_eq!(
            keys::worker_of(keys::SESSION, &keys::session(&node)),
            Some(node.clone())
        );
        assert_eq!(
            keys::worker_of(keys::OWNER, &keys::owner(&node)),
            Some(node.clone())
        );
        assert_eq!(
            keys::worker_of(keys::CORDON, &keys::cordon(&node)),
            Some(node)
        );
    }
    assert_eq!(keys::worker_of(keys::OWNER, "session/node-a"), None);
}
