//! What the matcher does with a key it does not know, pinned as it behaves today.
//!
//! The three readers differ on purpose: a request naming an unknown key is refused (the
//! action must not run anywhere because a constraint was dropped), a platform skips a
//! property that is not a capability (it belongs to whoever reads it), and a node report
//! skips an entry it does not match on (a newer or older daemon may send more). A change
//! to any of them fails here, so it is a visible decision.

use js_caps::{CpuCaps, NodeCaps, Request, RequestError, property_name};

const CPUINFO: &str = include_str!("fixtures/skylake_sp_platinum_8180.cpuinfo");

fn node() -> NodeCaps {
    NodeCaps::new(CpuCaps::from_linux_cpuinfo(CPUINFO).unwrap())
}

/// Catches: an unknown key dropped from a request, which would widen it to any node.
/// The key is refused alone, beside valid keys before and after it, and in the
/// error text.
#[test]
fn a_request_with_an_unknown_key_is_refused() {
    let unknown = || RequestError::UnknownKey("tier".to_owned());
    assert_eq!(Request::parse([("tier", "gold")]), Err(unknown()));
    assert_eq!(
        Request::parse([("tier", "gold"), ("arch", "x86_64")]),
        Err(unknown())
    );
    assert_eq!(
        Request::parse([("arch", "x86_64"), ("tier", "gold")]),
        Err(unknown())
    );
    assert_eq!(
        unknown().to_string(),
        "unknown capability key \"tier\"",
        "the message names the key, quoted"
    );
}

/// Catches: keys compared without regard to case, or an empty key accepted. Request
/// keys are exact, lower case: `ARCH` and `Cpus` are not `arch` and `cpus`.
#[test]
fn request_keys_are_case_sensitive_and_never_empty() {
    for key in ["ARCH", "Cpus", "OS", "Mem_Gib", ""] {
        assert_eq!(
            Request::parse([(key, "1")]),
            Err(RequestError::UnknownKey(key.to_owned())),
            "{key:?}"
        );
    }
}

/// Catches: the order of keys deciding whether an unknown key is seen. Keys are read
/// in the order given and the first fault is the one reported, so an unknown key after
/// a bad value is not reached.
#[test]
fn the_first_fault_in_key_order_is_the_one_reported() {
    assert_eq!(
        Request::parse([("tier", "gold"), ("arch", "riscv64")]),
        Err(RequestError::UnknownKey("tier".to_owned()))
    );
    assert_eq!(
        Request::parse([("arch", "riscv64"), ("tier", "gold")]),
        Err(RequestError::BadValue {
            key: "arch".to_owned(),
            value: "riscv64".to_owned(),
        })
    );
}

/// Catches: a platform property that is not a capability refused, or read as one.
/// `from_platform` leaves it out, so the request is the one without it, and
/// `property_name` says kbf does not read it.
#[test]
fn a_platform_skips_a_property_that_is_not_a_capability() {
    assert_eq!(property_name("tier"), None);
    assert_eq!(property_name("container-image"), None);
    let with = Request::from_platform([("tier", "gold"), ("OSFamily", "linux")]).unwrap();
    let without = Request::from_platform([("OSFamily", "linux")]).unwrap();
    assert_eq!(with, without);
    assert_eq!(with, Request::parse([("os", "linux")]).unwrap());
    let only_unknown = Request::from_platform([("tier", "gold")]).unwrap();
    assert_eq!(only_unknown, Request::default());
    assert!(only_unknown.matches(&node()));
}

/// Catches: an unknown report entry refused or recorded. A node report with extra
/// entries describes the node it describes without them.
#[test]
fn a_node_report_skips_an_entry_it_does_not_match_on() {
    let base = [("arch", "x86_64"), ("os", "linux"), ("cpus", "8")];
    let extra = [
        ("arch", "x86_64"),
        ("tier", "gold"),
        ("os", "linux"),
        ("cpus", "8"),
        ("isa_level", "x86-64-v3"),
    ];
    assert_eq!(
        NodeCaps::from_report(extra).unwrap(),
        NodeCaps::from_report(base).unwrap()
    );
}
