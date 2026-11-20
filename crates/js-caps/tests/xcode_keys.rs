//! The capability key `xcode`: named, detected so an operator cannot claim it, and read from
//! a node report whose one value holds every installed build joined with `,`.

use js_caps::{DETECTED_KEYS, NodeCaps, Request, Unmet, XCODE};

fn req(props: &[(&str, &str)]) -> Request {
    Request::parse(props.iter().copied()).unwrap()
}

/// A node reporting `entries` beside an arch.
fn node(entries: &[(&str, &str)]) -> NodeCaps {
    let arch = [("arch", "arm64")];
    NodeCaps::from_report(arch.iter().chain(entries).copied()).unwrap()
}

/// Asserts that the key carries the name the agent writes and is among the detected keys, the
/// list the agent refuses an operator's `--capability` against. Catches a key left out of
/// the list: a flag could then claim an Xcode the host does not have.
#[test]
fn a_flag_cannot_claim_xcode() {
    assert_eq!(XCODE, "xcode");
    assert!(DETECTED_KEYS.contains(&XCODE));
}

/// Asserts that a report whose `xcode` value is `16C5032a,16E140` holds both builds as
/// members, and that a request for either matches it while a request for another build
/// does not and names the build as unmet. Catches a joined value read as one build, which
/// no request names.
#[test]
fn a_joined_xcode_value_holds_every_build() {
    let mac = node(&[("xcode", "16C5032a,16E140")]);
    let builds: Vec<&str> = mac.members["xcode"].iter().map(String::as_str).collect();
    assert_eq!(builds, ["16C5032a", "16E140"]);
    assert!(req(&[("xcode", "16E140")]).matches(&mac));
    assert!(req(&[("xcode", "16C5032a")]).matches(&mac));
    let other = req(&[("xcode", "15F31d")]);
    assert!(!other.matches(&mac));
    assert_eq!(
        other.unmet(&mac),
        [Unmet::Member {
            key: "xcode",
            want: "15F31d"
        }]
    );
}

/// Asserts that a value with no comma stays one build, that empty pieces between commas are
/// skipped, that a joined value and a repeated entry add to the same set, and that a report
/// with an empty `xcode` value holds no build. Catches a split that keeps an empty member
/// or one that replaces the set at each entry.
#[test]
fn a_single_build_stays_one_member_and_empty_pieces_are_skipped() {
    let one = node(&[("xcode", "16E140")]);
    assert!(req(&[("xcode", "16E140")]).matches(&one));
    assert_eq!(one.members["xcode"].len(), 1);

    let gaps = node(&[("xcode", ",16C5032a,,16E140,")]);
    let builds: Vec<&str> = gaps.members["xcode"].iter().map(String::as_str).collect();
    assert_eq!(builds, ["16C5032a", "16E140"]);

    let mixed = node(&[("xcode", "16C5032a,16E140"), ("xcode", "15F31d")]);
    let builds: Vec<&str> = mixed.members["xcode"].iter().map(String::as_str).collect();
    assert_eq!(builds, ["15F31d", "16C5032a", "16E140"]);

    let none = node(&[("xcode", "")]);
    assert!(!req(&[("xcode", "16E140")]).matches(&none));
}
