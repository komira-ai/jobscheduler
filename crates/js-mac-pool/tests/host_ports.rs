//! The host's own ports: denied by number at every level that reaches loopback, after the
//! grants, and never at `Off`, which denies every port already.

use js_mac_pool::seatbelt::{
    AbsPath, Caps, MAX_DENIED_PORTS, Narrowing, Net, Policy, PolicyError, Ports, Profile,
    ReadModel, SlotLayout,
};

fn path(text: &str) -> AbsPath {
    AbsPath::parse(text).expect("a good path")
}

/// The layout of `tests/profile.rs`, whose goldens these profiles extend.
fn layout() -> SlotLayout {
    SlotLayout {
        trees: None,
        run: path("/Volumes/work/slot1/run"),
        uid_entry: Some(path("/private/var/folders/ab/cdef0123")),
        inputs: vec![path("/Volumes/work/slot1/run/in")],
        read_denies: vec![
            path("/Volumes/work/state"),
            path("/Library/Keychains/System.keychain"),
        ],
        mounts: vec![],
    }
}

fn denying(net: Net, ports: &[u16]) -> Result<Policy, PolicyError> {
    let caps = Caps {
        net,
        read_model: ReadModel::Open,
    };
    Policy::base(layout(), caps)?.narrowed(Narrowing {
        deny_ports: ports.to_vec(),
        ..Narrowing::default()
    })
}

fn text_of(policy: &Policy) -> String {
    Profile::for_policy(policy)
        .expect("a profile")
        .text()
        .to_owned()
}

fn deny_lines(port: u16) -> [String; 2] {
    [
        format!("(deny network* (local ip \"*:{port}\"))"),
        format!("(deny network* (remote ip \"localhost:{port}\"))"),
    ]
}

/// Asserts the loopback profile with three host ports, one of them given twice, against its
/// golden: the loopback golden with two denies per port, lowest port first, right after the
/// loopback grants. Catches a deny emitted before the grant it must beat, a port listed twice,
/// and a change of the rule's spelling that the runner tests measured.
#[test]
fn the_loopback_profile_with_host_ports_matches_its_golden() {
    let policy = denying(Net::Loopback, &[12049, 9300, 8021, 9300]).unwrap();
    assert_eq!(
        text_of(&policy),
        include_str!("goldens/open_loopback_host_ports.sb")
    );
}

/// Asserts, for every level that reaches loopback (`Ports`, `Loopback`, `Remote` and `On`),
/// that each denied port has both its rules and that they come after the last network grant
/// of the profile. Catches a level that leaves the host's ports reachable (`On` has no network
/// rule of its own, so a deny keyed on the loopback rules would skip it), and a deny a later
/// grant reopens.
#[test]
fn every_level_that_reaches_loopback_denies_the_host_ports_after_its_grants() {
    let one = Ports::new(&[9300]).unwrap();
    let levels = [
        Net::Ports(one.clone()),
        Net::Loopback,
        Net::Remote(one),
        Net::On,
    ];
    for net in levels {
        let text = text_of(&denying(net.clone(), &[9300, 12049]).unwrap());
        let last_grant = text.rfind("(allow network").unwrap_or(0);
        for port in [9300, 12049] {
            for line in deny_lines(port) {
                let at = text
                    .find(&line)
                    .unwrap_or_else(|| panic!("{net:?}: {line}"));
                assert!(at > last_grant, "{net:?}: {line} before a grant:\n{text}");
                assert_eq!(text.matches(&line).count(), 1, "{net:?}: {line}");
            }
        }
    }
}

/// Asserts that at `Off` denied ports add no rule: the profile is the one with no port named.
/// Catches rules emitted for a level that denies every port already, which would change the
/// profile of every job with no network.
#[test]
fn the_off_profile_names_no_port() {
    let with = text_of(&denying(Net::Off, &[9300]).unwrap());
    let without = text_of(&denying(Net::Off, &[]).unwrap());
    assert_eq!(with, without);
    assert!(!with.contains("9300"), "{with}");
}

/// Asserts that port 0 is refused by name, that [`MAX_DENIED_PORTS`] distinct ports are taken
/// and one more is refused as too many, and that a port named twice counts once. Catches a
/// `*:0` rule (no service holds port 0) and a profile that grows without bound.
#[test]
fn port_zero_and_too_many_ports_are_refused() {
    assert_eq!(
        denying(Net::Loopback, &[9300, 0]),
        Err(PolicyError::DeniedPortZero)
    );
    let bound = u16::try_from(MAX_DENIED_PORTS).unwrap();
    let at_bound: Vec<u16> = (1..=bound).collect();
    assert!(denying(Net::Loopback, &at_bound).is_ok());
    let mut twice = at_bound.clone();
    twice.push(1);
    assert!(denying(Net::Loopback, &twice).is_ok());
    let over: Vec<u16> = (1..=bound + 1).collect();
    assert_eq!(
        denying(Net::Loopback, &over),
        Err(PolicyError::TooMany("denied ports"))
    );
}
