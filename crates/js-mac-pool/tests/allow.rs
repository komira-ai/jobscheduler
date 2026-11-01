//! The allow-list read model: goldens with two trees, the order of the sections, the
//! grants and what refuses them, and the invariants of the base sets.

mod common;

use common::trace::{Kind, Mark, Row, parse};
use common::{Scratch, assurance, assurance_for, layout, path, tree, xcode_host};
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
fn a_macos_15_profile_without_grants_matches_its_golden() {
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
        .allowing(&assurance_for(MacosMajor::Macos15), vec![])
        .unwrap();
    assert_eq!(
        text_of(&policy),
        include_str!("goldens/allow_macos15_bare.sb")
    );
}

/// The macOS 15 base set is what the trace on the hosted macOS 15.8.2 image (build
/// 24H232) produced, written out here line by line: a line enters `MACOS_15` only
/// together with its row here, and the reviewer reads this table against the trace.
#[test]
fn the_macos_15_base_set_is_what_the_trace_produced() {
    let base = MacosMajor::Macos15.base();
    assert_eq!(base.names(), ["/etc", "/usr", "/var"]);
    assert_eq!(
        base.reads(),
        [
            (Match::Literal, "/"),
            (Match::Subpath, "/System/Library"),
            (Match::Subpath, "/System/Volumes/Preboot/Cryptexes"),
            (Match::Subpath, "/usr/lib"),
            (Match::Subpath, "/usr/bin"),
            (Match::Subpath, "/usr/share"),
            (Match::Subpath, "/bin"),
            (Match::Subpath, "/private/var/db/timezone"),
            (Match::Literal, "/dev/null"),
            (Match::Literal, "/dev/zero"),
            (Match::Literal, "/dev/random"),
            (Match::Literal, "/dev/urandom"),
            (Match::Literal, "/dev/tty"),
        ]
    );
    assert_eq!(
        base.setuid(),
        [
            "/System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/MacOS/ARDAgent",
            "/bin/ps",
            "/usr/bin/at",
            "/usr/bin/atq",
            "/usr/bin/atrm",
            "/usr/bin/batch",
            "/usr/bin/crontab",
            "/usr/bin/login",
            "/usr/bin/newgrp",
            "/usr/bin/quota",
            "/usr/bin/su",
            "/usr/bin/sudo",
            "/usr/bin/top",
            "/usr/bin/write",
        ]
    );
}

/// A newer major version is reviewed against the older one: it never reads a place the
/// reviewed macOS 14 set does not, and denies at least the setuid files it grants a
/// directory of.
#[test]
fn the_macos_15_base_set_reads_nothing_the_macos_14_set_does_not() {
    let old = MacosMajor::Macos14.base();
    let new = MacosMajor::Macos15.base();
    for read in new.reads() {
        assert!(old.reads().contains(read), "{read:?}");
    }
    for name in new.names() {
        assert!(old.names().contains(name), "{name}");
    }
    for file in new.setuid() {
        assert!(old.setuid().contains(file), "{file}");
    }
    let granted: Vec<&str> = new
        .reads()
        .iter()
        .filter(|(how, _)| *how == Match::Subpath)
        .map(|(_, path)| *path)
        .collect();
    for file in old.setuid() {
        let under = granted
            .iter()
            .any(|dir| file.starts_with(&format!("{dir}/")));
        assert_eq!(new.setuid().contains(file), under, "{file}");
    }
}

#[test]
fn a_macos_27_profile_without_grants_matches_its_golden() {
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
        .allowing(&assurance_for(MacosMajor::Macos27), vec![])
        .unwrap();
    assert_eq!(
        text_of(&policy),
        include_str!("goldens/allow_macos27_bare.sb")
    );
}

/// The table of the trace on macOS 27.0.1 (build 26A434, arm64, Xcode 27.1), committed as
/// the trace wrote it.
const TRACE_MACOS_27: &str = include_str!("goldens/trace_macos27.tsv");

fn trace_rows() -> Vec<Row> {
    parse(TRACE_MACOS_27).expect("the table is in the table format")
}

/// A table line (`literal /usr`, `subpath /bin`) as the base set spells it.
fn table_line(line: &str) -> (Match, String) {
    let (how, path) = line.split_once(' ').expect("a match and a path");
    let how = match how {
        "literal" => Match::Literal,
        "subpath" => Match::Subpath,
        other => panic!("unknown match {other:?}"),
    };
    (how, path.to_owned())
}

/// A line is in a set only when a workload touched it or failed without it.
fn kept(row: &Row) -> bool {
    row.touched == Mark::Yes || row.needed == Mark::Yes
}

fn is_line(row: &Row) -> bool {
    matches!(row.kind, Kind::Name | Kind::Read)
}

/// The macOS 27 base set is what the trace produced: every name and read line that a
/// workload touched or failed without, in the table's order, and the setuid files the
/// traced image listed under the directories the set grants.
#[test]
fn the_macos_27_base_set_is_what_the_trace_produced() {
    for header in [
        "# product-version=27.0.1\n",
        "# build-version=26A434\n",
        "# arch=arm64\n",
    ] {
        assert!(TRACE_MACOS_27.contains(header), "{header:?}");
    }
    let rows = trace_rows();
    let base = MacosMajor::Macos27.base();

    let names: Vec<String> = rows
        .iter()
        .filter(|row| row.kind == Kind::Name && kept(row))
        .map(|row| table_line(&row.line).1)
        .collect();
    assert_eq!(base.names(), names);

    let reads: Vec<(Match, String)> = rows
        .iter()
        .filter(|row| row.kind == Kind::Read && kept(row))
        .map(|row| table_line(&row.line))
        .collect();
    let have: Vec<(Match, String)> = base
        .reads()
        .iter()
        .map(|(how, path)| (*how, (*path).to_owned()))
        .collect();
    assert!(reads.len() > 5);
    assert_eq!(have, reads);

    let setuid: Vec<&str> = rows
        .iter()
        .filter(|row| row.kind == Kind::Setuid)
        .map(|row| row.line.as_str())
        .collect();
    assert_eq!(base.setuid(), setuid);
}

/// A line no workload touched and none failed without is not in the set, whatever an
/// older set held.
#[test]
fn the_macos_27_base_set_leaves_out_every_line_the_trace_found_unused() {
    let base = MacosMajor::Macos27.base();
    let rows = trace_rows();
    let unused: Vec<&Row> = rows
        .iter()
        .filter(|row| is_line(row) && !kept(row))
        .collect();
    assert!(!unused.is_empty());
    for row in unused {
        let (how, path) = table_line(&row.line);
        let listed = match row.kind {
            Kind::Name => base.names().contains(&path.as_str()),
            _ => base.reads().contains(&(how, path.as_str())),
        };
        assert!(!listed, "{}", row.line);
    }
}

/// The trace ran clean: every workload and control worked with the whole set, every
/// denied read was refused, and removing a line changed no denial.
#[test]
fn the_macos_27_trace_shows_every_workload_working_and_every_denial_holding() {
    let rows = trace_rows();
    let baselines: Vec<&Row> = rows
        .iter()
        .filter(|row| row.kind == Kind::Baseline)
        .collect();
    assert!(baselines.len() > 30);
    for row in baselines {
        let want = if row.line.starts_with("denied:") {
            "refused"
        } else {
            "success"
        };
        assert_eq!(row.workloads, [want], "{}", row.line);
    }
    for row in rows.iter().filter(|row| is_line(row)) {
        let expected_changes = row.line == "literal /";
        assert_eq!(
            row.denial_changed.is_empty(),
            !expected_changes,
            "{}",
            row.line
        );
        assert_ne!(row.touched, Mark::Unknown, "{}", row.line);
    }
}

/// A newer major version never reads a place the reviewed macOS 15 set does not, and it
/// denies exactly the setuid files of the traced image that lie under a directory it
/// grants: the listed ones and the ones the trace found new, which lie in directories
/// the set does not grant.
#[test]
fn the_macos_27_base_set_reads_nothing_the_macos_15_set_does_not() {
    let old = MacosMajor::Macos15.base();
    let new = MacosMajor::Macos27.base();
    for read in new.reads() {
        assert!(old.reads().contains(read), "{read:?}");
    }
    for name in new.names() {
        assert!(old.names().contains(name), "{name}");
    }
    let granted: Vec<&str> = new
        .reads()
        .iter()
        .filter(|(how, _)| *how == Match::Subpath)
        .map(|(_, path)| *path)
        .collect();
    let found: Vec<Row> = trace_rows()
        .into_iter()
        .filter(|row| matches!(row.kind, Kind::Setuid | Kind::SetuidNew))
        .collect();
    assert!(found.len() > 15);
    for row in found {
        let under = granted
            .iter()
            .any(|dir| row.line.starts_with(&format!("{dir}/")));
        assert_eq!(
            new.setuid().contains(&row.line.as_str()),
            under,
            "{}",
            row.line
        );
        assert_eq!(row.kind == Kind::Setuid, under, "{}", row.line);
    }
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
    assert!(
        at("(allow file-read-metadata (literal \"/System\")")
            < at("(allow file-read* (literal \"/\")")
    );
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
                subpaths.push(*text);
            }
        }
    }
    for major in MacosMajor::ALL {
        for (how, text) in major.base().reads() {
            if *how == Match::Subpath {
                subpaths.push(*text);
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
        all.extend(base.names());
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
        ("15.0", Some(MacosMajor::Macos15)),
        ("15.8.2", Some(MacosMajor::Macos15)),
        ("27.0.1", Some(MacosMajor::Macos27)),
        ("16.0", None),
        ("26.0", None),
        ("13.6", None),
        ("fourteen", None),
        ("", None),
    ];
    for (text, expected) in table {
        assert_eq!(MacosMajor::from_product_version(text), expected, "{text:?}");
    }
    assert_eq!(MacosMajor::from_major(14), Some(MacosMajor::Macos14));
    assert_eq!(MacosMajor::from_major(15), Some(MacosMajor::Macos15));
    assert_eq!(MacosMajor::from_major(27), Some(MacosMajor::Macos27));
    assert_eq!(MacosMajor::Macos14.major(), 14);
    assert_eq!(MacosMajor::Macos15.major(), 15);
    assert_eq!(MacosMajor::Macos27.major(), 27);
}

/// A major version without a reviewed base set has no value, so no allow profile of it
/// can be built: the majors between the reviewed ones (16 to 26), the one after the
/// newest (28) and the older ones fail closed.
#[test]
fn a_major_version_without_a_base_set_fails_closed() {
    for major in [0, 1, 10, 11, 12, 13, 16, 17, 25, 26, 28, u32::MAX] {
        assert_eq!(MacosMajor::from_major(major), None, "{major}");
        assert_eq!(
            MacosMajor::from_product_version(&format!("{major}.0.1")),
            None,
            "{major}"
        );
    }
    let known: Vec<u32> = MacosMajor::ALL.iter().map(|m| m.major()).collect();
    assert_eq!(known, [14, 15, 27]);
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
