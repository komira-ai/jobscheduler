//! [`open_root`] and [`open_beneath`] against directories, files and links in a scratch
//! directory: the kernel's resolution itself, which needs no privilege.

use std::os::fd::AsFd as _;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::resolve::{RESOLVE_ATTEMPTS, resolve_with};
use super::*;
use crate::walk::{Identity, identity};
use rustix::io::Errno;

/// A scratch directory for one test.
fn scratch(name: &str) -> crate::test_dir::TestDir {
    crate::unit_dir(&format!("job-fs-resolve-{name}"))
}

/// The device and inode of the directory at `path`.
fn id_of(path: &Path) -> Identity {
    identity(&open_root(path).expect("open")).expect("identity")
}

/// A root `root` with `root/tree/sub/file`, a file `root/plain`, a directory outside it
/// (`outside`), and links in the tree: one to inside the root, one absolute to outside it,
/// one relative that climbs out of it, one to the tree's own sub-directory and one that
/// climbs out of the tree but stays in the root.
fn layout(dir: &Path) -> PathBuf {
    let root = dir.join("root");
    std::fs::create_dir_all(root.join("tree/sub")).expect("mkdir");
    std::fs::create_dir_all(root.join("other")).expect("mkdir");
    std::fs::create_dir_all(dir.join("outside")).expect("mkdir");
    std::fs::write(root.join("tree/sub/file"), b"f").expect("write");
    std::fs::write(root.join("plain"), b"p").expect("write");
    symlink("../other", root.join("tree/inside")).expect("symlink");
    symlink(dir.join("outside"), root.join("tree/absolute")).expect("symlink");
    symlink("../../outside", root.join("tree/climbs")).expect("symlink");
    symlink("sub", root.join("tree/own")).expect("symlink");
    symlink("../../other", root.join("tree/sub/up")).expect("symlink");
    root
}

/// Catches a path opened somewhere else than where it names, an empty path that does not
/// name the directory itself, and a link inside the root not followed: each opens the
/// directory it leads to.
#[test]
fn a_path_beneath_its_root_opens_the_directory_it_names() {
    let dir = scratch("names");
    let root = layout(&dir);
    let at = open_root(&root).expect("the root");
    let open = |rest: &str| identity(&open_beneath(at.as_fd(), Path::new(rest)).expect(rest));
    assert_eq!(
        open("tree/sub").expect("identity"),
        id_of(&root.join("tree/sub"))
    );
    assert_eq!(open("").expect("identity"), id_of(&root));
    assert_eq!(
        open("tree/inside").expect("identity"),
        id_of(&root.join("other"))
    );
    assert_eq!(
        open("tree/own").expect("identity"),
        id_of(&root.join("tree/sub"))
    );
}

/// Catches a path that leads out of its root being opened: an absolute path, a `..` above
/// the root, an absolute link and a relative link that climbs out are each refused as
/// leading out, naming the path as given.
#[test]
fn a_path_that_leads_out_of_its_root_is_refused() {
    let dir = scratch("escapes");
    let root = layout(&dir);
    let at = open_root(&root).expect("the root");
    for rest in ["tree/absolute", "tree/climbs", "../outside", "/"] {
        let why = open_beneath(at.as_fd(), Path::new(rest)).expect_err(rest);
        assert!(
            matches!(&why, ResolveError::Escapes { path } if path == Path::new(rest)),
            "{rest}: {why}"
        );
    }
    let why = open_beneath(at.as_fd(), Path::new("tree/climbs")).expect_err("climbs");
    assert_eq!(
        why.to_string(),
        "tree/climbs leads out of the directory it is opened beneath"
    );
}

/// Catches a sub-path resolved beneath the root rather than beneath its host path: a link
/// that stays in the root but leaves the tree opens from the root and is refused from the
/// opened tree, and one to the tree's own directory is followed.
#[test]
fn a_sub_path_is_resolved_beneath_its_host_path() {
    let dir = scratch("sub-path");
    let root = layout(&dir);
    let at = open_root(&root).expect("the root");
    let tree = open_beneath(at.as_fd(), Path::new("tree")).expect("the tree");
    open_beneath(at.as_fd(), Path::new("tree/sub/up")).expect("inside the root");
    let up = open_beneath(tree.as_fd(), Path::new("sub/up")).expect_err("leaves the tree");
    assert!(matches!(up, ResolveError::Escapes { .. }), "{up}");
    let own = open_beneath(tree.as_fd(), Path::new("own")).expect("in the tree");
    assert_eq!(
        identity(&own).expect("identity"),
        id_of(&root.join("tree/sub"))
    );
}

/// Catches a path that cannot be opened as a directory being reported as leading out, or
/// opened: a missing path, a file, and a directory below one that cannot be searched are
/// unavailable, with the kernel's answer.
#[test]
fn a_missing_path_a_file_or_an_unsearchable_one_is_unavailable() {
    let dir = scratch("unavailable");
    let root = layout(&dir);
    let at = open_root(&root).expect("the root");
    // The kernel's answer of a path that is unavailable, as the error's source.
    let kind = |rest: &str| {
        let why = open_beneath(at.as_fd(), Path::new(rest)).expect_err(rest);
        assert_eq!(why.to_string().split(':').next(), Some(rest), "{why}");
        std::error::Error::source(&why)
            .and_then(|source| source.downcast_ref::<io::Error>())
            .and_then(io::Error::raw_os_error)
    };
    assert_eq!(kind("missing"), Some(libc::ENOENT));
    assert_eq!(kind("plain"), Some(libc::ENOTDIR));
    assert_eq!(kind("tree/sub/file"), Some(libc::ENOTDIR));
    let locked = root.join("tree");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let denied = kind("tree/sub");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(denied, Some(libc::EACCES));
    let why = open_beneath(at.as_fd(), Path::new("missing")).expect_err("missing");
    assert_eq!(
        why.to_string(),
        "missing: No such file or directory (os error 2)"
    );
}

/// Catches a root that is not a directory, or not there, being opened.
#[test]
fn a_root_that_is_not_a_directory_is_unavailable() {
    let dir = scratch("root");
    let root = layout(&dir);
    for path in [root.join("plain"), root.join("missing")] {
        let why = open_root(&path).expect_err("not a directory");
        assert!(
            matches!(&why, ResolveError::Unavailable { path: p, .. } if *p == path),
            "{why}"
        );
    }
}

/// Catches a descriptor that follows the path's text after it was opened: once opened, the
/// directory is the one it was, though the path is renamed away and a link to outside the
/// root put in its place.
#[test]
fn a_path_swapped_for_a_link_after_it_was_opened_changes_nothing() {
    let dir = scratch("swapped");
    let root = layout(&dir);
    let at = open_root(&root).expect("the root");
    let before = id_of(&root.join("tree/sub"));
    let opened = open_beneath(at.as_fd(), Path::new("tree/sub")).expect("opened");
    std::fs::rename(root.join("tree/sub"), root.join("moved")).expect("rename");
    symlink(dir.join("outside"), root.join("tree/sub")).expect("symlink");
    assert_eq!(identity(&opened).expect("identity"), before);
    let again = open_beneath(at.as_fd(), Path::new("tree/sub")).expect_err("now a link out");
    assert!(matches!(again, ResolveError::Escapes { .. }), "{again}");
}

/// An `openat2` that answers `EAGAIN` the first `races` times it is called and then opens
/// `dir` again, counting its calls in `calls` and recording the paths it was given.
fn racing<'a>(
    dir: &'a OwnedFd,
    races: usize,
    calls: &'a mut Vec<PathBuf>,
) -> impl FnMut(&Path) -> Result<OwnedFd, Errno> + 'a {
    move |path| {
        calls.push(path.to_owned());
        match calls.len() <= races {
            true => Err(Errno::AGAIN),
            false => Ok(dir.try_clone().expect("a clone")),
        }
    }
}

/// Catches a resolution a rename or a mount raced being given up at once, or made again
/// for another answer: `EAGAIN` once, and on every attempt but the last, is followed by
/// another call with the same path, which opens the directory; an empty path is resolved
/// as `.` each time.
#[test]
fn a_resolution_raced_by_a_rename_is_made_again() {
    let dir = scratch("raced");
    let at = open_root(&dir).expect("the root");
    for (rest, given, races) in [
        ("sub/up", "sub/up", 1),
        ("sub/up", "sub/up", RESOLVE_ATTEMPTS - 1),
        ("", ".", RESOLVE_ATTEMPTS - 1),
    ] {
        let mut calls = Vec::new();
        let opened = resolve_with(Path::new(rest), &mut racing(&at, races, &mut calls))
            .expect("opened on a later attempt");
        assert_eq!(identity(&opened).expect("identity"), id_of(&dir));
        assert_eq!(calls, vec![PathBuf::from(given); races + 1]);
    }
}

/// Catches a resolution made again without bound, or one whose last `EAGAIN` is reported
/// as anything but the path being unavailable with the kernel's answer: after a fixed
/// number of attempts, each raced, the path is unavailable, naming it.
#[test]
fn a_resolution_raced_on_every_attempt_is_unavailable() {
    let dir = scratch("always-raced");
    let at = open_root(&dir).expect("the root");
    let mut calls = Vec::new();
    let why = resolve_with(
        Path::new("sub/up"),
        &mut racing(&at, usize::MAX, &mut calls),
    )
    .expect_err("raced every time");
    assert_eq!(calls.len(), RESOLVE_ATTEMPTS);
    assert!(
        matches!(&why, ResolveError::Unavailable { path, .. } if path == Path::new("sub/up")),
        "{why}"
    );
    assert_eq!(
        why.to_string(),
        "sub/up: Resource temporarily unavailable (os error 11)"
    );
}

/// Catches an answer other than `EAGAIN` being taken for a race: a path that leads out, or
/// one that is missing, is answered after one call.
#[test]
fn an_answer_other_than_a_race_is_not_made_again() {
    for (errno, escapes) in [(Errno::XDEV, true), (Errno::NOENT, false)] {
        let mut calls = 0;
        let why = resolve_with(Path::new("x"), &mut |_| {
            calls += 1;
            Err(errno)
        })
        .expect_err("refused");
        assert_eq!(calls, 1);
        assert_eq!(
            matches!(why, ResolveError::Escapes { .. }),
            escapes,
            "{why}"
        );
    }
}
