//! The generated profile: a golden per network level, the order, the floor, narrowing.

use std::ffi::OsString;
use std::net::Ipv4Addr;

use js_mac_pool::seatbelt::{
    AbsPath, BuildError, Caps, MAX_LISTED, Narrowing, Net, Policy, PolicyError, Ports, Profile,
    ReadModel, SANDBOX_EXEC, SlotLayout,
};

fn path(text: &str) -> AbsPath {
    AbsPath::new(text).expect("a good path")
}

fn layout() -> SlotLayout {
    SlotLayout {
        run: path("/Volumes/work/slot1/run"),
        uid_entry: path("/private/var/folders/ab/cdef0123"),
        inputs: vec![path("/Volumes/work/slot1/run/in")],
        read_denies: vec![
            path("/Volumes/work/state"),
            path("/Library/Keychains/System.keychain"),
        ],
    }
}

fn bare_layout() -> SlotLayout {
    SlotLayout {
        inputs: vec![],
        read_denies: vec![],
        ..layout()
    }
}

fn caps(net: Net) -> Caps {
    Caps {
        net,
        read_model: ReadModel::Open,
        deny_egress: vec![],
    }
}

fn text_of(policy: &Policy) -> String {
    Profile::for_policy(policy)
        .expect("a profile")
        .text()
        .to_owned()
}

fn ports(list: &[u16]) -> Ports {
    Ports::new(list.iter().copied()).expect("ports")
}

/// The last eight lines of every profile, written out here on its own.
const FLOOR_TEXT: &str = "(deny mach-register)
(deny file-mount)
(deny job-creation)
(deny lsopen)
(deny file-read* file-write* (subpath \"/private/var/protected/sfanalytics\") (subpath \"/private/var/protected/trustd/private\"))
(deny mach-lookup (global-name-prefix \"com.apple.CoreDevice.\"))
(deny mach-lookup (global-name-prefix \"com.apple.remoted\"))
(deny network-outbound (literal \"/private/var/run/usbmuxd\"))
";

#[test]
fn the_off_profile_matches_its_golden() {
    let policy = Policy::base(layout(), caps(Net::Off)).unwrap();
    assert_eq!(text_of(&policy), include_str!("goldens/open_off.sb"));
}

#[test]
fn the_ports_profile_lists_single_loopback_ports_in_order() {
    let policy = Policy::base(layout(), caps(Net::Ports(ports(&[8080, 80, 8080])))).unwrap();
    assert_eq!(text_of(&policy), include_str!("goldens/open_ports.sb"));
}

#[test]
fn the_loopback_profile_matches_its_golden() {
    let policy = Policy::base(layout(), caps(Net::Loopback)).unwrap();
    assert_eq!(text_of(&policy), include_str!("goldens/open_loopback.sb"));
}

#[test]
fn the_on_profile_denies_the_metadata_address_and_the_listed_egress_in_order() {
    let mut on = caps(Net::On);
    on.deny_egress = vec![Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(10, 0, 0, 5)];
    let policy = Policy::base(layout(), on).unwrap();
    assert_eq!(text_of(&policy), include_str!("goldens/open_on.sb"));
}

#[test]
fn a_profile_without_the_user_folder_grant_or_lists_matches_its_golden() {
    let policy = Policy::base(bare_layout(), caps(Net::Off)).unwrap();
    let narrowed = policy
        .narrowed(Narrowing {
            drop_uid_entry: true,
            ..Narrowing::default()
        })
        .unwrap();
    assert_eq!(text_of(&narrowed), include_str!("goldens/open_off_bare.sb"));
}

#[test]
fn the_allow_read_model_is_refused_not_run_as_open() {
    let mut allow = caps(Net::Off);
    allow.read_model = ReadModel::Allow;
    let policy = Policy::base(layout(), allow).unwrap();
    assert_eq!(policy.read_model(), ReadModel::Allow);
    assert_eq!(
        Profile::for_policy(&policy),
        Err(BuildError::ReadModelUnavailable)
    );
}

#[test]
fn every_path_is_a_named_parameter_in_a_fixed_order() {
    let policy = Policy::base(layout(), caps(Net::Off)).unwrap();
    let profile = Profile::for_policy(&policy).unwrap();
    let expected: Vec<(String, String)> = [
        ("RUN", "/Volumes/work/slot1/run"),
        ("UENTRY", "/private/var/folders/ab/cdef0123"),
        ("RD_0", "/Library/Keychains/System.keychain"),
        ("RD_1", "/Volumes/work/state"),
        ("IN_0", "/Volumes/work/slot1/run/in"),
    ]
    .iter()
    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
    .collect();
    assert_eq!(profile.params(), expected.as_slice());
}

#[test]
fn the_argv_is_sandbox_exec_parameters_profile_program_and_arguments() {
    let policy = Policy::base(bare_layout(), caps(Net::Off)).unwrap();
    let profile = Profile::for_policy(&policy).unwrap();
    let args = [OsString::from("-c"), OsString::from("echo hi")];
    let argv = profile.argv(&path("/bin/sh"), &args);
    let expected = [
        OsString::from(SANDBOX_EXEC),
        OsString::from("-D"),
        OsString::from("RUN=/Volumes/work/slot1/run"),
        OsString::from("-D"),
        OsString::from("UENTRY=/private/var/folders/ab/cdef0123"),
        OsString::from("-p"),
        OsString::from(profile.text()),
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from("echo hi"),
    ];
    assert_eq!(argv, expected);
    assert_eq!(SANDBOX_EXEC, "/usr/bin/sandbox-exec");
}

/// Every combination the builder can emit ends with the floor, in order, and a policy
/// with every deny added still does.
#[test]
fn the_floor_is_last_in_every_profile() {
    let nets = [
        Net::Off,
        Net::Ports(ports(&[1])),
        Net::Ports(ports(&[1, 2, 3])),
        Net::Loopback,
        Net::On,
    ];
    for net in nets {
        for drop_uid_entry in [false, true] {
            for full in [false, true] {
                let base = if full { layout() } else { bare_layout() };
                let narrowing = Narrowing {
                    drop_uid_entry,
                    protect: vec![path("/Volumes/work/other")],
                    deny_read: vec![path("/Volumes/work/secret")],
                    deny_egress: vec![Ipv4Addr::new(1, 2, 3, 4)],
                    ..Narrowing::default()
                };
                let policy = Policy::base(base, caps(net.clone()))
                    .unwrap()
                    .narrowed(narrowing)
                    .unwrap();
                let text = text_of(&policy);
                assert!(text.ends_with(FLOOR_TEXT), "{net:?}:\n{text}");
                assert_eq!(text.matches("(deny mach-register)").count(), 1);
            }
        }
    }
}

#[test]
fn a_narrowing_meets_levels_and_never_raises_one() {
    let two = ports(&[80, 8080]);
    let cases: [(Net, Net, Net); 11] = [
        (Net::On, Net::Off, Net::Off),
        (Net::Off, Net::On, Net::Off),
        (Net::On, Net::Loopback, Net::Loopback),
        (Net::Loopback, Net::On, Net::Loopback),
        (Net::On, Net::On, Net::On),
        (
            Net::Loopback,
            Net::Ports(two.clone()),
            Net::Ports(two.clone()),
        ),
        (
            Net::Ports(two.clone()),
            Net::Loopback,
            Net::Ports(two.clone()),
        ),
        (Net::Ports(two.clone()), Net::On, Net::Ports(two.clone())),
        (Net::Ports(two.clone()), Net::Off, Net::Off),
        (
            Net::Ports(two.clone()),
            Net::Ports(ports(&[8080, 9000])),
            Net::Ports(ports(&[8080])),
        ),
        (Net::Ports(two), Net::Ports(ports(&[9000])), Net::Off),
    ];
    for (base, request, expected) in cases {
        let policy = Policy::base(layout(), caps(base.clone())).unwrap();
        let narrowed = policy
            .narrowed(Narrowing {
                net: Some(request.clone()),
                ..Narrowing::default()
            })
            .unwrap();
        assert_eq!(narrowed.net(), &expected, "{base:?} met with {request:?}");
    }
}

#[test]
fn a_narrowing_with_no_level_keeps_the_level() {
    let policy = Policy::base(layout(), caps(Net::Loopback)).unwrap();
    let same = policy.clone().narrowed(Narrowing::default()).unwrap();
    assert_eq!(same, policy);
}

#[test]
fn a_narrowing_adds_denies_and_protections_without_repeating_existing_ones() {
    let policy = Policy::base(layout(), caps(Net::On)).unwrap();
    let narrowed = policy
        .narrowed(Narrowing {
            protect: vec![
                path("/Volumes/work/slot1/run/in"),
                path("/Volumes/work/slot1/run/ro"),
            ],
            deny_read: vec![path("/Users/Shared")],
            deny_egress: vec![Ipv4Addr::new(169, 254, 169, 254), Ipv4Addr::new(8, 8, 8, 8)],
            ..Narrowing::default()
        })
        .unwrap();
    let profile = Profile::for_policy(&narrowed).unwrap();
    let names: Vec<&str> = profile
        .params()
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        ["RUN", "UENTRY", "RD_0", "RD_1", "RD_2", "IN_0", "IN_1"]
    );
    let text = profile.text();
    assert_eq!(text.matches("169.254.169.254:*").count(), 1);
    assert!(text.contains("(deny network-outbound (remote ip \"8.8.8.8:*\"))"));
}

#[test]
fn a_request_over_the_cap_gets_the_cap() {
    let policy = Policy::base(layout(), caps(Net::Loopback)).unwrap();
    let asked = policy
        .narrowed(Narrowing {
            net: Some(Net::On),
            ..Narrowing::default()
        })
        .unwrap();
    assert_eq!(text_of(&asked), include_str!("goldens/open_loopback.sb"));
}

#[test]
fn dropping_the_user_folder_grant_leaves_the_run_directory_and_dev() {
    let policy = Policy::base(bare_layout(), caps(Net::Off)).unwrap();
    let dropped = policy
        .narrowed(Narrowing {
            drop_uid_entry: true,
            ..Narrowing::default()
        })
        .unwrap();
    let text = text_of(&dropped);
    assert!(text.contains("(allow file-write* (subpath (param \"RUN\")) (subpath \"/dev\"))"));
    assert!(!text.contains("UENTRY"));
}

#[test]
fn lists_over_the_bound_are_refused_where_they_grow() {
    let many = |n: usize, stem: &str| -> Vec<AbsPath> {
        (0..n)
            .map(|i| path(&format!("/Volumes/{stem}/{i}")))
            .collect()
    };
    let addrs = |n: u8| -> Vec<Ipv4Addr> { (0..n).map(|i| Ipv4Addr::new(10, 0, 0, i)).collect() };
    let at_bound = u8::try_from(MAX_LISTED).unwrap();

    let ok = SlotLayout {
        inputs: many(MAX_LISTED, "in"),
        read_denies: many(MAX_LISTED, "rd"),
        ..layout()
    };
    let mut full = caps(Net::On);
    full.deny_egress = addrs(at_bound - 1);
    let policy = Policy::base(ok, full).expect("at the bound is fine");

    let over = |narrowing: Narrowing| policy.clone().narrowed(narrowing);
    assert_eq!(
        over(Narrowing {
            protect: many(1, "more"),
            ..Narrowing::default()
        }),
        Err(PolicyError::TooMany("inputs"))
    );
    assert_eq!(
        over(Narrowing {
            deny_read: many(1, "more"),
            ..Narrowing::default()
        }),
        Err(PolicyError::TooMany("read denies"))
    );
    // 63 listed plus the one new address is the bound; two new addresses is over it.
    assert!(
        over(Narrowing {
            deny_egress: addrs(at_bound),
            ..Narrowing::default()
        })
        .is_ok()
    );
    assert_eq!(
        over(Narrowing {
            deny_egress: vec![Ipv4Addr::new(11, 0, 0, 1), Ipv4Addr::new(11, 0, 0, 2)],
            ..Narrowing::default()
        }),
        Err(PolicyError::TooMany("denied egress"))
    );
}

#[test]
fn a_layout_over_the_bound_is_refused() {
    let inputs: Vec<AbsPath> = (0..=MAX_LISTED)
        .map(|i| path(&format!("/Volumes/in/{i}")))
        .collect();
    let big = SlotLayout { inputs, ..layout() };
    assert_eq!(
        Policy::base(big, caps(Net::Off)),
        Err(PolicyError::TooMany("inputs"))
    );
}
