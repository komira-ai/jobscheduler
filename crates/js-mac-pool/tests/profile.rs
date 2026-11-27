//! The generated profile: a golden per network level, the order, the floor, narrowing.

use std::ffi::OsString;

use js_mac_pool::seatbelt::{
    AbsPath, Caps, MAX_LISTED, MountGrant, Narrowing, Net, Policy, PolicyError, Ports, Profile,
    ReadModel, SANDBOX_EXEC, SlotLayout,
};

fn path(text: &str) -> AbsPath {
    AbsPath::parse(text).expect("a good path")
}

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
    }
}

fn text_of(policy: &Policy) -> String {
    Profile::for_policy(policy)
        .expect("a profile")
        .text()
        .to_owned()
}

fn ports(list: &[u16]) -> Ports {
    Ports::new(list).expect("ports")
}

/// The last nine lines of every profile, written out here on its own.
const FLOOR_TEXT: &str = "(deny mach-register)
(deny file-mount)
(deny ipc-posix-shm-write-create)
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

/// The `localhost:PORT` rule of each port, in the order given.
fn loopback_rules(list: &[u16]) -> String {
    list.iter()
        .map(|port| format!("(allow network* (remote ip \"localhost:{port}\"))\n"))
        .collect()
}

/// Asserts that a job's host ports on a policy at `Off` (a ceiling of `off`) render as the
/// `Off` rules and one `localhost:PORT` allow per port, lowest first and each once, right
/// after them and before the read-deny socket rules, matching the golden whole. Catches host
/// ports dropped at the level that needs them, a port listed twice, and a host port that
/// opens all of loopback.
#[test]
fn host_ports_at_off_add_one_loopback_rule_per_port() {
    let policy = Policy::base(layout(), caps(Net::Off))
        .unwrap()
        .with_host_ports(ports(&[5432, 80, 5432]));
    assert_eq!(policy.net(), &Net::Off, "host ports are no level");
    assert_eq!(
        text_of(&policy),
        include_str!("goldens/open_off_host_ports.sb")
    );
}

/// Asserts that host ports on a policy at `Ports` are listed with the level's own ports as one
/// set, lowest first and each once (a port both name is one rule), and that the profile is the
/// `Off` profile with exactly those lines. Catches a port of both lists listed twice, and the
/// level's ports replaced by the host ports.
#[test]
fn host_ports_at_ports_join_the_levels_ports() {
    let policy = Policy::base(layout(), caps(Net::Ports(ports(&[8080, 80]))))
        .unwrap()
        .with_host_ports(ports(&[9000, 80]));
    let off = include_str!("goldens/open_off.sb");
    let syslog = "(allow network-outbound (literal \"/private/var/run/syslog\"))\n";
    let want = off.replacen(
        syslog,
        &format!("{syslog}{}", loopback_rules(&[80, 8080, 9000])),
        1,
    );
    assert_eq!(text_of(&policy), want);
}

/// Asserts that host ports add nothing at `Loopback`, `Remote` and `On`, where loopback is not
/// filtered: each profile is the one of its level without them, to the byte. Catches a host
/// port rule that narrows an open loopback, and one rendered where it cannot matter.
#[test]
fn host_ports_add_nothing_where_loopback_is_open() {
    for net in [Net::Loopback, Net::Remote(ports(&[443])), Net::On] {
        let without = Policy::base(layout(), caps(net.clone())).unwrap();
        let with = without.clone().with_host_ports(ports(&[5432]));
        assert_eq!(text_of(&with), text_of(&without), "{net:?}");
        assert!(!text_of(&with).contains("localhost:5432"), "{net:?}");
    }
}

/// Asserts that a narrowing applied after the host ports lowers the level and keeps the ports
/// (a run narrowed from `Loopback` to `Off` still reaches its host port, and no other loopback
/// port), and that host ports given again replace the earlier ones. Catches host ports lost to
/// a later narrowing, which would refuse the job's service, and two lists merged.
#[test]
fn host_ports_outlive_a_narrowing_and_the_last_given_win() {
    let narrowed = Policy::base(layout(), caps(Net::Loopback))
        .unwrap()
        .with_host_ports(ports(&[80, 5432]))
        .narrowed(Narrowing {
            net: Some(Net::Off),
            ..Narrowing::default()
        })
        .unwrap();
    assert_eq!(narrowed.net(), &Net::Off);
    assert_eq!(
        text_of(&narrowed),
        include_str!("goldens/open_off_host_ports.sb")
    );
    let replaced = Policy::base(layout(), caps(Net::Off))
        .unwrap()
        .with_host_ports(ports(&[7000]))
        .with_host_ports(ports(&[5432, 80]));
    assert_eq!(
        text_of(&replaced),
        include_str!("goldens/open_off_host_ports.sb")
    );
}

#[test]
fn the_loopback_profile_matches_its_golden() {
    let policy = Policy::base(layout(), caps(Net::Loopback)).unwrap();
    assert_eq!(text_of(&policy), include_str!("goldens/open_loopback.sb"));
}

#[test]
fn the_on_profile_has_no_network_rule() {
    let policy = Policy::base(layout(), caps(Net::On)).unwrap();
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
fn a_layout_that_knows_no_user_folder_entry_grants_nothing_there() {
    let layout = SlotLayout {
        uid_entry: None,
        ..bare_layout()
    };
    let policy = Policy::base(layout, caps(Net::Off)).unwrap();
    assert_eq!(text_of(&policy), include_str!("goldens/open_off_bare.sb"));
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
    let argv = profile.argv(path("/bin/sh"), &args);
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
    let policy = Policy::base(layout(), caps(Net::Off)).unwrap();
    let narrowed = policy
        .narrowed(Narrowing {
            protect: vec![
                path("/Volumes/work/slot1/run/in"),
                path("/Volumes/work/slot1/run/ro"),
            ],
            deny_read: vec![path("/Users/Shared")],
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
    assert!(
        profile
            .text()
            .contains("(deny file-read* (subpath (param \"RD_1\")))")
    );
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
    assert!(text.contains("(allow file-write* (subpath (param \"RUN\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))"));
    assert!(!text.contains("UENTRY"));
}

#[test]
fn lists_over_the_bound_are_refused_where_they_grow() {
    let many = |n: usize, stem: &str| -> Vec<AbsPath> {
        (0..n)
            .map(|i| path(&format!("/Volumes/{stem}/{i}")))
            .collect()
    };
    let at_bound = SlotLayout {
        inputs: many(MAX_LISTED, "in"),
        read_denies: many(MAX_LISTED, "rd"),
        ..layout()
    };
    let policy = Policy::base(at_bound, caps(Net::Off)).expect("at the bound is fine");
    let over = |narrowing: Narrowing| policy.clone().narrowed(narrowing);

    let repeated = over(Narrowing {
        protect: many(1, "in"),
        deny_read: many(1, "rd"),
        ..Narrowing::default()
    });
    assert!(
        repeated.is_ok(),
        "an entry already listed does not grow the list"
    );
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

fn mount(text: &str, write: bool, no_exec: bool) -> MountGrant {
    MountGrant {
        path: path(text),
        write,
        no_exec,
    }
}

fn mounted() -> SlotLayout {
    SlotLayout {
        mounts: vec![
            mount("/Volumes/data/out", true, false),
            mount("/Volumes/data/in", false, true),
        ],
        ..layout()
    }
}

/// Asserts the whole text of a profile with a read-only no-exec mount and a read-write
/// mount, and the parameters in the order they are passed (mounts sorted by path).
///
/// Catches a grant that is missing, in the wrong place, or that gives a read-only mount
/// a write grant.
#[test]
fn a_profile_with_two_mounts_matches_its_golden() {
    let policy = Policy::base(mounted(), caps(Net::Off)).unwrap();
    let profile = Profile::for_policy(&policy).unwrap();
    assert_eq!(profile.text(), include_str!("goldens/open_off_mounts.sb"));
    let granted: Vec<_> = profile
        .params()
        .iter()
        .filter(|(name, _)| name.starts_with("GR_"))
        .cloned()
        .collect();
    assert_eq!(
        granted,
        vec![
            ("GR_0".to_owned(), "/Volumes/data/in".to_owned()),
            ("GR_1".to_owned(), "/Volumes/data/out".to_owned()),
        ]
    );
}

/// A layout with no mounts renders the same text as before mounts existed.
///
/// Catches a mount rule or parameter that appears without a mount.
#[test]
fn a_layout_without_mounts_renders_no_mount_rule() {
    let policy = Policy::base(layout(), caps(Net::Off)).unwrap();
    let profile = Profile::for_policy(&policy).unwrap();
    assert!(!profile.text().contains("GR_"));
    assert!(
        profile
            .params()
            .iter()
            .all(|(name, _)| !name.starts_with("GR_"))
    );
}

/// The mount grants come after the read denies and the input write denies (a later rule
/// wins, so the grant reopens a path under a denied mount root), the exec denies come
/// after every grant, and the network rules and the floor follow.
///
/// Catches a grant placed before the deny it must beat, and an exec deny that a later
/// grant would reopen.
#[test]
fn mount_grants_follow_the_denies_the_exec_denies_follow_the_grants_and_the_floor_is_last() {
    let policy = Policy::base(mounted(), caps(Net::Off)).unwrap();
    let text = text_of(&policy);
    let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("no {needle}"));
    let last_at = |needle: &str| text.rfind(needle).unwrap_or_else(|| panic!("no {needle}"));
    let denies = at("(deny file-write* (subpath (param \"RD_1\")))")
        .max(at("(deny file-write* (subpath (param \"IN_0\")))"));
    let first_grant = at("(allow file-read* (subpath (param \"GR_0\")))");
    let last_grant = last_at("(allow file-write* (subpath (param \"GR_1\")))");
    let first_exec = at("(deny process-exec* (subpath (param \"GR_0\")))");
    let last_exec = last_at("(deny file-map-executable (subpath (param \"GR_0\")))");
    let network = at("(deny network*)");
    assert!(denies < first_grant);
    assert!(last_grant < first_exec);
    assert!(last_exec < network);
    assert!(text.ends_with(FLOOR_TEXT));
}

/// A read-write mount that is not `no_exec` gets no exec deny, and a read-only one gets no
/// write grant.
///
/// Catches exec denies on every mount and write grants on read-only mounts.
#[test]
fn only_the_write_mount_is_writable_and_only_the_no_exec_mount_is_exec_denied() {
    let policy = Policy::base(mounted(), caps(Net::Off)).unwrap();
    let text = text_of(&policy);
    assert_eq!(
        text.matches("(allow file-write* (subpath (param \"GR_")
            .count(),
        1
    );
    assert!(!text.contains("(allow file-write* (subpath (param \"GR_0\")))"));
    assert_eq!(
        text.matches("(deny process-exec* (subpath (param \"GR_")
            .count(),
        1
    );
    assert!(!text.contains("(deny process-exec* (subpath (param \"GR_1\")))"));
}

/// More mounts than [`MAX_LISTED`] are refused, and exactly that many are not.
///
/// Catches a mount list that is unbounded.
#[test]
fn a_layout_with_more_mounts_than_the_bound_is_refused() {
    let many = |n: usize| -> Vec<MountGrant> {
        (0..n)
            .map(|i| mount(&format!("/Volumes/m/{i}"), false, true))
            .collect()
    };
    let at_bound = SlotLayout {
        mounts: many(MAX_LISTED),
        ..layout()
    };
    assert!(Policy::base(at_bound, caps(Net::Off)).is_ok());
    let over = SlotLayout {
        mounts: many(MAX_LISTED + 1),
        ..layout()
    };
    assert_eq!(
        Policy::base(over, caps(Net::Off)),
        Err(PolicyError::TooMany("mounts"))
    );
}
