//! The two directory layouts of the scratch root and the run-files root: the podman rule
//! (`Private`: nothing for group or others, kept as it was) and the rootless crun rule
//! (`Searchable`: `0711`, because the job's ids must walk down to their directories). Each
//! rule is pinned by what it accepts and refuses, and by the modes of what it makes. The
//! kernel's side of the same rules (a job's ids reaching, or failing to reach, the
//! directories) is in `tests/privilege`.
//!
//! Every directory here is below `/tmp`, whose ancestors are searchable by others on any
//! host the tests run on; the target directory of a checkout often is not (a home directory
//! of mode `0750`), and the `Searchable` rule refuses a root below one.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_agent::lifecycle::{DirLayout, FilesRoot, ScratchRoot};
use js_agent_wire::RunFile;
use js_testkit::dir::TestDir;

/// A new directory below `/tmp`, searchable by everyone.
fn world(name: &str) -> TestDir {
    let dir = TestDir::under("/tmp", name);
    set_mode(dir.path(), 0o755);
    dir
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// A directory `name` below `parent` with exactly `bits`.
fn dir(parent: &Path, name: &str, bits: u32) -> PathBuf {
    let path = parent.join(name);
    fs::create_dir_all(&path).unwrap();
    set_mode(&path, bits);
    path
}

fn file(name: &str, contents: &str) -> RunFile {
    RunFile {
        name: name.into(),
        contents: contents.into(),
    }
}

fn me() -> (u32, u32) {
    (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    )
}

/// BEFORE: the default layout is the podman rule, so a root that others can search is
/// refused by both roots exactly as it was, and `open` still means `Private`. It catches a
/// default that drifted to the crun rule, which would let a podman host run with a scratch
/// root other users can enter.
#[test]
fn the_default_layout_still_refuses_a_root_others_can_search() {
    let parent = world("layout-before");
    let root = dir(parent.path(), "root", 0o711);
    assert!(
        ScratchRoot::open(&root)
            .unwrap_err()
            .contains("no permission")
    );
    assert!(
        FilesRoot::open(&root)
            .unwrap_err()
            .contains("no permission")
    );
    set_mode(&root, 0o700);
    assert_eq!(
        ScratchRoot::open(&root).unwrap().layout(),
        DirLayout::Private
    );
    assert_eq!(FilesRoot::open(&root).unwrap().layout(), DirLayout::Private);
    assert_eq!(DirLayout::default(), DirLayout::Private);
}

/// AFTER: a `Searchable` root takes `0711` and `0701` (others may search, and only that;
/// the group is not the job's) and refuses every other mode: one others cannot search
/// (`0700`, `0710`, `0750`), one others can list (`0755`, `0744`) or write (`0733`, `0702`),
/// and one the group can read or write (`0771`). It
/// catches a rule that lets a job list the names of the other runs, or write into the
/// root, and one that leaves the job's ids unable to reach their directories.
#[test]
fn the_searchable_layout_takes_0711_and_refuses_every_listable_writable_or_closed_mode() {
    let parent = world("layout-modes");
    let root = dir(parent.path(), "root", 0o711);
    let cases = [
        (0o711, true),
        (0o701, true),
        (0o700, false),
        (0o710, false),
        (0o755, false),
        (0o744, false),
        (0o733, false),
        (0o702, false),
        (0o771, false),
        (0o750, false),
    ];
    for (bits, accepted) in cases {
        set_mode(&root, bits);
        let scratch = ScratchRoot::open_with(&root, DirLayout::Searchable);
        let files = FilesRoot::open_with(&root, DirLayout::Searchable);
        assert_eq!(scratch.is_ok(), accepted, "scratch root mode {bits:o}");
        assert_eq!(files.is_ok(), accepted, "files root mode {bits:o}");
        if !accepted {
            let why = scratch.unwrap_err();
            assert!(why.starts_with("--scratch-root: "), "{why}");
            assert!(
                why.contains("search") || why.contains("read or write"),
                "{why}"
            );
        }
    }
    set_mode(&root, 0o711);
    assert_eq!(
        ScratchRoot::open_with(&root, DirLayout::Searchable)
            .unwrap()
            .layout(),
        DirLayout::Searchable
    );
}

/// The `Searchable` rule also needs every directory above the root searchable by others:
/// the job's ids walk down from `/`. A root of the right mode below a closed directory is
/// refused, naming the closed directory. It catches an agent that starts and then fails
/// every job at its first bind mount.
#[test]
fn a_searchable_root_below_a_directory_others_cannot_search_is_refused_by_name() {
    let parent = world("layout-ancestor");
    let closed = dir(parent.path(), "closed", 0o700);
    let root = dir(&closed, "root", 0o711);
    let why = ScratchRoot::open_with(&root, DirLayout::Searchable).unwrap_err();
    let closed_real = fs::canonicalize(&closed).unwrap();
    assert!(why.contains("not searchable by others"), "{why}");
    assert!(why.contains(closed_real.to_str().unwrap()), "{why}");
    assert!(FilesRoot::open_with(&root, DirLayout::Searchable).is_err());
    assert!(
        ScratchRoot::open_with(&root, DirLayout::Private).is_err(),
        "a 0711 root is refused by the private rule anyway"
    );
    set_mode(&closed, 0o755);
    ScratchRoot::open_with(&root, DirLayout::Searchable).unwrap();
}

/// BEFORE: a private scratch root makes each run's directory `0700` whatever the umask.
#[test]
fn a_private_scratch_root_makes_each_runs_directory_0700() {
    let parent = world("layout-scratch-before");
    let root = dir(parent.path(), "root", 0o700);
    let scratch = ScratchRoot::open(&root).unwrap();
    let made = scratch.create("r1").unwrap();
    assert_eq!(mode(&made), 0o700);
}

/// AFTER: a searchable scratch root makes each run's directory `0711` whatever the umask
/// (the parent of a lease: the job's ids pass through it to the directories the agent gives
/// them), replaces a stale one, and removes it. It catches a run directory the job's ids
/// cannot pass through, and one a job could list.
#[test]
fn a_searchable_scratch_root_makes_each_runs_directory_0711() {
    let parent = world("layout-scratch-after");
    let root = dir(parent.path(), "root", 0o711);
    let scratch = ScratchRoot::open_with(&root, DirLayout::Searchable).unwrap();
    let made = scratch.create("r1").unwrap();
    assert_eq!(mode(&made), 0o711);
    fs::write(made.join("leftover"), "x").unwrap();
    let again = scratch.create("r1").unwrap();
    assert!(
        !again.join("leftover").exists(),
        "a stale directory is replaced"
    );
    assert_eq!(mode(&again), 0o711);
    scratch.remove("r1").unwrap();
    assert!(!made.exists());
}

/// BEFORE: a private files root hands the job a `0555` directory of `0444` files, the
/// agent's, readable by the container's user without a hand-over.
#[test]
fn a_private_files_root_writes_a_job_readable_closed_directory() {
    let parent = world("layout-files-before");
    let root = dir(parent.path(), "files", 0o700);
    let files = FilesRoot::open(&root).unwrap();
    let made = files.write("r1", &[file("grant", "token")]).unwrap();
    assert_eq!(mode(&made), 0o555);
    assert_eq!(mode(&made.join("grant")), 0o444);
    let refused = files.hand_over("r1", me().0, me().1).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Unsupported);
}

/// AFTER: a searchable files root is made `0711` when missing, writes the run's directory
/// `0700` and its files `0400`, the agent's, so a job that was not handed its files, and
/// every other user, reads nothing; `hand_over` gives them to the job's ids and closes the
/// directory to `0500`. It catches secrets readable by every user who can walk to them, and
/// a hand-over that leaves the directory writable by the job's ids.
#[test]
fn a_searchable_files_root_keeps_the_run_closed_until_it_is_handed_over() {
    let parent = world("layout-files-after");
    let root = parent.path().join("files");
    let files = FilesRoot::open_with(&root, DirLayout::Searchable).unwrap();
    assert_eq!(
        mode(&root),
        0o711,
        "a missing root is made with the layout's mode"
    );
    let made = files
        .write(
            "r1",
            &[
                file("grant", "token"),
                file("put-url", "https://store.example/put"),
            ],
        )
        .unwrap();
    assert_eq!(mode(&made), 0o700);
    assert_eq!(mode(&made.join("grant")), 0o400);
    assert_eq!(fs::read_to_string(made.join("grant")).unwrap(), "token");

    let (uid, gid) = me();
    files.hand_over("r1", uid, gid).unwrap();
    assert_eq!(mode(&made), 0o500);
    assert_eq!(mode(&made.join("grant")), 0o400);
    assert_eq!(mode(&made.join("put-url")), 0o400);

    for bad in ["", ".", "..", "a/b"] {
        assert_eq!(
            files.hand_over(bad, uid, gid).unwrap_err().kind(),
            ErrorKind::InvalidInput,
            "{bad:?}"
        );
    }
    assert_eq!(
        files.hand_over("absent", uid, gid).unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

/// A handed-over (`0500`) run directory is still removed by the agent that made it, and
/// swept at a restart; a handed-over directory the agent could not open would leave its
/// secrets behind.
#[test]
fn a_handed_over_run_directory_is_removed_and_swept() {
    let parent = world("layout-files-remove");
    let root = parent.path().join("files");
    let files = FilesRoot::open_with(&root, DirLayout::Searchable).unwrap();
    let (uid, gid) = me();
    for run in ["r1", "r2"] {
        files.write(run, &[file("grant", "token")]).unwrap();
        files.hand_over(run, uid, gid).unwrap();
    }
    files.remove("r1").unwrap();
    assert!(!files.of("r1").exists());
    assert_eq!(files.sweep([]).unwrap(), 1);
    assert!(!files.of("r2").exists());
}
