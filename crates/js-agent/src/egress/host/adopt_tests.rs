//! The pure parts of taking over an earlier process's wiring: the names it reads, the members
//! of the sets it keeps and the text it puts back. The calls it makes against the scripted
//! machine are in `tests/takeover.rs` of the host side's tests.

use std::net::Ipv4Addr;

use serde_json::json;

use super::*;

fn forward(host: u16, last: u8, iface: &str) -> Forward {
    Forward {
        host,
        job: Ipv4Addr::new(198, 18, 0, last),
        container: 80,
        iface: iface.to_owned(),
    }
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

/// Asserts which set each member is in: under `open` a member of `jobifs` only is in
/// `jobifs`, of `openifs` only in `openifs`, and of both in none; under `allow-list` (which has
/// no `openifs`) a member of `openifs` is in none; a set the agent does not make, an entry with
/// no name, a set with no members, a member that is not text and a listing with no
/// `nftables` add nothing. Catches a start that puts a member back in a set where it would
/// widen its access.
#[test]
fn a_member_in_both_sets_or_in_a_set_the_ceiling_lacks_is_in_none() {
    let listing = json!({"nftables": [
        {"metainfo": {"version": "1.0.9"}},
        {"set": {"name": "jobifs", "elem": ["jsv0", "jsv1"]}},
        {"set": {"name": "openifs", "elem": ["jsv1", "jsv2", 5, "jsv4"]}},
        {"set": {"name": "other", "elem": ["jsv9"]}},
        {"set": {"elem": ["jsv8"]}},
        {"set": {"name": "jobifs"}},
        {"rule": {"chain": "forward"}},
    ]});
    let key = |name: &str| name.to_owned();
    assert_eq!(
        members(&listing, Mode::Open),
        BTreeMap::from([
            (key("jsv0"), Some(IFACE_SET)),
            (key("jsv1"), None),
            (key("jsv2"), Some(OPEN_SET)),
            (key("jsv4"), Some(OPEN_SET)),
        ])
    );
    assert_eq!(
        members(&listing, Mode::AllowList),
        BTreeMap::from([
            (key("jsv0"), Some(IFACE_SET)),
            (key("jsv1"), None),
            (key("jsv2"), None),
            (key("jsv4"), None),
        ])
    );
    assert_eq!(members(&json!({}), Mode::Open), BTreeMap::new());
    assert_eq!(
        members(&json!({"nftables": 3}), Mode::Open),
        BTreeMap::new()
    );
}

/// Asserts the lines that follow the ruleset: each kept member in its set in block order (a
/// block in no set has none), then every kept forward sorted by host port whatever block holds
/// it; the blocks kept out of use are the kept ones and the strays, each once; nothing kept
/// adds no text. Catches a transaction that puts back a member in the wrong set, a forward
/// out of order, or a block given out that an earlier process still wires.
#[test]
fn what_is_kept_is_put_back_as_members_then_forwards_and_its_blocks_stay_in_use() {
    assert_eq!(Left::default().text(), "");
    assert_eq!(Left::default().blocks(), BTreeSet::new());
    let left = Left {
        kept: BTreeMap::from([
            (
                2,
                Kept {
                    ifindex: 12,
                    set: Some(IFACE_SET),
                    forwards: vec![forward(9000, 10, "jsv2")],
                },
            ),
            (
                0,
                Kept {
                    ifindex: 10,
                    set: Some(OPEN_SET),
                    forwards: vec![forward(8080, 2, "jsv0")],
                },
            ),
            (
                1,
                Kept {
                    ifindex: 11,
                    set: None,
                    forwards: Vec::new(),
                },
            ),
        ]),
        strays: BTreeSet::from([5]),
    };
    let wanted = [forward(8080, 2, "jsv0"), forward(9000, 10, "jsv2")];
    assert_eq!(
        left.text(),
        format!(
            "add element inet js_crun_egress openifs {{ \"jsv0\" }}\n\
             add element inet js_crun_egress jobifs {{ \"jsv2\" }}\n{}",
            forward::transaction(&wanted)
        )
    );
    assert_eq!(left.blocks(), BTreeSet::from([0, 1, 2, 5]));
}
