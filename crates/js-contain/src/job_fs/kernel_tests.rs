//! [`Kernel`]'s calls, made for real with arguments every kernel refuses, as root or not
//! (a pipe is on no mount a caller may clone, set or move; a path that is not there cannot
//! be mounted or bound), so these tests change no mount of the host whoever runs them. The
//! calls that succeed are made by the root suite (`tests/crun_root/job_fs.rs`).

use std::os::fd::AsFd as _;

use super::kernel::{checked, mount_attr, mount_setattr, overlay_flags, probe};
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
