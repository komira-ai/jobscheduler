use std::os::unix::fs::symlink;

use js_agent_wire::Propagation;
use js_mac_pool::seatbelt::{
    Caps, MountGrant, Net, Policy, PolicyError, Profile, ReadModel, SlotLayout,
};
use js_testkit::dir::TestDir;

use super::*;

fn mount(source: MountSource, mode: MountMode) -> JobMount {
    JobMount {
        source,
        target: "/work/.kbf-tree".to_owned(),
        mode,
        sub_path: None,
        no_exec: true,
        propagation: Propagation::Private,
    }
}

fn provider() -> MountSource {
    MountSource::Provider {
        provider: "tierstore".to_owned(),
        reference: "cas/l/00".to_owned(),
    }
}

fn not_allowed(why: String) -> Result<Option<TreeRules>, NativeRefusal> {
    Err(NativeRefusal {
        code: refusal::INPUT_NOT_ALLOWED,
        why,
    })
}

/// Asserts that a provider's mount is admitted read-only and refused read-write and
/// copy-on-write, `input-not-allowed` with the exact detail, and that a host path's mount
/// is not this rule's in any mode. Catches a job that writes a tree every job of the Mac
/// that names it shares.
#[test]
fn a_providers_tree_is_admitted_read_only_only() {
    assert_eq!(
        check_provider_mode(&mount(provider(), MountMode::ReadOnly)),
        Ok(())
    );
    for mode in [MountMode::ReadWrite, MountMode::CopyOnWrite] {
        assert_eq!(
            check_provider_mode(&mount(provider(), mode)),
            Err(NativeRefusal {
                code: refusal::INPUT_NOT_ALLOWED,
                why: "/work/.kbf-tree: a provider's tree is shared by every native job \
                      that names it, so a native job mounts it read-only only"
                    .to_owned(),
            }),
            "{mode:?}"
        );
    }
    let host = MountSource::HostPath {
        path: "/srv/out".to_owned(),
    };
    for mode in [
        MountMode::ReadOnly,
        MountMode::ReadWrite,
        MountMode::CopyOnWrite,
    ] {
        assert_eq!(check_provider_mode(&mount(host.clone(), mode)), Ok(()));
    }
}

/// A directory with a read-only root `trees`, a writable root `put`, a root `both` given
/// under both flags, and a directory `loose` that is no root; their canonical paths.
struct Roots {
    _dir: TestDir,
    roots: MountRoots,
    trees: PathBuf,
    put: PathBuf,
    both: PathBuf,
    loose: PathBuf,
}

fn roots(name: &str) -> Roots {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let made = |name: &str| {
        let path = dir.path().join(name);
        fs::create_dir(&path).unwrap();
        fs::canonicalize(path).unwrap()
    };
    let (trees, put, both, loose) = (made("trees"), made("put"), made("both"), made("loose"));
    let named = |name: &str, path: &Path| (name.to_owned(), path.to_path_buf());
    let roots = MountRoots::with_read_only(
        &[named("put", &put), named("both-rw", &both)],
        &[named("trees", &trees), named("both-ro", &both)],
    )
    .unwrap();
    Roots {
        _dir: dir,
        roots,
        trees,
        put,
        both,
        loose,
    }
}

/// Asserts that `--native-trees-root` is taken only as an absolute path, with a
/// `--mount-provider`, that resolves to a `--mount-root-ro` no `--mount-root` also names; the
/// path taken is the canonical one (through a link), and no flag is no trees root. Each
/// refusal names the flag, by exact equality: a relative path, no provider, a path that is
/// not there, a directory that is no root, a writable root, and a path that is both. Catches
/// an agent that treats a writable root, or a directory outside every root, as the trees
/// root whose reads it opens to every run's trees.
#[test]
fn the_trees_root_is_one_read_only_mount_root_beside_a_provider() {
    let r = roots("native-fs-trees-root");
    let check = |given: &Path, providers| trees_root(Some(given), &r.roots, providers);
    assert_eq!(trees_root(None, &r.roots, false), Ok(None));
    assert_eq!(check(&r.trees, true), Ok(Some(r.trees.clone())));
    let link = r.loose.join("link");
    symlink(&r.trees, &link).unwrap();
    assert_eq!(check(&link, true), Ok(Some(r.trees.clone())));

    assert_eq!(
        check(Path::new("trees"), true),
        Err("--native-trees-root: trees is not an absolute path".to_owned())
    );
    assert_eq!(
        check(&r.trees, false),
        Err(
            "--native-trees-root: needs --mount-provider: the trees root is where a \
             provider serves its trees"
                .to_owned()
        )
    );
    let gone = r.loose.join("gone");
    assert_eq!(
        check(&gone, true),
        Err(format!(
            "--native-trees-root: {}: No such file or directory (os error 2)",
            gone.display()
        ))
    );
    assert_eq!(
        check(&r.loose, true),
        Err(format!(
            "--native-trees-root: {} is no --mount-root-ro",
            r.loose.display()
        ))
    );
    for writable in [&r.put, &r.both] {
        assert_eq!(
            check(writable, true),
            Err(format!(
                "--native-trees-root: {} is a --mount-root, which jobs may write: the trees \
                 root is a --mount-root-ro only",
                writable.display()
            ))
        );
    }
}

/// Asserts that the roots are listed once by path, in path order, writable when any name of
/// the path is, with only the trees root marked; and that every root but the trees root is a
/// read deny, and with no trees root every root is. Catches a trees root denied to every run
/// (its trees would be unreadable) and another root left readable.
#[test]
fn every_mount_root_but_the_trees_root_is_denied() {
    let r = roots("native-fs-roots-of");
    let root = |path: &PathBuf, read_only, trees| MountRoot {
        path: path.clone(),
        read_only,
        trees,
    };
    let listed = roots_of(&r.roots, Some(&r.trees));
    assert_eq!(
        listed,
        [
            root(&r.both, false, false),
            root(&r.put, false, false),
            root(&r.trees, true, true),
        ]
    );
    let denied: Vec<&Path> = denied_roots(&listed)
        .map(|root| root.path.as_path())
        .collect();
    assert_eq!(denied, [r.both.as_path(), r.put.as_path()]);

    let without = roots_of(&r.roots, None);
    assert!(without.iter().all(|root| !root.trees), "{without:?}");
    assert_eq!(denied_roots(&without).count(), 3);
}

/// A trees root with two trees, a directory outside it, and links: `root/up` to the outside
/// directory, `outside/in` to tree `a`.
struct Trees {
    _dir: TestDir,
    root: PathBuf,
    a: PathBuf,
    b: PathBuf,
    outside: PathBuf,
}

fn trees(name: &str) -> Trees {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("mount");
    let (a, b) = (
        root.join("mount-provider/aa/r1/0"),
        root.join("mount-provider/aa/r2/0"),
    );
    let outside = base.join("outside");
    for path in [&a, &b, &outside] {
        fs::create_dir_all(path).unwrap();
    }
    symlink(&outside, root.join("up")).unwrap();
    symlink(&a, outside.join("in")).unwrap();
    Trees {
        _dir: dir,
        root,
        a,
        b,
        outside,
    }
}

/// A plan of the kbf shape: read-only, `no_exec`.
fn kbf(dir: &Path) -> (PathBuf, bool, bool) {
    (dir.to_path_buf(), true, true)
}

/// Asserts that no plan, and plans outside the root, are no trees (`None`); that two trees
/// of the kbf shape (read-only, `no_exec`) are taken canonical and in order, one named
/// through a link from outside the root included; and that a plan beside the trees stays
/// out of the rules. Catches refusing every real build action, which an inverted rule
/// would do, and a tree left out of the run's rules (unreadable to its job).
#[test]
fn trees_below_the_root_are_taken_canonical() {
    let t = trees("native-fs-tree-rules");
    assert_eq!(tree_rules(&t.root, &[]), Ok(None));
    assert_eq!(tree_rules(&t.root, &[kbf(&t.outside)]), Ok(None));
    let beside = (t.outside.clone(), false, false);
    assert_eq!(
        tree_rules(&t.root, &[kbf(&t.a), beside, kbf(&t.b.join("../0/."))]),
        Ok(Some(TreeRules {
            root: t.root.clone(),
            trees: vec![t.a.clone(), t.b.clone()],
        }))
    );
    assert_eq!(
        tree_rules(&t.root, &[kbf(&t.outside.join("in"))]),
        Ok(Some(TreeRules {
            root: t.root.clone(),
            trees: vec![t.a.clone()],
        }))
    );
}

/// Asserts the refusals of a tree, each `input-not-allowed` naming the directory as given,
/// by exact equality: the root itself, a `..` and a link that lead out of the root, a tree
/// mounted read-write, one without `no_exec`, and a mount that holds the root; and that a
/// directory that is not there is `input-unavailable`. Catches a tree that opens the whole
/// mount, one that opens a directory outside it, one a job may write or run, and a grant
/// above the root that reopens every run's trees.
#[test]
fn a_tree_that_is_not_one_read_only_tree_below_the_root_is_refused() {
    let t = trees("native-fs-tree-refusals");
    let root = t.root.display();
    let up = t.root.join("mount-provider/../..").join("outside");
    let link = t.root.join("up");
    let not_below = |dir: &Path| {
        not_allowed(format!(
            "{}: the tree {} is not below the mount root {root}",
            dir.display(),
            t.outside.display()
        ))
    };
    assert_eq!(tree_rules(&t.root, &[kbf(&up)]), not_below(&up));
    assert_eq!(tree_rules(&t.root, &[kbf(&link)]), not_below(&link));
    assert_eq!(
        tree_rules(&t.root, &[kbf(&t.root)]),
        not_allowed(format!("{root}: the tree {root} is the mount root itself"))
    );
    assert_eq!(
        tree_rules(&t.root, &[kbf(&t.a), (t.b.clone(), false, true)]),
        not_allowed(format!(
            "{}: a tree below the trees root {root} is mounted read-only only",
            t.b.display()
        ))
    );
    assert_eq!(
        tree_rules(&t.root, &[(t.a.clone(), true, false)]),
        not_allowed(format!(
            "{}: a tree below the trees root {root} is mounted no_exec only: the profile \
             grants a tree reads only",
            t.a.display()
        ))
    );
    let above = t.root.parent().unwrap().to_path_buf();
    assert_eq!(
        tree_rules(&t.root, &[kbf(&above)]),
        not_allowed(format!(
            "{}: holds the trees root {root}: its grant would open every run's trees",
            above.display()
        ))
    );
    let gone = t.root.join("mount-provider/gone");
    assert_eq!(
        tree_rules(&t.root, &[kbf(&gone)]),
        Err(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: format!("{}: No such file or directory (os error 2)", gone.display()),
        })
    );
}

const ROOT: &str = "/Volumes/tierstore";
const TREE_A: &str = "/Volumes/tierstore/mount-provider/aa/r1/0";
const TREE_B: &str = "/Volumes/tierstore/mount-provider/aa/r2/0";
const OTHER_ROOT: &str = "/Users/Shared/inputs";
const RUN: &str = "/Users/js-slot-700/exec";

fn path(text: &str) -> AbsPath {
    AbsPath::parse(text).unwrap()
}

/// The roots of a Mac with tierstore: the trees root and another read-only root.
fn mac_roots() -> Vec<MountRoot> {
    vec![
        MountRoot {
            path: PathBuf::from(OTHER_ROOT),
            read_only: true,
            trees: false,
        },
        MountRoot {
            path: PathBuf::from(ROOT),
            read_only: true,
            trees: true,
        },
    ]
}

/// The profile of a run whose trees are `trees`, with `grants`, and the read denies of
/// `roots` as [`denied_roots`] makes them.
fn profile_of(
    roots: &[MountRoot],
    trees: &[&str],
    grants: Vec<MountGrant>,
) -> Result<Profile, PolicyError> {
    let rules = TreeRules {
        root: PathBuf::from(ROOT),
        trees: trees.iter().map(PathBuf::from).collect(),
    };
    let layout = SlotLayout {
        run: path(RUN),
        uid_entry: None,
        inputs: Vec::new(),
        read_denies: denied_roots(roots)
            .map(|root| AbsPath::new(&root.path).unwrap())
            .collect(),
        mounts: grants,
        trees: Some(rules.mount_trees().unwrap()),
    };
    let caps = Caps {
        net: Net::Off,
        read_model: ReadModel::Open,
    };
    Ok(Profile::for_policy(&Policy::base(layout, caps)?).unwrap())
}

fn param<'a>(profile: &'a Profile, name: &str) -> Option<&'a str> {
    profile
        .params()
        .iter()
        .find(|(given, _)| given == name)
        .map(|(_, value)| value.as_str())
}

/// Asserts, on the real profile builder, that a run's profile with the read denies the
/// agent makes of its roots denies the mount root (`MOUNT`) and grants back its one tree
/// (`MT_0`), that no read deny (`RD_`) is the trees root while the other root is one, and
/// that run A's profile names its own tree and never run B's. Catches the trees root among
/// the read denies: the builder renders the read denies after the trees' grants, so that
/// later deny would hide every tree from its job.
#[test]
fn a_runs_profile_reads_its_own_tree_and_denies_no_tree_after_the_grant() {
    let roots = mac_roots();
    let a = profile_of(&roots, &[TREE_A], Vec::new()).unwrap();
    let b = profile_of(&roots, &[TREE_B], Vec::new()).unwrap();
    assert_eq!(param(&a, "MOUNT"), Some(ROOT));
    assert_eq!(param(&a, "MT_0"), Some(TREE_A));
    assert_eq!(param(&a, "MT_1"), None);
    assert_eq!(param(&b, "MT_0"), Some(TREE_B));
    let denies: Vec<&str> = a
        .params()
        .iter()
        .filter(|(name, _)| name.starts_with("RD_"))
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(denies, [OTHER_ROOT]);
    assert!(
        a.text()
            .contains("(allow file-read* (subpath (param \"MT_0\")))")
    );
    for (profile, own, other) in [(&a, TREE_A, TREE_B), (&b, TREE_B, TREE_A)] {
        let values: Vec<&str> = profile.params().iter().map(|(_, v)| v.as_str()).collect();
        assert!(values.contains(&own), "{values:?}");
        assert!(!values.contains(&other), "{values:?}");
    }
}

/// Asserts that a host path's grant beside the trees root builds with the run's trees,
/// while a grant of the run's tree (what a grant of every plan would add) is refused by the
/// builder as `MountOverTrees`. Catches the tree plans left among the grants: every start
/// with a tree would fail at its profile.
#[test]
fn a_grant_beside_the_trees_builds_and_a_grant_of_a_tree_does_not() {
    let grant = |at: &str| MountGrant {
        path: path(at),
        write: false,
        no_exec: true,
    };
    let beside = profile_of(
        &mac_roots(),
        &[TREE_A],
        vec![grant("/Users/Shared/inputs/x")],
    );
    assert!(beside.is_ok(), "{beside:?}");
    assert_eq!(
        profile_of(&mac_roots(), &[TREE_A], vec![grant(TREE_A)]).unwrap_err(),
        PolicyError::MountOverTrees
    );
}

/// Asserts that the rules become the builder's trees as given, and that a root the profile
/// cannot name (a `/tmp` spelling) and a tree the builder refuses are each an error naming
/// the path. Catches a conversion that drops a tree or passes a path the profile misreads.
#[test]
fn the_rules_become_the_builders_trees_or_name_the_path() {
    let rules = TreeRules {
        root: PathBuf::from(ROOT),
        trees: vec![PathBuf::from(TREE_B), PathBuf::from(TREE_A)],
    };
    assert_eq!(
        rules.mount_trees(),
        Ok(MountTrees::new(path(ROOT), vec![path(TREE_B), path(TREE_A)]).unwrap())
    );
    let linked = TreeRules {
        root: PathBuf::from("/tmp/mount"),
        trees: Vec::new(),
    };
    assert_eq!(
        linked.mount_trees().unwrap_err().to_string(),
        "/tmp/mount: path starts with /tmp, a symbolic link; use the canonical /private \
         spelling"
    );
    let outside = TreeRules {
        root: PathBuf::from(ROOT),
        trees: vec![PathBuf::from(OTHER_ROOT)],
    };
    assert_eq!(
        outside.mount_trees().unwrap_err().to_string(),
        format!("the tree {OTHER_ROOT} is not below the mount root {ROOT}")
    );
    let tree_path = TreeRules {
        root: PathBuf::from(ROOT),
        trees: vec![PathBuf::from("/Volumes/tierstore/a\nb")],
    };
    assert_eq!(
        tree_path.mount_trees().unwrap_err().to_string(),
        "/Volumes/tierstore/a\nb: path contains a control character"
    );
}
