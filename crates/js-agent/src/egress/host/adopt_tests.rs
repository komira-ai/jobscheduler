//! What a start keeps of an earlier process's wiring, read from the records of the egress store
//! and the interfaces on the host: the names it reads, the records it can and cannot keep (with
//! why), the strays, and the text it puts back. The calls it makes against the scripted
//! machine, and the takeover and the settle, are in `tests/takeover.rs` of the host side's
//! tests.

use std::net::Ipv4Addr;

use super::super::tests::records::{forwarding, put, scratch, store_in};
use super::super::tests::{Fake, links};
use super::*;

fn forward(host: u16, last: u8, container: u16, iface: &str) -> Forward {
    Forward {
        host,
        job: Ipv4Addr::new(198, 18, 0, last),
        container,
        iface: iface.to_owned(),
    }
}

fn port(host: u16, container: u16) -> PortMap {
    PortMap { host, container }
}

/// Asserts that only `<prefix><digits>` is a numbered name: `jsv12` is block 12 and `jsvx`,
/// `jspare0` (for `jsp`), `jsv`, `eth0`, a name with a sign and a number too big for a block
/// are none. Catches a read of the agent's interfaces that takes another program's for its own.
#[test]
fn only_a_prefix_and_digits_is_a_numbered_name() {
    assert_eq!(numbered("jsv12", "jsv"), Some(12));
    assert_eq!(numbered("jsp0", "jsp"), Some(0));
    for name in [
        "jsvx",
        "jsv",
        "jsv1x",
        "jsv-1",
        "jsv+1",
        "eth0",
        "",
        "jsv99999999999",
    ] {
        assert_eq!(numbered(name, "jsv"), None, "{name}");
    }
    assert_eq!(numbered("jspare0", "jsp"), None);
    assert_eq!(numbered("jsp3", "jsv"), None);
}

/// Asserts what a start keeps and why it drops the rest, with the exact reason of each record
/// it cannot keep: `above` (an `open` record under the `allow-list` ceiling), `none-level` (a
/// record of the level `none`), `outside` (a host port the agent does not forward), `pool` (a
/// block past the pool), `dup-b` (the block of `dup-a`, which is kept as it sorts first) and
/// `broken` (not JSON), in the order of the runs' names; the two kept records with their ports
/// sorted and their forwards; and the strays: the unrecorded numbered ends (`jsv2` and `jsp3`
/// and `jsv7`, though `jsv2` is the block of a record that was refused) and not `jsvx`,
/// `jspare0`, `eth0` or an entry with no name. Catches a start that keeps wiring a record
/// cannot vouch for, or takes another program's interface for its own.
#[test]
fn found_keeps_usable_records_names_why_it_drops_the_rest_and_finds_the_strays() {
    let dir = scratch("found");
    let store = store_in(&dir);
    put(&store, "above", (2, JobNetwork::Open), &[]);
    put(&store, "outside", (3, JobNetwork::AllowList), &[(9101, 80)]);
    put(&store, "pool", (99999, JobNetwork::AllowList), &[]);
    put(&store, "dup-a", (5, JobNetwork::AllowList), &[]);
    put(&store, "dup-b", (5, JobNetwork::AllowList), &[]);
    put(
        &store,
        "ok",
        (1, JobNetwork::AllowList),
        &[(9000, 90), (8080, 80)],
    );
    std::fs::write(store.dir().join("broken.json"), "nope").expect("a file");
    std::fs::write(
        store.dir().join("none-level.json"),
        r#"{"version":1,"index":6,"level":"none"}"#,
    )
    .expect("a file");
    let fake = Fake::new();
    let names = [
        "jsv1", "jsv2", "jsp3", "jsv5", "jsv7", "jsvx", "jspare0", "eth0",
    ];
    fake.answer("link show", Ok(links(&names).replace("]", ",{\"mtu\":1}]")));
    let config = forwarding("allow-list");
    let left = found(&*fake, &config, Some(&store)).expect("found");
    let unusable = |run: &str, why: &str| (run.to_owned(), why.to_owned());
    assert_eq!(
        left.unusable,
        [
            unusable("above", "the ceiling allow-list does not allow open"),
            unusable("broken", "not a record: expected ident at line 1 column 2"),
            unusable("dup-b", "block 5 is another run's"),
            unusable("none-level", "the ceiling allow-list does not allow none"),
            unusable(
                "outside",
                "host port 9101 is not among the ports this agent forwards"
            ),
            unusable("pool", "block 99999 is not one of the pool"),
        ]
    );
    let kept = |index, ports: &[(u16, u16)], forwards| Kept {
        index,
        level: JobNetwork::AllowList,
        set: IFACE_SET,
        ports: ports
            .iter()
            .map(|&(host, container)| port(host, container))
            .collect(),
        forwards,
    };
    assert_eq!(
        left.kept,
        BTreeMap::from([
            ("dup-a".to_owned(), kept(5, &[], Vec::new())),
            (
                "ok".to_owned(),
                kept(
                    1,
                    &[(8080, 80), (9000, 90)],
                    vec![forward(8080, 6, 80, "jsv1"), forward(9000, 6, 90, "jsv1")]
                )
            ),
        ])
    );
    assert_eq!(left.strays, BTreeSet::from([2, 3, 7]));
    assert_eq!(left.blocks(), BTreeSet::from([1, 2, 3, 5, 7]));
}

/// Asserts that with no store nothing is kept and every numbered end on the host is a stray
/// (so the settle removes them all): `jsv0` and `jsp4` are blocks 0 and 4, and the text put
/// back after the ruleset is nothing. Catches an agent with no state root that leaves an earlier
/// process's interfaces in the way of its own.
#[test]
fn with_no_store_nothing_is_kept_and_every_numbered_interface_is_a_stray() {
    let fake = Fake::new();
    fake.answer("link show", Ok(links(&["jsv0", "jsp4", "eth0"])));
    let left = found(&*fake, &forwarding("allow-list"), None).expect("found");
    assert_eq!(left.kept, BTreeMap::new());
    assert_eq!(left.unusable, []);
    assert_eq!(left.strays, BTreeSet::from([0, 4]));
    assert_eq!(left.text(), "");
}

/// Asserts that a store that cannot be listed (its directory is gone) fails `found` with the
/// reading of the store and the reason, and runs nothing. Catches a start that goes on as if
/// the store held no records, and so removes the wiring of runs it cannot see.
#[test]
fn a_store_that_cannot_be_listed_fails_found_with_its_reason() {
    let dir = scratch("found-gone");
    let store = store_in(&dir);
    std::fs::remove_dir_all(store.dir()).expect("removed");
    let fake = Fake::new();
    let error = found(&*fake, &forwarding("allow-list"), Some(&store)).expect_err("fails");
    assert_eq!(
        error,
        EgressError::Command {
            what: "reading the egress store".to_owned(),
            why: "No such file or directory (os error 2)".to_owned(),
        }
    );
    assert_eq!(fake.log(), Vec::<String>::new());
}

/// Asserts the lines that follow the ruleset: each kept interface in the set of its level in the
/// order of the runs' names, then every kept forward sorted by host port whatever block holds
/// it; the blocks kept out of use are the kept ones and the strays, each once; nothing kept
/// adds no text. Catches a transaction that puts back a member in the wrong set, a forward out
/// of order, or a block given out that an earlier process still wires.
#[test]
fn what_is_kept_is_put_back_as_members_then_forwards_and_its_blocks_stay_in_use() {
    assert_eq!(Left::default().text(), "");
    assert_eq!(Left::default().blocks(), BTreeSet::new());
    let kept = |index, level, set, forwards| Kept {
        index,
        level,
        set,
        ports: Vec::new(),
        forwards,
    };
    let left = Left {
        kept: BTreeMap::from([
            (
                "run-2".to_owned(),
                kept(
                    2,
                    JobNetwork::AllowList,
                    IFACE_SET,
                    vec![forward(9000, 10, 90, "jsv2")],
                ),
            ),
            (
                "run-0".to_owned(),
                kept(
                    0,
                    JobNetwork::Open,
                    OPEN_SET,
                    vec![forward(8080, 2, 80, "jsv0")],
                ),
            ),
            (
                "run-1".to_owned(),
                kept(1, JobNetwork::AllowList, IFACE_SET, Vec::new()),
            ),
        ]),
        unusable: vec![("old".to_owned(), "why".to_owned())],
        strays: BTreeSet::from([1, 5]),
    };
    let wanted = [forward(8080, 2, 80, "jsv0"), forward(9000, 10, 90, "jsv2")];
    assert_eq!(
        left.text(),
        format!(
            "add element inet js_crun_egress openifs {{ \"jsv0\" }}\n\
             add element inet js_crun_egress jobifs {{ \"jsv1\" }}\n\
             add element inet js_crun_egress jobifs {{ \"jsv2\" }}\n{}",
            forward::transaction(&wanted)
        )
    );
    assert_eq!(left.blocks(), BTreeSet::from([0, 1, 2, 5]));
}
