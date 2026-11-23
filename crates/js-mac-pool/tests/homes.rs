//! The homes' read denies of a narrowing: every read below a homes root is denied but the
//! run directory's, which lies below one, in every job shape and both read models; what a
//! later grant names stays open and what a read deny names stays closed; and what refuses
//! a homes root.

mod common;

use common::{assurance, layout};
use js_mac_pool::seatbelt::{
    AbsPath, Caps, MAX_LISTED, MountGrant, MountTrees, Narrowing, Net, Policy, PolicyError, Ports,
    Profile, ReadModel, SlotLayout,
};

const RUN: &str = "/Users/kbf-acct-1/exec";
const HOMES: [&str; 4] = [
    "/Users",
    "/System/Volumes/Data/Users",
    "/private/var/root",
    "/System/Volumes/Data/private/var/root",
];
const ROOT: &str = "/Volumes/store/fs";
const TREE: &str = "/Volumes/store/fs/mount-provider/aa/r1/0";
/// A mount below a homes root: an operator's grant that opens part of it.
const MOUNTED: &str = "/Users/Shared/cache";
/// A read deny inside the run directory.
const SECRET: &str = "/Users/kbf-acct-1/exec/.state";

/// The run's own grant back, in the order it follows the homes' denies.
const RUN_READ: &str = "(allow file-read* (subpath (param \"RUN\")))";
const RUN_WALK: &str = "(allow file-read-metadata (path-ancestors (param \"RUN\")))";

fn path(text: &str) -> AbsPath {
    AbsPath::parse(text).expect("a good path")
}

fn homes() -> Vec<AbsPath> {
    HOMES.iter().map(|home| path(home)).collect()
}

fn caps(net: Net, read_model: ReadModel) -> Caps {
    Caps { net, read_model }
}

fn mount(at: &str) -> MountGrant {
    MountGrant {
        path: path(at),
        write: false,
        no_exec: true,
    }
}

/// A job whose run directory lies below the homes, with `trees`, `mounts` and a read deny
/// inside its run directory.
fn job(trees: Option<MountTrees>, mounts: Vec<MountGrant>) -> SlotLayout {
    SlotLayout {
        run: path(RUN),
        uid_entry: None,
        inputs: vec![],
        read_denies: vec![path(SECRET)],
        mounts,
        trees,
    }
}

fn trees() -> MountTrees {
    MountTrees::new(path(ROOT), vec![path(TREE)]).expect("trees")
}

fn with_homes(list: Vec<AbsPath>) -> Narrowing {
    Narrowing {
        deny_homes: list,
        ..Narrowing::default()
    }
}

fn profile(layout: SlotLayout, net: Net) -> Profile {
    let policy = Policy::base(layout, caps(net, ReadModel::Open))
        .and_then(|policy| policy.narrowed(with_homes(homes())))
        .expect("a policy");
    Profile::for_policy(&policy).expect("a profile")
}

fn ports(list: &[u16]) -> Ports {
    Ports::new(list).expect("ports")
}

/// The index of the line `line` in `lines`, which must hold it once.
fn at(lines: &[&str], line: &str) -> usize {
    let found: Vec<usize> = (0..lines.len()).filter(|i| lines[*i] == line).collect();
    assert_eq!(found.len(), 1, "{line:?} in\n{}", lines.join("\n"));
    found[0]
}

/// The whole text of a job with homes roots, a tree, a mount below a homes root and a read
/// deny in its run directory, and its parameters: each root after `RUN`, in path order.
///
/// Catches a homes root spelled into the text, a lost run-directory grant, and a section
/// out of its place.
#[test]
fn a_job_with_homes_roots_matches_its_golden() {
    let profile = profile(job(Some(trees()), vec![mount(MOUNTED)]), Net::Off);
    assert_eq!(profile.text(), include_str!("goldens/job_homes.sb"));
    let params: Vec<(&str, &str)> = profile
        .params()
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        params,
        [
            ("RUN", RUN),
            ("HM_0", "/System/Volumes/Data/Users"),
            ("HM_1", "/System/Volumes/Data/private/var/root"),
            ("HM_2", "/Users"),
            ("HM_3", "/private/var/root"),
            ("MOUNT", ROOT),
            ("MT_0", TREE),
            ("RD_0", SECRET),
            ("GR_0", MOUNTED),
        ]
    );
    assert!(!profile.text().contains("/Users"));
}

/// In every job shape the homes' denies come before every read grant and every read deny,
/// and the run directory's read and walk grants follow the last of them at once: a later
/// grant (a tree, a mount) opens what it names, the read deny in the run directory still
/// closes it, and nothing between the denies and the run's grant opens anything.
///
/// Catches a homes deny after a grant it would hide (a mount below `/Users` closed), a deny
/// after the run's grant (the job cannot read its own directory), and a read deny inside the
/// run directory that the run's grant reopens.
#[test]
fn the_homes_denies_precede_every_read_rule_and_the_run_is_granted_back_next() {
    let nets = [
        Net::Off,
        Net::Ports(ports(&[1])),
        Net::Loopback,
        Net::Remote(ports(&[443])),
        Net::On,
    ];
    for net in nets {
        for set in [None, Some(trees())] {
            for mounts in [vec![], vec![mount(MOUNTED)]] {
                let profile = profile(job(set.clone(), mounts), net.clone());
                let lines: Vec<&str> = profile.text().lines().collect();
                let denies: Vec<usize> = (0..HOMES.len())
                    .map(|i| {
                        at(
                            &lines,
                            &format!("(deny file-read* (subpath (param \"HM_{i}\")))"),
                        )
                    })
                    .collect();
                let last = *denies.iter().max().expect("four denies");
                assert_eq!(denies, (last + 1 - HOMES.len()..=last).collect::<Vec<_>>());
                assert_eq!(at(&lines, RUN_READ), last + 1);
                assert_eq!(at(&lines, RUN_WALK), last + 2);
                let first_read = lines
                    .iter()
                    .position(|line| {
                        line.starts_with("(allow file-read") || line.starts_with("(deny file-read")
                    })
                    .expect("a read rule");
                assert_eq!(first_read, denies[0], "{net:?}:\n{}", profile.text());
                let secret = at(&lines, "(deny file-read* (subpath (param \"RD_0\")))");
                assert!(secret > last + 2, "{net:?}:\n{}", profile.text());
            }
        }
    }
}

/// A job without homes roots has neither the denies nor the run directory's read grant, so
/// its profile is the one it had before homes roots existed (the other goldens hold it).
///
/// Catches a run read grant emitted for every policy, and a deny left over from an empty
/// list.
#[test]
fn a_job_without_homes_roots_has_no_homes_rules() {
    let policy = Policy::base(job(None, vec![]), caps(Net::Off, ReadModel::Open))
        .and_then(|policy| policy.narrowed(Narrowing::default()))
        .expect("a policy");
    let profile = Profile::for_policy(&policy).expect("a profile");
    assert!(!profile.text().contains("HM_"), "{}", profile.text());
    assert!(!profile.text().contains(RUN_READ), "{}", profile.text());
    assert!(!profile.text().contains(RUN_WALK), "{}", profile.text());
}

/// Under the allow read model the homes' rules come before its default read deny and its
/// grants, so every grant of that model still opens what it names.
///
/// Catches a homes deny placed after the allow model's grants, where it would close a
/// granted place below a homes root.
#[test]
fn under_the_allow_model_the_homes_rules_precede_its_default_deny() {
    let base = SlotLayout {
        run: path(RUN),
        ..layout()
    };
    let policy = Policy::base(base, caps(Net::Off, ReadModel::Allow))
        .and_then(|policy| policy.narrowed(with_homes(homes())))
        .and_then(|policy| policy.allowing(&assurance(), vec![]))
        .expect("a policy");
    let text = Profile::for_policy(&policy)
        .expect("a profile")
        .text()
        .to_owned();
    let lines: Vec<&str> = text.lines().collect();
    let walk = at(&lines, RUN_WALK);
    let default = at(&lines, "(deny file-read*)");
    assert_eq!(default, walk + 1, "{text}");
    assert_eq!(
        at(&lines, "(deny file-read* (subpath (param \"HM_0\")))"),
        walk - 5
    );
}

/// A homes root at the run directory or below it is refused, given at once or by a second
/// narrowing; a root beside the run directory that only begins like it is not.
///
/// Catches a homes deny that the run directory's grant, which follows it, silently undoes.
#[test]
fn a_homes_root_in_the_run_directory_is_refused() {
    let base = || Policy::base(job(None, vec![]), caps(Net::Off, ReadModel::Open));
    for root in [RUN, "/Users/kbf-acct-1/exec/.home"] {
        assert_eq!(
            base().and_then(|policy| policy.narrowed(with_homes(vec![path(root)]))),
            Err(PolicyError::HomesInRun),
            "{root}"
        );
        assert_eq!(
            base()
                .and_then(|policy| policy.narrowed(with_homes(homes())))
                .and_then(|policy| policy.narrowed(with_homes(vec![path(root)]))),
            Err(PolicyError::HomesInRun),
            "{root}"
        );
    }
    let beside = with_homes(vec![path("/Users/kbf-acct-1/execx")]);
    assert!(base().and_then(|policy| policy.narrowed(beside)).is_ok());
    assert_eq!(
        PolicyError::HomesInRun.to_string(),
        "a homes root lies in the run directory"
    );
}

/// A mount grant at or above a homes root is refused, whether the mount or the root came
/// first: its grant follows the homes' denies and would reopen every home. A mount below a
/// root, or beside it, is built.
///
/// Catches a mount of `/Users` that hands a job every home.
#[test]
fn a_mount_grant_at_or_above_a_homes_root_is_refused() {
    let narrowed = |at: &str| {
        Policy::base(job(None, vec![mount(at)]), caps(Net::Off, ReadModel::Open))
            .and_then(|policy| policy.narrowed(with_homes(homes())))
    };
    for at in [
        "/Users",
        "/System/Volumes/Data",
        "/private/var/root",
        "/private",
    ] {
        assert_eq!(narrowed(at), Err(PolicyError::MountOverReadDeny), "{at}");
    }
    for at in [
        MOUNTED,
        "/Usersx",
        "/private/var/rootx",
        "/System/Volumes/Data/Library",
    ] {
        assert!(narrowed(at).is_ok(), "{at}");
    }
}

/// The homes list is bounded like every list of a policy, and a root named twice is one.
///
/// Catches an unbounded list and a repeated rule.
#[test]
fn a_homes_list_over_the_bound_is_refused_and_repeats_count_once() {
    let many = |n: usize| -> Vec<AbsPath> {
        (0..n)
            .map(|i| path(&format!("/Volumes/homes/{i}")))
            .collect()
    };
    let base = || Policy::base(job(None, vec![]), caps(Net::Off, ReadModel::Open));
    assert!(
        base()
            .and_then(|policy| policy.narrowed(with_homes(many(MAX_LISTED))))
            .is_ok()
    );
    assert_eq!(
        base().and_then(|policy| policy.narrowed(with_homes(many(MAX_LISTED + 1)))),
        Err(PolicyError::TooMany("homes"))
    );
    let twice = base()
        .and_then(|policy| policy.narrowed(with_homes(vec![path("/Users"), path("/Users")])))
        .expect("a policy");
    let profile = Profile::for_policy(&twice).expect("a profile");
    assert_eq!(profile.text().matches("HM_").count(), 1);
}
