//! The profile of a native job: its folder, its own trees on the store's mount, a
//! read-write mount beside them, read denies, and each network level. A golden per shape,
//! the order of the trees' rule, read denies after the allows, the floor, and the
//! in-process apply's strings. (A job is never given a writable put directory; the
//! read-write mount here is any mount grant.)

use js_mac_pool::seatbelt::{
    AbsPath, Caps, InitError, MAX_LISTED, MountGrant, MountTrees, Narrowing, Net, Policy,
    PolicyError, Ports, Profile, ReadModel, SandboxInit, SlotLayout, TreesError,
};

const ROOT: &str = "/Volumes/store/fs";
const TREE_A: &str = "/Volumes/store/fs/mount-provider/aa/r1/0";
const TREE_B: &str = "/Volumes/store/fs/mount-provider/aa/r1/1";
const OUT: &str = "/Volumes/work/out";
const JOB: &str = "/private/var/js-mac-job/jobs/j1";

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

fn path(text: &str) -> AbsPath {
    AbsPath::parse(text).expect("a good path")
}

fn trees(list: &[&str]) -> MountTrees {
    MountTrees::new(path(ROOT), list.iter().map(|t| path(t)).collect()).expect("trees")
}

/// A read-write mount grant beside the store's mount.
fn rw_mount() -> MountGrant {
    MountGrant {
        path: path(OUT),
        write: true,
        no_exec: false,
    }
}

/// A job's layout: its folder is the one writable place, with no user-folder entry.
fn job(trees: Option<MountTrees>, mounts: Vec<MountGrant>) -> SlotLayout {
    SlotLayout {
        run: path(JOB),
        uid_entry: None,
        inputs: vec![],
        read_denies: vec![],
        mounts,
        trees,
    }
}

fn caps(net: Net) -> Caps {
    Caps {
        net,
        read_model: ReadModel::Open,
    }
}

fn ports(list: &[u16]) -> Ports {
    Ports::new(list).expect("ports")
}

fn profile(layout: SlotLayout, net: Net) -> Profile {
    let policy = Policy::base(layout, caps(net)).expect("a policy");
    Profile::for_policy(&policy).expect("a profile")
}

fn params(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// The whole text of a job with two trees on the mount, and its parameters: the mount
/// root, then each tree in path order.
///
/// Catches a missing deny of the mount, a tree rule without its metadata grant, and a
/// tree path written into the text.
#[test]
fn a_job_with_two_trees_matches_its_golden() {
    let profile = profile(job(Some(trees(&[TREE_B, TREE_A])), vec![]), Net::Off);
    assert_eq!(profile.text(), include_str!("goldens/job_trees.sb"));
    assert_eq!(
        profile.params(),
        params(&[
            ("RUN", JOB),
            ("MOUNT", ROOT),
            ("MT_0", TREE_A),
            ("MT_1", TREE_B)
        ])
    );
}

/// The whole text of a job that names outbound ports: loopback, TCP to each port on any
/// address, and the resolver's socket.
///
/// Catches a port rule that names loopback only, and a lost name-lookup rule.
#[test]
fn a_job_that_names_outbound_ports_matches_its_golden() {
    let profile = profile(job(None, vec![]), Net::Remote(ports(&[8980, 443])));
    assert_eq!(profile.text(), include_str!("goldens/job_remote.sb"));
    assert_eq!(profile.params(), params(&[("RUN", JOB)]));
}

/// The whole text of a job with trees and outbound ports.
///
/// Catches a section out of its place when the other is present.
#[test]
fn a_job_with_trees_and_ports_matches_its_golden() {
    let profile = profile(
        job(Some(trees(&[TREE_A, TREE_B])), vec![]),
        Net::Remote(ports(&[443, 8980])),
    );
    assert_eq!(profile.text(), include_str!("goldens/job_all.sb"));
    assert_eq!(
        profile.params(),
        params(&[
            ("RUN", JOB),
            ("MOUNT", ROOT),
            ("MT_0", TREE_A),
            ("MT_1", TREE_B),
        ])
    );
}

/// A job whose own tree is A, on a mount that also holds B: its profile denies reads below
/// the mount first, then allows A, and nothing in it names B.
///
/// Catches a profile without the deny (every tree on the mount readable), an allow placed
/// before the deny (the deny would win and the job could not read its own tree), and a
/// tree list that leaks another job's tree.
#[test]
fn a_job_reads_its_own_tree_and_not_another_on_the_same_mount() {
    let profile = profile(job(Some(trees(&[TREE_A])), vec![]), Net::Off);
    let text = profile.text();
    let deny = text
        .find("(deny file-read* (subpath (param \"MOUNT\")))")
        .expect("the mount is denied");
    let allow = text
        .find("(allow file-read* (subpath (param \"MT_0\")))")
        .expect("the tree is allowed");
    let walk = text
        .find("(allow file-read-metadata (path-ancestors (param \"MT_0\")))")
        .expect("the walk to the tree is allowed");
    assert!(deny < allow, "{text}");
    assert!(deny < walk, "{text}");
    assert!(!text.contains("MT_1"), "{text}");
    let values: Vec<&str> = profile.params().iter().map(|(_, v)| v.as_str()).collect();
    assert_eq!(values, [JOB, ROOT, TREE_A]);
    assert!(!values.contains(&TREE_B));
}

/// Nothing below the mount's root is run or mapped executable, the job's own trees included:
/// the exec and executable-mapping denies of the root come before the trees' read grants, and
/// no rule of the profile allows either again.
///
/// Catches a tree a job can execute because it may read it (the profile allows by default,
/// and a tree gets no mount grant that would carry a `no_exec` deny).
#[test]
fn nothing_below_the_mount_is_executable_even_the_jobs_own_tree() {
    let profile = profile(job(Some(trees(&[TREE_A, TREE_B])), vec![]), Net::Off);
    let text = profile.text();
    let at = |line: &str| {
        text.lines()
            .position(|one| one == line)
            .unwrap_or_else(|| panic!("no {line:?} in\n{text}"))
    };
    let exec = at("(deny process-exec* (subpath (param \"MOUNT\")))");
    let map = at("(deny file-map-executable (subpath (param \"MOUNT\")))");
    let grant = at("(allow file-read* (subpath (param \"MT_0\")))");
    assert!(exec < grant && map < grant, "{text}");
    assert!(!text.contains("(allow process-exec"), "{text}");
    assert!(!text.contains("(allow file-map-executable"), "{text}");
}

/// A job on a host with a mount and no trees of its own reads nothing below the mount.
///
/// Catches a deny that is left out when the tree list is empty.
#[test]
fn a_job_without_trees_reads_nothing_on_the_mount() {
    let profile = profile(job(Some(trees(&[])), vec![]), Net::Off);
    assert!(
        profile
            .text()
            .contains("(deny file-read* (subpath (param \"MOUNT\")))\n(deny network*)")
    );
    assert_eq!(profile.params(), params(&[("RUN", JOB), ("MOUNT", ROOT)]));
}

/// A tree must be strictly below the mount root, by whole components; each refusal names
/// the path.
///
/// Catches a tree that is the mount root (it would grant every tree), a sibling that
/// shares the root's spelling as a prefix, and a tree elsewhere.
#[test]
fn a_tree_not_strictly_below_the_mount_root_is_refused_by_its_path() {
    let refused = |tree: &str| MountTrees::new(path(ROOT), vec![path(TREE_A), path(tree)]);
    assert_eq!(refused(ROOT), Err(TreesError::IsRoot(ROOT.to_owned())));
    for tree in ["/Volumes/store/fsx/t", "/Volumes/store", "/Volumes/other/t"] {
        assert_eq!(
            refused(tree),
            Err(TreesError::NotBelow {
                tree: tree.to_owned(),
                root: ROOT.to_owned()
            }),
            "{tree}"
        );
    }
    assert_eq!(
        TreesError::IsRoot(ROOT.to_owned()).to_string(),
        "the tree /Volumes/store/fs is the mount root itself"
    );
    assert_eq!(
        refused("/Volumes/store/fsx/t").unwrap_err().to_string(),
        "the tree /Volumes/store/fsx/t is not below the mount root /Volumes/store/fs"
    );
}

/// The tree list is bounded like every list of a policy, and a tree named twice is one.
///
/// Catches an unbounded list and a repeated rule.
#[test]
fn a_tree_list_over_the_bound_is_refused_and_repeats_count_once() {
    let many =
        |n: usize| -> Vec<AbsPath> { (0..n).map(|i| path(&format!("{ROOT}/t/{i}"))).collect() };
    assert!(MountTrees::new(path(ROOT), many(MAX_LISTED)).is_ok());
    assert_eq!(
        MountTrees::new(path(ROOT), many(MAX_LISTED + 1)),
        Err(TreesError::TooMany)
    );
    assert_eq!(TreesError::TooMany.to_string(), "too many trees");
    let twice = MountTrees::new(path(ROOT), vec![path(TREE_A), path(TREE_A)]).expect("trees");
    let profile = profile(job(Some(twice), vec![]), Net::Off);
    assert_eq!(profile.text().matches("MT_").count(), 2);
}

/// A mount grant that is the trees' root, above it or below it would reopen what the
/// trees' rule denies, so the layout is refused; a grant beside the root is not.
///
/// Catches a mount placed where its read grant undoes the
/// own-tree rule.
#[test]
fn a_mount_grant_over_the_trees_root_is_refused() {
    let grant = |at: &str| MountGrant {
        path: path(at),
        write: true,
        no_exec: false,
    };
    for at in [ROOT, "/Volumes/store", TREE_B, "/Volumes/store/fs/other"] {
        let layout = job(Some(trees(&[TREE_A])), vec![grant(at)]);
        assert_eq!(
            Policy::base(layout, caps(Net::Off)),
            Err(PolicyError::MountOverTrees),
            "{at}"
        );
    }
    let beside = job(Some(trees(&[TREE_A])), vec![grant(OUT)]);
    assert!(Policy::base(beside, caps(Net::Off)).is_ok());
    assert_eq!(
        PolicyError::MountOverTrees.to_string(),
        "a mount grant overlaps the trees' mount root"
    );
}

/// The allow read model takes read grants only as proof objects, so trees named by path
/// are refused there.
///
/// Catches a path-named read grant slipping into the allow model.
#[test]
fn trees_are_refused_under_the_allow_read_model() {
    let layout = job(Some(trees(&[TREE_A])), vec![]);
    let caps = Caps {
        net: Net::Off,
        read_model: ReadModel::Allow,
    };
    assert_eq!(
        Policy::base(layout, caps),
        Err(PolicyError::TreesNeedOpenModel)
    );
    assert_eq!(
        PolicyError::TreesNeedOpenModel.to_string(),
        "trees on a mount need the open read model"
    );
}

/// Every shape of a job's policy ends with the floor, in order: each network level, with
/// and without trees (none, empty, two) and a read-write mount.
///
/// Catches a trees, mount or network rule emitted after the floor, where it would beat it.
#[test]
fn the_floor_is_last_in_every_job_shape() {
    let nets = [
        Net::Off,
        Net::Ports(ports(&[1])),
        Net::Loopback,
        Net::Remote(ports(&[443])),
        Net::Remote(ports(&[80, 443, 8980])),
        Net::On,
    ];
    let tree_sets: [Option<&[&str]>; 3] = [None, Some(&[]), Some(&[TREE_A, TREE_B])];
    for net in nets {
        for set in tree_sets {
            for mounts in [vec![], vec![rw_mount()]] {
                let profile = profile(job(set.map(trees), mounts), net.clone());
                let text = profile.text();
                assert!(text.ends_with(FLOOR_TEXT), "{net:?} {set:?}:\n{text}");
                assert_eq!(text.matches("(deny mach-register)").count(), 1);
            }
        }
    }
}

/// The devices a job may write are `/dev/null` and its own descriptors (`/dev/fd`), in
/// every job shape: `/dev/zero` and `/dev/tty` are not granted. On a Mac the measured build
/// set (clang, swiftc, python, git, xcrun) wrote neither: `/dev/tty` was touched only by a
/// shell without a terminal, where the write fails without a profile too.
///
/// Catches a device grant widened to `/dev/zero`, `/dev/tty` or all of `/dev`.
#[test]
fn a_job_writes_no_device_but_dev_null_and_its_own_descriptors() {
    let grant = "(allow file-write* (subpath (param \"RUN\")) (literal \"/dev/null\") (subpath \"/dev/fd\"))\n";
    for net in [Net::Off, Net::Loopback, Net::Remote(ports(&[443])), Net::On] {
        for mounts in [vec![], vec![rw_mount()]] {
            let profile = profile(job(Some(trees(&[TREE_A])), mounts), net.clone());
            let text = profile.text();
            assert_eq!(text.matches(grant).count(), 1, "{net:?}:\n{text}");
            for device in ["/dev/zero", "/dev/tty", "(subpath \"/dev\")"] {
                assert!(!text.contains(device), "{net:?} grants {device}:\n{text}");
            }
        }
    }
}

/// Every job shape denies process information about other processes, then allows it
/// for the job's own (itself and its sandbox), in that order, so the allow cannot widen
/// to others.
///
/// Catches the deny dropped, or placed after the allow it narrows.
#[test]
fn a_job_is_denied_other_processes_information_before_its_own_is_allowed() {
    let deny = "(deny process-info* (target others))\n";
    let allow = "(allow process-info* (target self) (target same-sandbox))\n";
    for net in [Net::Off, Net::Loopback, Net::Remote(ports(&[443])), Net::On] {
        for set in [None, Some(trees(&[TREE_A, TREE_B]))] {
            let profile = profile(job(set, vec![rw_mount()]), net.clone());
            let text = profile.text();
            assert_eq!(text.matches(deny).count(), 1, "{net:?}:\n{text}");
            assert_eq!(text.matches(allow).count(), 1, "{net:?}:\n{text}");
            assert_eq!(
                text.find(allow).map(|at| at - deny.len()),
                text.find(deny),
                "the allow follows the deny:\n{text}"
            );
        }
    }
}

/// Places a runtime denies as the privacy fence: a directory, a file in it, a file
/// elsewhere, each under both spellings.
const FENCE: [&str; 6] = [
    "/Library/Application Support/com.apple.TCC",
    "/Library/Application Support/com.apple.TCC/TCC.db",
    "/Library/Preferences/com.apple.TimeMachine.plist",
    "/System/Volumes/Data/Library/Application Support/com.apple.TCC",
    "/System/Volumes/Data/Library/Application Support/com.apple.TCC/TCC.db",
    "/System/Volumes/Data/Library/Preferences/com.apple.TimeMachine.plist",
];

/// Read denies, each place's file-read deny after every file allow of the profile but a
/// mount grant's (which may not hold a denied place: see the next test), in every job
/// shape; each place is a parameter, never text.
///
/// Catches a read deny emitted before an allow that covers it (a later rule wins, so the
/// allow would reopen the place), and a place spelled into the text.
#[test]
fn read_denies_follow_every_allow_but_a_mount_grants_in_every_job_shape() {
    let nets = [Net::Off, Net::Loopback, Net::Remote(ports(&[443])), Net::On];
    for net in nets {
        for set in [None, Some(trees(&[TREE_A, TREE_B]))] {
            for mounts in [vec![], vec![rw_mount()]] {
                let layout = SlotLayout {
                    read_denies: FENCE.iter().map(|place| path(place)).collect(),
                    ..job(set.clone(), mounts)
                };
                let profile = profile(layout, net.clone());
                let lines: Vec<&str> = profile.text().lines().collect();
                let last_allow = lines
                    .iter()
                    .rposition(|line| {
                        line.starts_with("(allow file-") && !line.contains("(param \"GR_")
                    })
                    .expect("the job folder's grant");
                for i in 0..FENCE.len() {
                    let deny = format!("(deny file-read* (subpath (param \"RD_{i}\")))");
                    let at = lines.iter().position(|line| *line == deny);
                    assert!(at > Some(last_allow), "{net:?} {deny}:\n{}", profile.text());
                }
                let named: Vec<&str> = profile
                    .params()
                    .iter()
                    .filter(|(name, _)| name.starts_with("RD_"))
                    .map(|(_, value)| value.as_str())
                    .collect();
                let mut fence = FENCE.to_vec();
                fence.sort_unstable();
                assert_eq!(named, fence);
                assert!(!profile.text().contains("com.apple.TCC"));
            }
        }
    }
}

/// Read denies also deny hard links and clones of the place: in every job shape each read
/// deny has exactly one `file-link file-clone` deny line, after its file-read deny and before
/// the first mount grant, and no later line allows either operation again but each mount's
/// one grant of both, for its own path.
///
/// Catches a denied file that a run hard-links or clones into its run directory and reads
/// there under a path no deny names, a link deny placed after a grant that reopens it, an
/// allow of link or clone after the denies that is not a mount's, and a mount that loses
/// links and clones below its denied root.
#[test]
fn every_read_deny_also_denies_links_and_clones_in_every_job_shape() {
    let nets = [Net::Off, Net::Loopback, Net::Remote(ports(&[443])), Net::On];
    for net in nets {
        for set in [None, Some(trees(&[TREE_A, TREE_B]))] {
            for mounts in [vec![], vec![rw_mount()]] {
                let layout = SlotLayout {
                    read_denies: FENCE.iter().map(|place| path(place)).collect(),
                    ..job(set.clone(), mounts)
                };
                let profile = profile(layout, net.clone());
                let text = profile.text();
                let lines: Vec<&str> = text.lines().collect();
                let first_grant = lines.iter().position(|line| {
                    line.starts_with("(allow file-read*") && line.contains("(param \"GR_")
                });
                let mut last_link_deny = 0;
                for i in 0..FENCE.len() {
                    let read = format!("(deny file-read* (subpath (param \"RD_{i}\")))");
                    let link = format!("(deny file-link file-clone (subpath (param \"RD_{i}\")))");
                    let found: Vec<usize> =
                        (0..lines.len()).filter(|at| lines[*at] == link).collect();
                    assert_eq!(found.len(), 1, "{net:?} {link}:\n{text}");
                    let read_at = lines.iter().position(|line| *line == read);
                    assert!(
                        read_at.is_some_and(|at| at < found[0]),
                        "{net:?} {link}:\n{text}"
                    );
                    if let Some(grant) = first_grant {
                        assert!(found[0] < grant, "{net:?} {link}:\n{text}");
                    }
                    last_link_deny = last_link_deny.max(found[0]);
                }
                // Only a mount grant gives links and clones back after the denies, and only
                // for what it grants (a mount at or above a denied place is refused, see
                // the next test).
                for line in &lines[last_link_deny..] {
                    if line.starts_with("(allow file-link") || line.starts_with("(allow file-clone")
                    {
                        assert!(
                            line.starts_with("(allow file-link file-clone (subpath (param \"GR_"),
                            "{net:?}:\n{text}"
                        );
                    }
                }
                let mount_links = lines
                    .iter()
                    .filter(|line| {
                        line.starts_with("(allow file-link file-clone (subpath (param \"GR_")
                    })
                    .count();
                assert_eq!(
                    mount_links,
                    profile
                        .params()
                        .iter()
                        .filter(|(name, _)| name.starts_with("GR_"))
                        .count()
                );
            }
        }
    }
}

/// A mount grant at or above a read-denied place is refused, at the start and when a
/// narrowing adds the deny: its grant comes after the denies and would reopen the place.
/// A grant below a denied place, or beside it, is not.
///
/// Catches a mount of `/Library` (or of the place itself) that hands a job the places
/// the runtime denies.
#[test]
fn a_mount_grant_at_or_above_a_read_deny_is_refused() {
    let grant = |at: &str| MountGrant {
        path: path(at),
        write: false,
        no_exec: true,
    };
    let with = |at: &str| SlotLayout {
        read_denies: FENCE.iter().map(|place| path(place)).collect(),
        ..job(None, vec![grant(at)])
    };
    for at in [
        "/Library",
        "/Library/Application Support/com.apple.TCC",
        "/Library/Preferences/com.apple.TimeMachine.plist",
        "/System/Volumes/Data",
    ] {
        assert_eq!(
            Policy::base(with(at), caps(Net::Off)),
            Err(PolicyError::MountOverReadDeny),
            "{at}"
        );
    }
    for at in [
        "/Library/Application Support/com.apple.TCC/below",
        "/Library/Application Support/com.apple.TCCx",
        "/Library/Preferences/other.plist",
    ] {
        assert!(Policy::base(with(at), caps(Net::Off)).is_ok(), "{at}");
    }
    let mounted = Policy::base(job(None, vec![grant("/Volumes/work")]), caps(Net::Off))
        .expect("no denies yet");
    let narrowing = Narrowing {
        deny_read: vec![path("/Volumes/work/state")],
        ..Narrowing::default()
    };
    assert_eq!(
        mounted.narrowed(narrowing),
        Err(PolicyError::MountOverReadDeny)
    );
    assert_eq!(
        PolicyError::MountOverReadDeny.to_string(),
        "a mount grant holds a read-denied path"
    );
}

/// `Remote` is above `Loopback` and below `On`; two port lists meet in the ports they
/// share, and none shared is `Loopback`.
///
/// Catches a narrowing that raises a level or keeps a port only one side named.
#[test]
fn a_remote_level_narrows_by_rank_and_by_shared_ports() {
    let remote = |list: &[u16]| Net::Remote(ports(list));
    let cases = [
        (remote(&[443]), Net::On, remote(&[443])),
        (Net::On, remote(&[443]), remote(&[443])),
        (remote(&[443]), Net::Loopback, Net::Loopback),
        (
            remote(&[443]),
            Net::Ports(ports(&[8080])),
            Net::Ports(ports(&[8080])),
        ),
        (remote(&[443]), Net::Off, Net::Off),
        (remote(&[80, 443]), remote(&[443, 8980]), remote(&[443])),
        (remote(&[80]), remote(&[443]), Net::Loopback),
    ];
    for (a, b, expected) in cases {
        assert_eq!(a.clone().narrow(b.clone()), expected, "{a:?} {b:?}");
    }
}

/// The in-process apply takes the text and the parameters flattened in the profile's
/// order, each as a C string.
///
/// Catches a parameter list that pairs a name with the wrong value or drops one.
#[test]
fn the_apply_strings_are_the_text_and_each_name_then_value() {
    let profile = profile(job(Some(trees(&[TREE_A])), vec![rw_mount()]), Net::Off);
    let init = profile.sandbox_init().expect("no NUL");
    assert_eq!(init.text().to_bytes(), profile.text().as_bytes());
    let flat: Vec<&str> = init
        .params()
        .iter()
        .map(|c| c.to_str().expect("text"))
        .collect();
    assert_eq!(
        flat,
        ["RUN", JOB, "MOUNT", ROOT, "MT_0", TREE_A, "GR_0", OUT]
    );
}

/// A NUL byte in the text or in a parameter's name or value is refused when the strings
/// are made, which is before any fork; the error names the parameter.
///
/// Catches a NUL that would cut the text or a parameter short inside the C call.
#[test]
fn a_nul_in_the_text_or_a_parameter_is_refused_before_the_apply() {
    let good = params(&[("RUN", JOB)]);
    assert!(SandboxInit::new("(version 1)", &good).is_ok());
    assert_eq!(
        SandboxInit::new("(version 1)\0(allow default)", &good),
        Err(InitError::TextNul)
    );
    assert_eq!(
        SandboxInit::new("(version 1)", &params(&[("RUN", JOB), ("MT_0", "/a\0b")])),
        Err(InitError::ParamNul("MT_0".to_owned()))
    );
    assert_eq!(
        SandboxInit::new("(version 1)", &params(&[("A\0B", "/a")])),
        Err(InitError::ParamNul("A\\0B".to_owned()))
    );
    assert_eq!(
        InitError::TextNul.to_string(),
        "the profile text holds a NUL byte"
    );
    assert_eq!(
        InitError::ParamNul("MT_0".to_owned()).to_string(),
        "the profile parameter MT_0 holds a NUL byte"
    );
}
