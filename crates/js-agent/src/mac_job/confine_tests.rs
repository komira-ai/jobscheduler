//! The profile of a native job: its inputs map to the builder's policy, and nothing else.

use std::path::{Path, PathBuf};

use js_mac_pool::seatbelt::{
    BuildError, InitError, PathError, PolicyError, PortsError, TreesError,
};

use super::{ConfineError, Confinement, confine, policy};
use crate::mac_job::spec::Network;

const ROOT: &str = "/Volumes/store/fs";
const TREE_A: &str = "/Volumes/store/fs/mount-provider/aa/r1/0";
const TREE_B: &str = "/Volumes/store/fs/mount-provider/aa/r1/1";
const PUT: &str = "/Volumes/store/put/cas/b/build1/0011223344556677";
const JOB: &str = "/private/var/js-mac-job/jobs/j1";

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

fn net(loopback: bool, outbound_ports: &[u16]) -> Network {
    Network {
        loopback,
        outbound_ports: outbound_ports.to_vec(),
    }
}

/// The text and the parameters of the profile of a job in `JOB`, with `trees` below `ROOT`
/// when any, `puts`, and `network`.
fn profile_of(trees: &[&str], puts: &[&str], network: &Network) -> (String, Vec<(String, String)>) {
    let trees = paths(trees);
    let puts = paths(puts);
    let job = Confinement {
        folder: Path::new(JOB),
        mount: (!trees.is_empty()).then_some((Path::new(ROOT), trees.as_slice())),
        put_dirs: &puts,
        network,
    };
    let confined = confine(&job).expect("a profile");
    (
        confined.profile.text().to_owned(),
        confined.profile.params().to_vec(),
    )
}

fn params(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// Asserts that a job with two trees below the mount root, given out of order, has the
/// builder's whole text for it: the mount denied, each tree read in path order, the job
/// folder the one writable place.
#[test]
fn a_job_with_two_trees_gets_the_builders_text_with_the_trees_in_path_order() {
    let (text, got) = profile_of(&[TREE_B, TREE_A], &[], &net(false, &[]));
    assert_eq!(
        text,
        include_str!("../../../js-mac-pool/tests/goldens/job_trees.sb")
    );
    assert_eq!(
        got,
        params(&[
            ("RUN", JOB),
            ("MOUNT", ROOT),
            ("MT_0", TREE_A),
            ("MT_1", TREE_B)
        ])
    );
}

/// Asserts that a put directory is granted for writing, and that nothing else is writable
/// but the job folder and the two device paths.
#[test]
fn a_put_directory_of_the_job_is_writable_and_nothing_else_is() {
    let (text, got) = profile_of(&[], &[PUT], &net(false, &[]));
    assert_eq!(
        text,
        include_str!("../../../js-mac-pool/tests/goldens/job_put.sb")
    );
    assert_eq!(got, params(&[("RUN", JOB), ("GR_0", PUT)]));
    let writes: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("(allow file-write*"))
        .collect();
    assert_eq!(
        writes,
        [
            "(allow file-write* (subpath (param \"RUN\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))",
            "(allow file-write* (subpath (param \"GR_0\")))",
        ]
    );
}

/// Asserts that outbound ports give loopback, TCP to each port on any address and the
/// resolver's socket, in the builder's text.
#[test]
fn outbound_ports_give_the_remote_level_with_name_lookups() {
    let (text, got) = profile_of(&[], &[], &net(false, &[8980, 443]));
    assert_eq!(
        text,
        include_str!("../../../js-mac-pool/tests/goldens/job_remote.sb")
    );
    assert_eq!(got, params(&[("RUN", JOB)]));
}

/// Asserts the text of a job that has trees, a put directory and outbound ports at once.
#[test]
fn a_job_with_trees_a_put_directory_and_ports_gets_the_builders_whole_text() {
    let (text, got) = profile_of(&[TREE_A, TREE_B], &[PUT], &net(true, &[443, 8980]));
    assert_eq!(
        text,
        include_str!("../../../js-mac-pool/tests/goldens/job_all.sb")
    );
    assert_eq!(
        got,
        params(&[
            ("RUN", JOB),
            ("MOUNT", ROOT),
            ("MT_0", TREE_A),
            ("MT_1", TREE_B),
            ("GR_0", PUT),
        ])
    );
}

/// Asserts the network rules of each request: none is no network rule (and no resolver
/// socket), loopback is the loopback rules alone, and ports are loopback, each port and the
/// resolver socket.
#[test]
fn each_network_request_gets_its_own_rules() {
    let rules = |network: &Network| -> Vec<String> {
        let (text, _) = profile_of(&[], &[], network);
        text.lines()
            .filter(|l| l.contains("network") || l.contains("dnssd"))
            .map(str::to_owned)
            .collect()
    };
    let floor = [
        "(deny network*)",
        "(allow network-outbound (literal \"/private/var/run/syslog\"))",
    ];
    let off = rules(&net(false, &[]));
    assert_eq!(&off[..2], floor);
    assert!(
        !off.iter()
            .any(|l| l.contains("localhost") || l.contains("remote tcp"))
    );
    assert!(!off.iter().any(|l| l.contains("mDNSResponder")));

    let loopback = rules(&net(true, &[]));
    assert_eq!(&loopback[..2], floor);
    assert!(
        loopback
            .iter()
            .any(|l| l.contains("local ip \"localhost:*\""))
    );
    assert!(!loopback.iter().any(|l| l.contains("remote tcp")));
    assert!(!loopback.iter().any(|l| l.contains("mDNSResponder")));

    for with_loopback in [false, true] {
        let remote = rules(&net(with_loopback, &[8980, 443]));
        for want in [
            "(allow network-outbound (remote tcp \"*:443\"))",
            "(allow network-outbound (remote tcp \"*:8980\"))",
            "(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))",
            "(allow network* (remote ip \"localhost:*\"))",
        ] {
            assert!(remote.iter().any(|l| l == want), "{with_loopback}: {want}");
        }
    }
}

/// Asserts that the job folder is the only writable place of the run and that no per-user
/// folder grant is made.
#[test]
fn the_profile_names_no_per_user_folder() {
    let (text, got) = profile_of(&[], &[], &net(false, &[]));
    assert!(!text.contains("UENTRY"));
    assert!(!text.contains("/var/folders"));
    assert_eq!(got, params(&[("RUN", JOB)]));
}

/// Asserts that a tree that is not strictly below the mount root is refused with its path
/// in the message, whichever way it fails: outside the root, a sibling that shares its
/// prefix, or the root itself.
#[test]
fn a_tree_not_strictly_below_the_mount_root_is_refused_naming_it() {
    for (tree, want) in [
        (
            "/other/t",
            "the tree /other/t is not below the mount root /Volumes/store/fs",
        ),
        (
            "/Volumes/store/fsx/t",
            "the tree /Volumes/store/fsx/t is not below the mount root /Volumes/store/fs",
        ),
        (ROOT, "the tree /Volumes/store/fs is the mount root itself"),
    ] {
        let trees = paths(&[TREE_A, tree]);
        let job = Confinement {
            folder: Path::new(JOB),
            mount: Some((Path::new(ROOT), &trees)),
            put_dirs: &[],
            network: &net(false, &[]),
        };
        let why = confine(&job).expect_err(tree);
        assert!(matches!(why, ConfineError::Trees(_)), "{why:?}");
        assert_eq!(why.to_string(), want);
    }
}

/// Asserts that a put directory over the trees' mount root is refused: its read grant would
/// reopen what the trees' rule denies.
#[test]
fn a_put_directory_over_the_mount_root_is_refused() {
    let trees = paths(&[TREE_A]);
    let puts = paths(&["/Volumes/store/fs/put"]);
    let job = Confinement {
        folder: Path::new(JOB),
        mount: Some((Path::new(ROOT), &trees)),
        put_dirs: &puts,
        network: &net(false, &[]),
    };
    assert_eq!(
        confine(&job).expect_err("overlap"),
        ConfineError::Policy(PolicyError::MountOverTrees)
    );
}

/// Asserts that a NUL in any path of the job, and a path the builder refuses for another
/// reason, is refused by name before a profile exists: nothing has been forked, so nothing
/// is left to clean up, and the message spells the NUL out.
#[test]
fn a_nul_in_any_path_is_refused_before_anything_is_started() {
    let trees = paths(&[TREE_A]);
    let bad_tree = paths(&["/Volumes/store/fs/a\0b"]);
    let bad_put = paths(&["/put/a\0b"]);
    let good_put = paths(&[PUT]);
    let off = net(false, &[]);
    let cases = [
        (
            Confinement {
                folder: Path::new("/jobs/j\01"),
                mount: None,
                put_dirs: &[],
                network: &off,
            },
            "the job folder \"/jobs/j\\\\01\": path contains a control character",
        ),
        (
            Confinement {
                folder: Path::new(JOB),
                mount: Some((Path::new("/Volumes/a\0b"), &trees)),
                put_dirs: &[],
                network: &off,
            },
            "the mount root \"/Volumes/a\\\\0b\": path contains a control character",
        ),
        (
            Confinement {
                folder: Path::new(JOB),
                mount: Some((Path::new(ROOT), &bad_tree)),
                put_dirs: &[],
                network: &off,
            },
            "a tree \"/Volumes/store/fs/a\\\\0b\": path contains a control character",
        ),
        (
            Confinement {
                folder: Path::new(JOB),
                mount: None,
                put_dirs: &bad_put,
                network: &off,
            },
            "a put directory \"/put/a\\\\0b\": path contains a control character",
        ),
        (
            Confinement {
                folder: Path::new("jobs/j1"),
                mount: None,
                put_dirs: &good_put,
                network: &off,
            },
            "the job folder \"jobs/j1\": path is not absolute",
        ),
    ];
    for (job, want) in cases {
        let why = confine(&job).expect_err(want);
        assert!(matches!(why, ConfineError::Path { .. }), "{why:?}");
        assert_eq!(why.to_string(), want);
    }
}

/// Asserts that a port list the builder refuses is refused with its reason, and that port
/// lists of one port and of repeated ports are taken.
#[test]
fn outbound_ports_the_builder_refuses_are_refused() {
    let job = |ports: &[u16]| {
        policy(&Confinement {
            folder: Path::new(JOB),
            mount: None,
            put_dirs: &[],
            network: &net(false, ports),
        })
    };
    assert_eq!(
        job(&[0]).expect_err("zero"),
        ConfineError::Ports(PortsError::Zero)
    );
    assert!(job(&[443, 443]).is_ok());
    let too_many: Vec<u16> = (1..=65).collect();
    assert_eq!(
        job(&too_many).expect_err("many").to_string(),
        "outbound ports: too many ports listed"
    );
}

/// Asserts what each error says and that each of the builder's errors converts into one.
#[test]
fn every_builders_error_is_one_of_the_confine_errors() {
    let from: [(ConfineError, &str); 6] = [
        (TreesError::TooMany.into(), "too many trees"),
        (PortsError::Empty.into(), "outbound ports: no ports listed"),
        (
            PolicyError::TreesNeedOpenModel.into(),
            "the policy: trees on a mount need the open read model",
        ),
        (
            BuildError::AllowWithoutGrants.into(),
            "the profile: the allow read model needs its grants (Policy::allowing)",
        ),
        (
            InitError::TextNul.into(),
            "the profile: the profile text holds a NUL byte",
        ),
        (
            ConfineError::Path {
                what: "a tree",
                path: "x".to_owned(),
                why: PathError::NotAbsolute,
            },
            "a tree \"x\": path is not absolute",
        ),
    ];
    for (error, want) in from {
        assert_eq!(error.to_string(), want);
    }
}
