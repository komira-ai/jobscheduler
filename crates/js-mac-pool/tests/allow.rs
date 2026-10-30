//! The allow-list read model: goldens with two trees, the order of the sections, the
//! grants and what refuses them, and the invariants of the base sets.

mod common;

use common::{Scratch, assurance, layout, path, tree, xcode_host};
use js_bundle::ReadGrantKind;
use js_mac_pool::seatbelt::{
    AbsPath, BuildError, Caps, MAX_LISTED, MacosMajor, Match, Narrowing, Net, PathError,
    PinnedTree, Policy, PolicyError, Profile, ReadGrant, ReadModel, SlotLayout, is_directory_wide,
    kind_rules,
};

const STORE: &str = "/Volumes/work/store";

fn allow_caps(net: Net) -> Caps {
    Caps {
        net,
        read_model: ReadModel::Allow,
    }
}

fn text_of(policy: &Policy) -> String {
    Profile::for_policy(policy)
        .expect("a profile")
        .text()
        .to_owned()
}

fn two_tree_grants(scratch: &Scratch) -> Vec<ReadGrant> {
    vec![
        ReadGrant::Tree(tree(STORE, 'b')),
        ReadGrant::Tree(tree(STORE, 'a')),
        ReadGrant::Xcode(xcode_host(scratch, "Xcode_15.4.app")),
        ReadGrant::Kind(ReadGrantKind::SystemCerts),
        ReadGrant::Kind(ReadGrantKind::SysctlInfo),
        ReadGrant::Kind(ReadGrantKind::Timezone),
        ReadGrant::Kind(ReadGrantKind::SystemCerts),
    ]
}

#[test]
fn a_profile_with_two_trees_an_xcode_and_kinds_matches_its_golden() {
    let scratch = Scratch::new("allow-two");
    let policy = Policy::base(layout(), allow_caps(Net::Off))
        .unwrap()
        .allowing(&assurance(), two_tree_grants(&scratch))
        .unwrap();
    assert_eq!(
        text_of(&policy),
        include_str!("goldens/allow_macos14_two_trees.sb")
    );
}

#[test]
fn every_granted_path_is_a_named_parameter_in_a_fixed_order() {
    let scratch = Scratch::new("allow-params");
    let policy = Policy::base(layout(), allow_caps(Net::Off))
        .unwrap()
        .allowing(&assurance(), two_tree_grants(&scratch))
        .unwrap();
    let profile = Profile::for_policy(&policy).unwrap();
    let a = format!("{STORE}/v1/objects/trees/{}", "a".repeat(64));
    let b = format!("{STORE}/v1/objects/trees/{}", "b".repeat(64));
    let app = scratch.abs("Xcode_15.4.app");
    let expected: Vec<(String, String)> = [
        ("RUN", "/Volumes/work/slot1/run"),
        ("UENTRY", "/private/var/folders/ab/cdef0123"),
        ("TREE_0", a.as_str()),
        ("TREE_1", b.as_str()),
        ("XCODE_APP", app.as_str()),
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
fn a_profile_without_grants_the_user_folder_or_lists_matches_its_golden() {
    let bare = SlotLayout {
        inputs: vec![],
        read_denies: vec![],
        ..layout()
    };
    let policy = Policy::base(bare, allow_caps(Net::Loopback))
        .unwrap()
        .narrowed(Narrowing {
            drop_uid_entry: true,
            ..Narrowing::default()
        })
        .unwrap()
        .allowing(&assurance(), vec![])
        .unwrap();
    assert_eq!(
        text_of(&policy),
        include_str!("goldens/allow_macos14_bare.sb")
    );
}

#[test]
fn an_allow_policy_without_its_grants_is_refused_not_run_as_open() {
    let policy = Policy::base(layout(), allow_caps(Net::Off)).unwrap();
    assert_eq!(policy.read_model(), ReadModel::Allow);
    assert_eq!(
        Profile::for_policy(&policy),
        Err(BuildError::AllowWithoutGrants)
    );
    assert_eq!(
        BuildError::AllowWithoutGrants.to_string(),
        "the allow read model needs its grants (Policy::allowing)"
    );
}

#[test]
fn grants_are_refused_when_they_do_not_fit_the_policy() {
    let scratch = Scratch::new("allow-refused");
    let open = Policy::base(
        layout(),
        Caps {
            net: Net::Off,
            read_model: ReadModel::Open,
        },
    )
    .unwrap();
    assert_eq!(
        open.allowing(&assurance(), vec![]),
        Err(PolicyError::NotAllowModel)
    );

    let granted = Policy::base(layout(), allow_caps(Net::Off))
        .unwrap()
        .allowing(&assurance(), vec![])
        .unwrap();
    assert_eq!(
        granted.allowing(&assurance(), vec![]),
        Err(PolicyError::AlreadyGranted)
    );

    let two = vec![
        ReadGrant::Xcode(xcode_host(&scratch, "Xcode_15.4.app")),
        ReadGrant::Xcode(xcode_host(&scratch, "Xcode_15.3.app")),
    ];
    let policy = Policy::base(layout(), allow_caps(Net::Off)).unwrap();
    assert_eq!(
        policy.allowing(&assurance(), two),
        Err(PolicyError::TwoXcodes)
    );

    let many: Vec<ReadGrant> = (0..=MAX_LISTED)
        .map(|i| {
            let id = format!("sha256:{:064x}", i);
            let id = js_bundle::DiffId::parse(&id).expect("a diff id");
            ReadGrant::Tree(PinnedTree::new(&path(STORE), &id).expect("a tree"))
        })
        .collect();
    assert_eq!(many.len(), MAX_LISTED + 1);
    let policy = Policy::base(layout(), allow_caps(Net::Off)).unwrap();
    assert_eq!(
        policy.allowing(&assurance(), many),
        Err(PolicyError::TooMany("trees"))
    );
}

#[test]
fn the_same_tree_and_kind_granted_twice_is_granted_once() {
    let once = Policy::base(layout(), allow_caps(Net::Off))
        .unwrap()
        .allowing(
            &assurance(),
            vec![
                ReadGrant::Tree(tree(STORE, 'a')),
                ReadGrant::Kind(ReadGrantKind::Locale),
            ],
        )
        .unwrap();
    let twice = Policy::base(layout(), allow_caps(Net::Off))
        .unwrap()
        .allowing(
            &assurance(),
            vec![
                ReadGrant::Tree(tree(STORE, 'a')),
                ReadGrant::Tree(tree(STORE, 'a')),
                ReadGrant::Kind(ReadGrantKind::Locale),
                ReadGrant::Kind(ReadGrantKind::Locale),
            ],
        )
        .unwrap();
    assert_eq!(text_of(&once), text_of(&twice));
    assert_eq!(
        Profile::for_policy(&once).unwrap().params(),
        Profile::for_policy(&twice).unwrap().params()
    );
}

/// A deny added by a narrowing comes after every grant, so it wins over them; the setuid
/// denies and the floor are after the grants as well.
#[test]
fn denies_come_after_every_grant_and_the_floor_is_last() {
    let scratch = Scratch::new("allow-order");
    let policy = Policy::base(layout(), allow_caps(Net::Off))
        .unwrap()
        .allowing(&assurance(), two_tree_grants(&scratch))
        .unwrap()
        .narrowed(Narrowing {
            deny_read: vec![path("/Volumes/work/store-private")],
            ..Narrowing::default()
        })
        .unwrap();
    let text = text_of(&policy);
    let lines: Vec<&str> = text.lines().collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no line with {needle}"))
    };
    let last_grant = lines
        .iter()
        .rposition(|line| line.starts_with("(allow file-read*"))
        .expect("grants");
    assert!(at("(deny file-read*)") < at("(allow file-read-metadata"));
    assert!(at("(allow file-read-metadata") < at("(allow file-read* (literal \"/\")"));
    assert!(last_grant < at("(deny file-read* (literal"));
    assert!(at("(deny file-read* (literal") < at("(param \"RD_0\")"));
    assert!(at("(param \"RD_2\")") < at("(deny network*)"));
    assert!(text.ends_with("(deny network-outbound (literal \"/private/var/run/usbmuxd\"))\n"));
}

#[test]
fn paths_that_look_like_profile_syntax_never_enter_an_allow_profile() {
    let hostile = "/Volumes/x\")) (allow file-read* (subpath \"/\")) ((\"";
    let scratch = Scratch::new("allow-hostile");
    let store = AbsPath::parse(hostile).expect("a parameter value is never profile text");
    let evil = PinnedTree::new(&store, &common::diff_id('c')).expect("a tree");
    let plain = PinnedTree::new(&path(STORE), &common::diff_id('c')).expect("a tree");
    let policy = |tree: PinnedTree| {
        Policy::base(layout(), allow_caps(Net::On))
            .unwrap()
            .allowing(
                &assurance(),
                vec![
                    ReadGrant::Tree(tree),
                    ReadGrant::Xcode(xcode_host(&scratch, "Xcode_15.4.app")),
                ],
            )
            .unwrap()
    };
    let hostile_text = text_of(&policy(evil));
    assert_eq!(hostile_text, text_of(&policy(plain)));
    assert!(!hostile_text.contains("Volumes/x"));
}

#[test]
fn a_tree_grant_names_one_store_tree() {
    let grant = tree(STORE, 'a');
    assert_eq!(
        grant.path().as_str(),
        format!("{STORE}/v1/objects/trees/{}", "a".repeat(64))
    );
    assert_eq!(grant.diff_id(), &common::diff_id('a'));
    let long = AbsPath::parse(&format!("/{}", "r".repeat(1000))).expect("long, but a path");
    assert_eq!(
        PinnedTree::new(&long, &common::diff_id('a')),
        Err(PathError::TooLong)
    );
}

#[test]
fn directory_wide_paths_are_named_and_the_literal_files_are_not() {
    let wide = [
        "/",
        "",
        "/System",
        "/System/Volumes/Data",
        "/Library",
        "/Library/",
        "/Library/Apple",
        "/Library/Preferences",
        "/Users",
        "/Users/Shared",
        "/Volumes",
        "/opt",
        "/opt/homebrew",
        "/private",
        "/private/etc",
        "/private/tmp",
        "/private/var",
        "/private/var/db",
        "/private/var/tmp",
    ];
    for text in wide {
        assert!(is_directory_wide(text), "{text:?}");
    }
    let narrow = [
        "/System/Library",
        "/private/var/db/dyld",
        "/private/var/db/timezone",
        "/usr/bin",
        "/Library/Preferences/com.apple.dt.Xcode.plist",
        "/private/etc/ssl",
    ];
    for text in narrow {
        assert!(!is_directory_wide(text), "{text:?}");
    }
}

#[test]
fn no_base_set_or_kind_grants_a_directory_wide_path() {
    let mut subpaths: Vec<&str> = Vec::new();
    for kind in js_bundle::ReadGrantKind::ALL {
        for (how, text) in kind_rules(kind) {
            if *how == Match::Subpath {
                subpaths.push(text);
            }
        }
    }
    for major in MacosMajor::ALL {
        for (how, text) in major.base().reads() {
            if *how == Match::Subpath {
                subpaths.push(text);
            }
        }
    }
    assert!(subpaths.len() > 10);
    for text in subpaths {
        assert!(!is_directory_wide(text), "{text}");
    }
}

/// The constants are profile text, not parameters, so each must be a plain absolute path
/// that cannot end a string, close a form or start a comment.
#[test]
fn every_path_constant_is_plain_absolute_text() {
    let mut all: Vec<&str> = Vec::new();
    for kind in js_bundle::ReadGrantKind::ALL {
        all.extend(kind_rules(kind).iter().map(|(_, text)| *text));
    }
    for major in MacosMajor::ALL {
        let base = major.base();
        all.extend(base.reads().iter().map(|(_, text)| *text));
        all.extend(base.setuid());
        assert!(
            base.setuid()
                .contains(&js_mac_pool::seatbelt::SETUID_CANARY)
        );
    }
    for text in all {
        assert!(text.starts_with('/'), "{text}");
        let plain = text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'));
        assert!(plain, "{text}");
        assert!(!text.contains("//"), "{text}");
    }
}

#[test]
fn kinds_map_to_fixed_files_and_sysctl_info_has_none() {
    for kind in ReadGrantKind::ALL {
        let rules = kind_rules(kind);
        assert_eq!(
            rules.is_empty(),
            kind == ReadGrantKind::SysctlInfo,
            "{kind:?}"
        );
    }
}

#[test]
fn macos_versions_are_read_from_a_product_version() {
    let table = [
        ("14.5", Some(MacosMajor::Macos14)),
        ("14", Some(MacosMajor::Macos14)),
        (" 14.5.1\n", Some(MacosMajor::Macos14)),
        ("15.0", None),
        ("13.6", None),
        ("fourteen", None),
        ("", None),
    ];
    for (text, expected) in table {
        assert_eq!(MacosMajor::from_product_version(text), expected, "{text:?}");
    }
    assert_eq!(MacosMajor::from_major(14), Some(MacosMajor::Macos14));
    assert_eq!(MacosMajor::from_major(26), None);
    assert_eq!(MacosMajor::Macos14.major(), 14);
}

#[test]
fn every_policy_error_says_what_is_wrong() {
    let table = [
        (PolicyError::TooMany("trees"), "too many entries in trees"),
        (
            PolicyError::NotAllowModel,
            "read grants need the allow read model",
        ),
        (
            PolicyError::AlreadyGranted,
            "the policy already has its read grants",
        ),
        (PolicyError::TwoXcodes, "more than one Xcode granted"),
    ];
    for (error, text) in table {
        assert_eq!(error.to_string(), text);
    }
}
