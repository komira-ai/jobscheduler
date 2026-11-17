//! [`Kernel`]'s calls, made for real with arguments every kernel refuses, as root or not
//! (a pipe is on no mount a caller may clone, set or move; a path that is not there cannot
//! be mounted or bound), so these tests change no mount of the host whoever runs them. The
//! calls that succeed are made by the root suite (`tests/crun_root/job_fs.rs`).

use std::os::fd::AsFd as _;

use super::kernel::{checked, mount_attr, mount_setattr, overlay_flags, probe, set_fs_ids};
use super::*;

/// A scratch directory for one test.
fn scratch(name: &str) -> crate::test_dir::TestDir {
    crate::unit_dir(&format!("job-fs-kernel-{name}"))
}

/// Catches a call that reaches the kernel with the wrong descriptor or path, or succeeds
/// where it cannot: each call on a pipe, or at a point that is not there, fails.
#[test]
fn each_call_is_made_and_its_refusal_returned() {
    let dir = scratch("refused");
    let missing = dir.join("missing");
    let (pipe, _writer) = io::pipe().expect("pipe");
    let kernel = Kernel;
    assert!(kernel.clone_tree(pipe.as_fd()).is_err());
    assert!(
        kernel
            .set_attributes(pipe.as_fd(), Attributes::default())
            .is_err()
    );
    assert!(kernel.attach(pipe.as_fd(), &missing).is_err());
    let options = "lowerdir=/missing-lower,upperdir=/missing-upper,workdir=/missing-work";
    assert!(kernel.mount_overlay(&missing, false, options).is_err());
    assert!(
        kernel.detach(&dir).is_err(),
        "a directory that is not mounted"
    );
    assert!(kernel.bind_onto_itself(&missing).is_err());
    assert!(kernel.make_private(&missing).is_err());
    let nul = kernel
        .mount_overlay(&missing, false, "lowerdir=/a\0b")
        .expect_err("a NUL");
    assert_eq!(nul.kind(), io::ErrorKind::InvalidInput);
}

/// Catches a mount point test that compares a directory with itself, or reads the wrong
/// mount: a plain directory is not a mount point, and one that is not there is an error.
#[test]
fn a_plain_directory_is_not_a_mount_point() {
    let dir = scratch("plain");
    std::fs::create_dir(dir.join("child")).expect("mkdir");
    let kernel = Kernel;
    assert!(!kernel.is_mount_point(&dir.join("child")).expect("statx"));
    let why = kernel
        .is_mount_point(&dir.join("missing"))
        .expect_err("missing");
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
}

/// Catches a clone that is not private, `nosuid` and `nodev` whatever is asked, a
/// read-only or `noexec` clone without its flag, or a flag given that was not asked for,
/// or anything cleared (a source read-only on the host made writable).
#[test]
fn a_clone_is_private_nosuid_and_nodev_with_what_is_asked() {
    let base = libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV;
    let cases = [
        (false, false, base),
        (true, false, base | libc::MOUNT_ATTR_RDONLY),
        (false, true, base | libc::MOUNT_ATTR_NOEXEC),
        (
            true,
            true,
            base | libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOEXEC,
        ),
    ];
    for (read_only, no_exec, set) in cases {
        let attr = mount_attr(Attributes { read_only, no_exec });
        assert_eq!(attr.attr_set, set, "{read_only} {no_exec}");
        assert_eq!(attr.attr_clr, 0);
        assert_eq!(attr.propagation, libc::MS_PRIVATE);
        assert_eq!(attr.userns_fd, 0);
    }
}

/// Catches `mount_setattr` reaching the kernel with its flags out of place: a flag no
/// kernel knows is refused `EINVAL` before any permission or path is looked at, so the
/// refusal is the kernel's reading of the flags this process passed, as root or not.
#[test]
fn mount_setattr_passes_its_flags_to_the_kernel() {
    let (pipe, _writer) = io::pipe().expect("pipe");
    let attr = mount_attr(Attributes::default());
    let unknown = libc::AT_EMPTY_PATH | 1 << 30;
    let why = mount_setattr(pipe.as_fd(), unknown, &attr).expect_err("unknown flag");
    assert_eq!(why.raw_os_error(), Some(libc::EINVAL));
}

/// Catches a raw system call's answer read the wrong way round: 0 is success, and -1 is
/// the error the failed call left in this thread's `errno`.
#[test]
fn a_raw_answer_of_zero_is_success() {
    let dir = scratch("errno");
    checked(0).expect("0 is success");
    // A call through the C library that fails sets `errno`, which `checked` reads back.
    let _ = std::fs::metadata(dir.join("missing"));
    assert_eq!(
        checked(-1).expect_err("-1").raw_os_error(),
        Some(libc::ENOENT)
    );
}

/// Catches an overlay mounted without `nosuid` or `nodev`, or `noexec` not given or given
/// unasked.
#[test]
fn an_overlay_is_nosuid_and_nodev_and_noexec_when_asked() {
    use rustix::mount::MountFlags;
    assert_eq!(overlay_flags(false), MountFlags::NOSUID | MountFlags::NODEV);
    assert_eq!(
        overlay_flags(true),
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC
    );
}

/// Catches a probe that changes something, names a call wrongly, or reads an answer of a
/// kernel that has the calls as their absence: on this kernel (5.12 or later) none of the
/// four is `ENOSYS`, the two opens succeed, and the two calls on an empty path are refused.
#[test]
fn the_probe_finds_each_call_and_changes_nothing() {
    let answers = probe();
    let names: Vec<&str> = answers.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        ["openat2", "open_tree", "move_mount", "mount_setattr"]
    );
    let [openat2, open_tree, move_mount, mount_setattr] = answers;
    openat2.1.expect("openat2 opens /");
    open_tree.1.expect("open_tree opens /");
    for (name, answer) in [move_mount, mount_setattr] {
        let why = answer.expect_err(name);
        assert_ne!(why.raw_os_error(), Some(libc::ENOSYS), "{name}");
    }
}

/// Runs `work` in a thread of its own, which is what every call that changes the calling
/// thread's root, directory, namespace or ids must be made from.
fn in_thread<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(work).join().expect("the thread ended"))
}

/// Catches a source opened through a link, or one that is not told apart from a directory:
/// a temp directory and a temp file open, `fstat` says which is a directory, a link is
/// opened as itself, and a path that is not there is `ENOENT`.
#[test]
fn a_source_is_opened_and_told_apart() {
    let dir = scratch("source");
    std::fs::write(dir.join("file"), "x").expect("write");
    let kernel = Kernel;
    let opened = kernel.open_source(&dir).expect("open the directory");
    assert!(kernel.is_dir(opened.as_fd()).expect("fstat"));
    let opened = kernel
        .open_source(&dir.join("file"))
        .expect("open the file");
    assert!(!kernel.is_dir(opened.as_fd()).expect("fstat"));
    let why = kernel
        .open_source(&dir.join("missing"))
        .expect_err("missing");
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
    std::os::unix::fs::symlink(&*dir, dir.join("link")).expect("link");
    let link = kernel
        .open_source(&dir.join("link"))
        .expect("the link itself");
    assert!(!kernel.is_dir(link.as_fd()).expect("fstat"), "not followed");
}

/// Catches mount points made with an error for what is there already: a directory and a
/// file are made, and made again, in a temp directory; a directory below one that is not
/// there is `ENOENT`.
#[test]
fn a_mount_point_is_made_once_and_found_the_second_time() {
    let dir = scratch("points");
    let kernel = Kernel;
    kernel.make_dir(&dir.join("d")).expect("make a directory");
    kernel.make_dir(&dir.join("d")).expect("it is there");
    kernel.make_file(&dir.join("f")).expect("make a file");
    kernel.make_file(&dir.join("f")).expect("it is there");
    assert!(dir.join("d").is_dir());
    assert!(dir.join("f").is_file());
    let why = kernel
        .make_dir(&dir.join("absent").join("d"))
        .expect_err("no parent");
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
}

/// Catches a namespace or root opened from the wrong path: this process's own open, and a
/// process that is not there (the highest pid there can be) does not.
#[test]
fn the_namespace_and_root_of_a_process_open_and_of_none_do_not() {
    let kernel = Kernel;
    let pid = std::process::id();
    kernel.open_namespace(pid).expect("own namespace");
    let root = kernel.open_root(pid).expect("own root");
    assert!(kernel.is_dir(root.as_fd()).expect("fstat"));
    let none = u32::try_from(i32::MAX).expect("fits");
    assert_eq!(
        kernel.open_namespace(none).expect_err("no process").kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        kernel.open_root(none).expect_err("no process").kind(),
        io::ErrorKind::NotFound
    );
}

/// Catches the thread-level calls made wrongly: in a thread of its own the file system
/// attributes are unshared, the directory is changed to a temp directory's descriptor, the
/// thread joins its own mount namespace (refused `EPERM` unless root, which may), and
/// changes its root (refused `EPERM` unless root, who changes the root of the thread
/// alone, which ends with it).
#[test]
fn a_thread_unshares_changes_directory_joins_and_changes_root() {
    let dir = scratch("thread");
    let kernel = Kernel;
    let (join, root) = in_thread(|| {
        kernel.unshare_fs().expect("unshare");
        let at = kernel.open_source(&dir).expect("open");
        kernel.change_dir(at.as_fd()).expect("fchdir");
        assert_eq!(std::env::current_dir().expect("cwd"), *dir);
        let namespace = kernel.open_namespace(std::process::id()).expect("own");
        (kernel.join(namespace.as_fd()), kernel.change_root())
    });
    let unprivileged = rustix::process::geteuid().as_raw() != 0;
    assert_eq!(join.is_err(), unprivileged, "{join:?}");
    assert_eq!(root.is_err(), unprivileged, "{root:?}");
    if let Err(why) = join {
        assert_eq!(why.raw_os_error(), Some(libc::EPERM));
    }
    if let Err(why) = root {
        assert_eq!(why.raw_os_error(), Some(libc::EPERM));
    }
}

/// Catches ids set that the kernel refused being taken for done: the thread's own ids are
/// set; another user, or another group, is refused `PermissionDenied` unless the thread may
/// (root, which then has changed only that thread's ids).
#[test]
fn a_thread_takes_its_own_ids_and_is_refused_others() {
    let kernel = Kernel;
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    let unprivileged = uid != 0;
    in_thread(|| {
        kernel.set_fs_ids((uid, gid)).expect("own ids");
        let other_user = kernel.set_fs_ids((uid + 1, gid));
        let other_group = kernel.set_fs_ids((uid, gid + 1));
        assert_eq!(other_user.is_err(), unprivileged, "{other_user:?}");
        assert_eq!(other_group.is_err(), unprivileged, "{other_group:?}");
        if let Err(why) = other_user.and(other_group) {
            assert_eq!(why.kind(), io::ErrorKind::PermissionDenied);
        }
    });
}

/// Catches an id check that trusts the first answer, or tests the wrong id: with setters
/// that take the id, both are set, and each id that is refused (the setter answers the id
/// it had) fails the whole with `PermissionDenied`, the group checked first.
#[test]
fn an_id_that_did_not_take_is_an_error() {
    let had = std::cell::Cell::new(5);
    let takes = |id: u32| had.replace(i32::try_from(id).expect("id"));
    let refuses = |_: u32| 5;
    set_fs_ids((7, 8), takes, takes).expect("both take");
    let why = set_fs_ids((7, 8), refuses, takes).expect_err("group refused");
    assert_eq!(why.kind(), io::ErrorKind::PermissionDenied);
    let why = set_fs_ids((7, 8), takes, refuses).expect_err("user refused");
    assert_eq!(why.kind(), io::ErrorKind::PermissionDenied);
}
