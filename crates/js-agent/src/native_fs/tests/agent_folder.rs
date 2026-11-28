//! The tree rule's checks that the numbered-attempt shape left without a test, and the
//! provider folder it held to nothing but hex: the folder is the one the provider keeps for
//! this agent, the index is a number as the provider writes it, and a path that is not UTF-8
//! is refused by name.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

use super::*;

/// Another agent's name in its provider calls, and its folder: `jy`, `6a79`.
const OTHER_FOLDER: &str = "mount-provider/6a79";

/// The text of a refusal of `dir` as not the run's shape, below the trees root `root`.
fn not_its_shape(root: &Path, dir: &Path) -> Result<TreeRules, NativeRefusal> {
    not_allowed(format!(
        "{}: below the trees root {}, a job mounts only a tree the provider answered for its \
         own run (mount-provider/<provider>/<run>/<index> or \
         mount-provider/<provider>/<run>/a<attempt>/<index>)",
        dir.display(),
        root.display()
    ))
}

/// The text of a refusal of `dir`, which lies in the folder `found` below `mount-provider`,
/// as not this agent's.
fn other_agents(root: &Path, dir: &Path, found: &str) -> Result<TreeRules, NativeRefusal> {
    not_allowed(format!(
        "{}: below the trees root {}, mount-provider/{found} is not this agent's folder: the \
         provider answers this agent's trees in mount-provider/6a7a",
        dir.display(),
        root.display()
    ))
}

/// Asserts that a tree in another agent's folder that carries this run's own name, at both
/// of the provider's shapes, is refused `input-not-allowed` naming both folders, by exact
/// equality, beside this agent's tree of the same run, which is taken; and that by path alone
/// the folders the old rule took for any lowercase hex (`6a79`, `0f9e`, the `aa` the tests
/// used), the agent's own folder in uppercase, a missing folder (`mount-provider//...`) and
/// another agent's folder itself are each refused as another agent's. Catches a rule that
/// takes any provider folder below the root, which would open to a run every agent's tree of
/// a run with its name.
#[test]
fn another_agents_folder_with_this_runs_name_is_refused_by_name() {
    let t = trees("native-fs-other-agent");
    let plain = t.root.join(OTHER_FOLDER).join("6a732e7231/0");
    let numbered = t.root.join(OTHER_FOLDER).join("6a732e7231/a3/0");
    for dir in [&plain, &numbered] {
        fs::create_dir_all(dir).unwrap();
    }
    for theirs in [&plain, &numbered] {
        assert_eq!(
            tree_rules(&t.root, Some(AGENT), OWN, &[kbf(&t.a), kbf(theirs)]),
            other_agents(&t.root, theirs, "6a79"),
            "{}",
            theirs.display()
        );
    }
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), OWN, &[kbf(&t.a)]).map(|rules| rules.trees),
        Ok(vec![t.a.clone()])
    );
    // The other agent's own run reads its own tree.
    assert_eq!(
        tree_rules(&t.root, Some("jy"), OWN, &[kbf(&plain), kbf(&numbered)])
            .map(|rules| rules.trees),
        Ok(vec![plain.clone(), numbered.clone()])
    );

    let root = Path::new("/Volumes/tierstore");
    for (rest, found) in [
        ("mount-provider/6a79/6a732e7231/0", "6a79"),
        ("mount-provider/0f9e/6a732e7231/12", "0f9e"),
        ("mount-provider/aa/6a732e7231/0", "aa"),
        ("mount-provider/aa/6a732e7231/a1/0", "aa"),
        ("mount-provider/6A7A/6a732e7231/0", "6A7A"),
        ("mount-provider/6A7A/6a732e7231/a1/0", "6A7A"),
        ("mount-provider//6a732e7231/0", "6a732e7231"),
        ("mount-provider/6a79", "6a79"),
    ] {
        assert_eq!(
            check_runs_tree(root, Some(AGENT), OWN, &root.join(rest)),
            Err(NotRunsTree::OtherAgent {
                found: found.to_owned(),
                own: "6a7a".to_owned(),
            }),
            "{rest}"
        );
    }
}

/// Asserts that a runtime told no name for its provider calls takes no tree, refused
/// `input-not-allowed` by exact equality, while a run without a tree, and a mount outside the
/// root, still get the root's rules with no tree. Catches a runtime that, never told whose
/// folder is its own, takes every agent's.
#[test]
fn a_runtime_told_no_agent_takes_no_tree() {
    let t = trees("native-fs-no-agent");
    assert_eq!(
        tree_rules(&t.root, None, OWN, &[kbf(&t.a)]),
        not_allowed(format!(
            "{}: below the trees root {}, this runtime knows no name the agent calls its mount \
             provider by, so no tree is its own",
            t.a.display(),
            t.root.display()
        ))
    );
    assert_eq!(tree_rules(&t.root, None, OWN, &[]), none(&t.root));
    assert_eq!(
        tree_rules(&t.root, None, OWN, &[kbf(&t.outside)]),
        none(&t.root)
    );
}

/// Asserts the index as the provider writes it (decimal of the wire's `u32`): `0`, `7` and
/// `4294967295` are taken at both shapes; a leading zero (`00`, `01`, `007`), a sign (`+1`,
/// `-1`), one past `u32` (`4294967296`), digits that are not ASCII (a full-width `３`, an
/// Arabic-Indic `٣`), and the same digits in the attempt (`a３`, `a٣`, `a１2`) are refused.
/// Catches an index the provider never writes taken as the run's tree (`01` beside `1` names
/// the same source twice), and an attempt or index read by a parse that takes any Unicode
/// digit.
#[test]
fn the_index_and_the_attempt_are_numbers_as_the_provider_writes_them() {
    let root = Path::new("/Volumes/tierstore");
    let check =
        |rest: &str| check_runs_tree(root, Some(AGENT), OWN, &root.join(RUN_DIR).join(rest));
    for its in ["0", "7", "4294967295", "a1/0", "a1/7", "a1/4294967295"] {
        assert_eq!(check(its), Ok(()), "{its}");
    }
    for not_its in [
        "00",
        "01",
        "007",
        "+1",
        "-1",
        "4294967296",
        "３",
        "٣",
        "a1/00",
        "a1/01",
        "a1/+1",
        "a1/4294967296",
        "a1/３",
        "a３/0",
        "a٣/0",
        "a１2/0",
    ] {
        assert_eq!(
            check(not_its),
            Err(NotRunsTree::NotTheRunsShape),
            "{not_its}"
        );
    }
}

/// Asserts, on directories made below a trees root, that a tree whose index or attempt is
/// written with full-width digits (`a３/0`, `３`) or with a leading zero (`01`) is refused
/// `input-not-allowed` by exact equality, beside the run's `1`, which is taken. Catches the
/// lookup taking a folder the provider never made for this run.
#[test]
fn a_tree_whose_numbers_the_provider_never_writes_is_refused_on_disk() {
    let t = trees("native-fs-numbers");
    let wide_attempt = t.root.join(RUN_DIR).join("a３/0");
    let wide_index = t.root.join(RUN_DIR).join("３");
    let leading = t.root.join(RUN_DIR).join("01");
    for dir in [&wide_attempt, &wide_index, &leading] {
        fs::create_dir_all(dir).unwrap();
    }
    for refused in [&wide_attempt, &wide_index, &leading] {
        assert_eq!(
            tree_rules(&t.root, Some(AGENT), OWN, &[kbf(&t.a1), kbf(refused)]),
            not_its_shape(&t.root, refused),
            "{}",
            refused.display()
        );
    }
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), OWN, &[kbf(&t.a1)]).map(|rules| rules.trees),
        Ok(vec![t.a1.clone()])
    );
}

/// Asserts that a path with a component that is not UTF-8 is refused by that name, wherever
/// the component is: the agent's folder, the run's, the attempt's or the index. Catches the
/// rule reading such a component as an empty name, which it took as a lowercase hex provider
/// folder and as an index.
#[test]
fn a_path_that_is_not_utf8_is_refused_by_name() {
    let bad = OsStr::from_bytes(b"\xff");
    let root = Path::new("/Volumes/tierstore");
    let provider = Path::new(PROVIDER_DIR);
    for dir in [
        provider.join(bad).join("6a732e7231/0"),
        provider.join("6a7a").join(bad).join("0"),
        provider.join("6a7a/6a732e7231").join(bad).join("0"),
        provider.join("6a7a/6a732e7231").join(bad),
        provider.join("6a7a/6a732e7231/a1").join(bad),
    ] {
        assert_eq!(
            check_runs_tree(root, Some(AGENT), OWN, &root.join(&dir)),
            Err(NotRunsTree::NotUtf8("\u{fffd}".to_owned())),
            "{}",
            dir.display()
        );
    }
}

/// Asserts, on directories made below a trees root, that an index folder whose name is not
/// UTF-8 is refused `input-not-allowed` by exact equality, naming the component. Linux only:
/// APFS refuses to make a name that is not UTF-8, so on a Mac only the provider's NFS mount
/// could show one, and the test cannot make it there.
#[cfg(target_os = "linux")]
#[test]
fn an_index_folder_that_is_not_utf8_is_refused_on_disk() {
    let bad = OsStr::from_bytes(b"\xff");
    let t = trees("native-fs-not-utf8");
    let index = t.root.join(RUN_DIR).join(bad);
    fs::create_dir_all(&index).unwrap();
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), OWN, &[kbf(&t.a), kbf(&index)]),
        not_allowed(format!(
            "{}: below the trees root {}, the path component \"\u{fffd}\" is not UTF-8: the \
             provider names every folder of a tree in ASCII",
            index.display(),
            t.root.display()
        ))
    );
}

/// Asserts that a link at the run's folder, or at the attempt's, that leads to another run's
/// folder is refused as not the run's tree, and one that leads out of the root is refused as
/// not below it, each `input-not-allowed` naming the directory as given, by exact equality.
/// The run `r3`'s folder is a link to `r2`'s and `r4`'s a link out of the root; the attempts
/// `a5` and `a6` of `r1` are links to `r2`'s `a5` and out of the root. Catches a rule that
/// checks the path as the job named it rather than the directory it resolves to, which would
/// give a run another run's tree, or a directory outside the provider's mount.
#[test]
fn a_link_at_the_run_or_the_attempt_folder_is_refused() {
    let t = trees("native-fs-folder-links");
    let folder = |run: &str| t.root.join(PROVIDER_DIR).join("6a7a").join(run);
    // `js.r2`, `js.r3`, `js.r4`.
    let (r2, r3, r4) = (
        folder("6a732e7232"),
        folder("6a732e7233"),
        folder("6a732e7234"),
    );
    fs::create_dir_all(r2.join("a5/0")).unwrap();
    fs::create_dir_all(t.outside.join("0")).unwrap();
    symlink(&r2, &r3).unwrap();
    symlink(&t.outside, &r4).unwrap();
    symlink(r2.join("a5"), t.root.join(RUN_DIR).join("a5")).unwrap();
    symlink(&t.outside, t.root.join(RUN_DIR).join("a6")).unwrap();
    let not_below = |dir: &Path, to: &Path| {
        not_allowed(format!(
            "{}: the tree {} is not below the mount root {}",
            dir.display(),
            to.display(),
            t.root.display()
        ))
    };

    let r3_tree = r3.join("0");
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), "r3", &[kbf(&r3_tree)]),
        not_its_shape(&t.root, &r3_tree)
    );
    let r4_tree = r4.join("0");
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), "r4", &[kbf(&r4_tree)]),
        not_below(&r4_tree, &t.outside.join("0"))
    );
    let a5 = t.root.join(RUN_DIR).join("a5/0");
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), OWN, &[kbf(&t.a), kbf(&a5)]),
        not_its_shape(&t.root, &a5)
    );
    let a6 = t.root.join(RUN_DIR).join("a6/0");
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), OWN, &[kbf(&t.a), kbf(&a6)]),
        not_below(&a6, &t.outside.join("0"))
    );
    // The run the links lead to reads its own tree through its own folder.
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), "r2", &[kbf(&r2.join("a5/0"))]).map(|rules| rules.trees),
        Ok(vec![r2.join("a5/0")])
    );
}

/// Asserts that the run's own tree of a numbered attempt (`a44/0`) mounted read-write, or
/// without `no_exec`, is refused `input-not-allowed` by exact equality, as the tree of the
/// plain shape is. Catches the numbered shape taken with a mode the rule refuses at the plain
/// one: a job that writes, or runs, a tree every job naming it shares.
#[test]
fn a_numbered_tree_is_read_only_and_no_exec_only() {
    let t = trees("native-fs-numbered-modes");
    let numbered = t.root.join(RUN_DIR).join("a44/0");
    fs::create_dir_all(&numbered).unwrap();
    let root = t.root.display();
    assert_eq!(
        tree_rules(
            &t.root,
            Some(AGENT),
            OWN,
            &[(numbered.clone(), false, true)]
        ),
        not_allowed(format!(
            "{}: a tree below the trees root {root} is mounted read-only only",
            numbered.display()
        ))
    );
    assert_eq!(
        tree_rules(
            &t.root,
            Some(AGENT),
            OWN,
            &[(numbered.clone(), true, false)]
        ),
        not_allowed(format!(
            "{}: a tree below the trees root {root} is mounted no_exec only: the profile \
             grants a tree reads only",
            numbered.display()
        ))
    );
    assert_eq!(
        tree_rules(&t.root, Some(AGENT), OWN, &[kbf(&numbered)]).map(|rules| rules.trees),
        Ok(vec![numbered.clone()])
    );
}
