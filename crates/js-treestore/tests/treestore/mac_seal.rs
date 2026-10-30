//! The flag sealer, over a fake that records the flags it is asked to set, so that the walk
//! runs on every platform. What `uchg` does to a write is for the macOS tests.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_testkit::dir::TestDir;
use js_treestore::Sealer;
use js_treestore::mac::{Cloner, FlagSealer, IMMUTABLE, Owner, UserFlags};

#[derive(Clone, Default)]
struct FakeFlags {
    flags: Arc<Mutex<BTreeMap<u64, u32>>>,
    touched: Arc<Mutex<BTreeSet<u64>>>,
    /// Inodes in the order their flags were set.
    order: Arc<Mutex<Vec<u64>>>,
    /// Inodes that carry an access control list.
    acls: Arc<Mutex<BTreeSet<u64>>>,
    fail: Arc<AtomicUsize>,
}

impl UserFlags for FakeFlags {
    fn set(&self, file: BorrowedFd<'_>, flags: u32) -> io::Result<()> {
        let failing = self.fail.load(Ordering::SeqCst);
        if failing > 0 {
            self.fail.store(failing - 1, Ordering::SeqCst);
            return Err(io::Error::other("fchflags refused"));
        }
        let ino = rustix::fs::fstat(file)?.st_ino;
        self.flags.lock().unwrap().insert(ino, flags);
        self.touched.lock().unwrap().insert(ino);
        self.order.lock().unwrap().push(ino);
        Ok(())
    }

    fn has_acl(&self, file: BorrowedFd<'_>) -> io::Result<bool> {
        let ino = rustix::fs::fstat(file)?.st_ino;
        Ok(self.acls.lock().unwrap().contains(&ino))
    }

    fn clear_acl(&self, file: BorrowedFd<'_>) -> io::Result<()> {
        let ino = rustix::fs::fstat(file)?.st_ino;
        self.acls.lock().unwrap().remove(&ino);
        Ok(())
    }
}

impl FakeFlags {
    fn of(&self, path: &Path) -> u32 {
        let ino = std::fs::symlink_metadata(path).unwrap().ino();
        self.flags.lock().unwrap().get(&ino).copied().unwrap_or(0)
    }

    fn plant_acl(&self, path: &Path) {
        let ino = std::fs::symlink_metadata(path).unwrap().ino();
        self.acls.lock().unwrap().insert(ino);
    }

    fn acl_of(&self, path: &Path) -> bool {
        let ino = std::fs::symlink_metadata(path).unwrap().ino();
        self.acls.lock().unwrap().contains(&ino)
    }

    fn touched(&self, path: &Path) -> bool {
        let ino = std::fs::symlink_metadata(path).unwrap().ino();
        self.touched.lock().unwrap().contains(&ino)
    }

    /// Where the flags of `path` were first set since the order was last forgotten.
    fn position(&self, path: &Path) -> usize {
        let ino = std::fs::symlink_metadata(path).unwrap().ino();
        let order = self.order.lock().unwrap();
        order
            .iter()
            .position(|seen| *seen == ino)
            .unwrap_or_else(|| panic!("flags of {} were never set", path.display()))
    }

    fn forget_order(&self) {
        self.order.lock().unwrap().clear();
    }

    fn flagged(&self) -> usize {
        self.flags
            .lock()
            .unwrap()
            .values()
            .filter(|flags| **flags != 0)
            .count()
    }
}

/// Where the open directory is, for a fake that works with paths.
#[cfg(target_os = "linux")]
fn fd_path(fd: BorrowedFd<'_>) -> PathBuf {
    use std::os::fd::AsRawFd as _;
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap()
}

#[cfg(target_os = "macos")]
fn fd_path(fd: BorrowedFd<'_>) -> PathBuf {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut buf = [0 as libc::c_char; libc::PATH_MAX as usize];
    // SAFETY: `F_GETPATH` writes a NUL-terminated path of at most PATH_MAX bytes into
    // `buf`, which is that long.
    assert_eq!(
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) },
        0
    );
    // SAFETY: the kernel NUL-terminated `buf`.
    let path = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    PathBuf::from(OsStr::from_bytes(path.to_bytes()))
}

/// A copy that keeps modes, links and pipes, and gives each new inode the flags of the one
/// it copies, as `clonefile` does; it shares no blocks.
#[derive(Clone)]
struct FakeClone {
    flags: FakeFlags,
}

impl FakeClone {
    fn copy(&self, src: &Path, dst: &Path, seen: &mut BTreeMap<u64, PathBuf>) -> io::Result<()> {
        let meta = std::fs::symlink_metadata(src)?;
        let kind = meta.file_type();
        if kind.is_dir() {
            std::fs::create_dir(dst)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                self.copy(&entry.path(), &dst.join(entry.file_name()), seen)?;
            }
            std::fs::set_permissions(dst, meta.permissions())?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        } else if kind.is_fifo() {
            let path = std::ffi::CString::new(dst.to_str().unwrap()).unwrap();
            // SAFETY: `path` is NUL-terminated and outlives the call.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        } else if let Some(first) = seen.get(&meta.ino()) {
            std::fs::hard_link(first, dst)?;
        } else {
            std::fs::copy(src, dst)?;
            seen.insert(meta.ino(), dst.to_owned());
        }
        if !kind.is_symlink() {
            self.give_flags(&meta, dst);
        }
        Ok(())
    }

    fn give_flags(&self, from: &std::fs::Metadata, to: &Path) {
        let ino = std::fs::symlink_metadata(to).unwrap().ino();
        let flags = self.flags.flags.lock().unwrap().get(&from.ino()).copied();
        if let Some(flags) = flags {
            self.flags.flags.lock().unwrap().insert(ino, flags);
        }
        if self.flags.acls.lock().unwrap().contains(&from.ino()) {
            self.flags.acls.lock().unwrap().insert(ino);
        }
    }
}

impl Cloner for FakeClone {
    fn clone_dir(&self, src: &Path, dst_dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
        self.copy(src, &fd_path(dst_dir).join(name), &mut BTreeMap::new())
    }

    fn clone_file(
        &self,
        src: BorrowedFd<'_>,
        dst_dir: BorrowedFd<'_>,
        name: &OsStr,
    ) -> io::Result<()> {
        let from = rustix::fs::fstat(src)?;
        let mut input = std::fs::File::from(src.try_clone_to_owned()?);
        let out: OwnedFd = rustix::fs::openat(
            dst_dir,
            name,
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL,
            rustix::fs::Mode::from_bits_truncate(from.st_mode),
        )?;
        let to = rustix::fs::fstat(&out)?;
        io::copy(&mut input, &mut std::fs::File::from(out))?;
        let flags = self.flags.flags.lock().unwrap().get(&from.st_ino).copied();
        if let Some(flags) = flags {
            self.flags.flags.lock().unwrap().insert(to.st_ino, flags);
        }
        Ok(())
    }
}

struct Rig {
    dir: TestDir,
    flags: FakeFlags,
    sealer: Arc<FlagSealer<FakeFlags, FakeClone>>,
}

impl Rig {
    fn new() -> Rig {
        let flags = FakeFlags::default();
        Rig {
            dir: TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-seal"),
            sealer: Arc::new(FlagSealer::new(
                Owner::caller(),
                flags.clone(),
                FakeClone {
                    flags: flags.clone(),
                },
            )),
            flags,
        }
    }

    /// `root/{a.txt, run.sh, link -> a.txt, sub/{b.txt, deeper/c.txt}}`.
    fn tree(&self) -> PathBuf {
        let root = self.dir.path().join("tree");
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("a.txt"), b"alpha").unwrap();
        std::fs::write(root.join("run.sh"), b"#!/bin/sh\n").unwrap();
        std::fs::write(root.join("sub/b.txt"), b"bravo").unwrap();
        std::fs::write(root.join("sub/deeper/c.txt"), b"charlie").unwrap();
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::set_permissions(root.join("a.txt"), std::fs::Permissions::from_mode(0o664))
            .unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
        root
    }

    /// Unseals at the end of the test, so the directory can be removed.
    fn guard(&self, root: &Path) -> Cleanup {
        Cleanup {
            sealer: self.sealer.clone(),
            root: root.to_owned(),
        }
    }
}

struct Cleanup {
    sealer: Arc<FlagSealer<FakeFlags, FakeClone>>,
    root: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.sealer.unseal_for_delete(&self.root);
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn blocks_below(path: &Path) -> u64 {
    let meta = std::fs::symlink_metadata(path).unwrap();
    let mut total = meta.blocks() * 512;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            total += blocks_below(&entry.unwrap().path());
        }
    }
    total
}

fn nest(root: &Path, levels: usize) {
    let mut path = root.to_owned();
    for _ in 0..levels {
        path.push("d");
    }
    std::fs::create_dir_all(path).unwrap();
}

#[test]
fn sealing_makes_every_file_and_directory_immutable_and_leaves_links_alone() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    rig.sealer.seal(&root).unwrap();
    for path in [
        "",
        "a.txt",
        "run.sh",
        "sub",
        "sub/b.txt",
        "sub/deeper",
        "sub/deeper/c.txt",
    ] {
        assert_eq!(rig.flags.of(&root.join(path)), IMMUTABLE, "{path}");
    }
    assert!(!rig.flags.touched(&root.join("link")));
    assert_eq!(rig.flags.flagged(), 7);
}

#[test]
fn sealing_normalises_modes_and_gives_the_tree_to_the_owner() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    rig.sealer.seal(&root).unwrap();
    assert_eq!(mode(&root), 0o555);
    assert_eq!(mode(&root.join("sub/deeper")), 0o555);
    assert_eq!(mode(&root.join("a.txt")), 0o444);
    assert_eq!(mode(&root.join("run.sh")), 0o555);
    let me = rustix::process::getuid().as_raw();
    assert_eq!(std::fs::metadata(root.join("sub/b.txt")).unwrap().uid(), me);
    assert_eq!(
        std::fs::symlink_metadata(root.join("link")).unwrap().uid(),
        me
    );
}

#[test]
fn the_root_owner_is_root_and_wheel() {
    assert_eq!(Owner::root().ids(), (0, 0));
    let me = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    assert_eq!(Owner::caller().ids(), me);
}

#[test]
fn sealing_reports_the_blocks_every_entry_takes() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    let sealed = rig.sealer.seal(&root).unwrap();
    assert_eq!(sealed.bytes, blocks_below(&root));
    assert!(sealed.bytes > 0);
}

#[test]
fn sealing_a_sealed_tree_again_is_accepted() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    let first = rig.sealer.seal(&root).unwrap();
    let second = rig.sealer.seal(&root).unwrap();
    assert_eq!(first, second);
    assert_eq!(rig.flags.of(&root.join("sub")), IMMUTABLE);
}

#[test]
fn a_file_with_a_second_hard_link_is_refused() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    std::fs::hard_link(root.join("a.txt"), root.join("sub/again")).unwrap();
    let error = rig.sealer.seal(&root).unwrap_err();
    assert!(error.0.contains("hard link"), "{error}");
}

#[test]
fn a_fifo_is_refused() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    let path = std::ffi::CString::new(root.join("pipe").to_str().unwrap()).unwrap();
    // SAFETY: `path` is NUL-terminated and outlives the call.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let error = rig.sealer.seal(&root).unwrap_err();
    assert!(error.0.contains("file type"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_file_with_an_extended_attribute_is_refused() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    let path = std::ffi::CString::new(root.join("a.txt").to_str().unwrap()).unwrap();
    // SAFETY: `path` and the name are NUL-terminated; the value is one byte we own.
    let set = unsafe {
        libc::setxattr(
            path.as_ptr(),
            c"user.note".as_ptr(),
            b"x".as_ptr().cast(),
            1,
            0,
        )
    };
    assert_eq!(set, 0);
    let error = rig.sealer.seal(&root).unwrap_err();
    assert!(error.0.contains("extended attribute"), "{error}");
}

#[test]
fn an_absolute_symlink_is_refused() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    std::os::unix::fs::symlink("/etc/passwd", root.join("sub/abs")).unwrap();
    let error = rig.sealer.seal(&root).unwrap_err();
    assert!(error.0.contains("symlink"), "{error}");
    assert!(error.0.contains("abs"), "{error}");
}

#[test]
fn a_symlink_pointing_outside_the_tree_is_never_followed_by_sealing_or_unsealing() {
    let rig = Rig::new();
    let root = rig.tree();
    let outside = rig.dir.path().join("outside.txt");
    std::fs::write(&outside, b"not part of the tree").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o664)).unwrap();
    std::os::unix::fs::symlink("../../outside.txt", root.join("sub/out")).unwrap();
    std::os::unix::fs::symlink("../..", root.join("sub/up")).unwrap();
    rig.sealer.seal(&root).unwrap();
    rig.sealer.unseal_for_delete(&root).unwrap();
    assert!(!rig.flags.touched(&outside));
    assert_eq!(mode(&outside), 0o664);
    assert_eq!(rig.flags.of(rig.dir.path()), 0);
    assert!(!rig.flags.touched(rig.dir.path()));
}

#[test]
fn a_tree_nested_to_the_depth_limit_is_sealed_and_one_level_deeper_is_refused() {
    let rig = Rig::new();
    let ok = rig.dir.path().join("ok");
    nest(&ok, js_bundle::MAX_DEPTH);
    let _clean = rig.guard(&ok);
    rig.sealer.seal(&ok).unwrap();
    let deep = rig.dir.path().join("deep");
    nest(&deep, js_bundle::MAX_DEPTH + 1);
    let _clean = rig.guard(&deep);
    let error = rig.sealer.seal(&deep).unwrap_err();
    assert!(error.0.contains("deeper"), "{error}");
}

#[test]
fn a_stage_that_is_not_there_is_an_error_naming_it() {
    let rig = Rig::new();
    let missing = rig.dir.path().join("nowhere");
    let error = rig.sealer.seal(&missing).unwrap_err();
    assert!(error.0.contains("nowhere"), "{error}");
}

#[test]
fn a_flag_call_that_fails_fails_the_seal() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    rig.flags.fail.store(1, Ordering::SeqCst);
    let error = rig.sealer.seal(&root).unwrap_err();
    assert!(error.0.contains("fchflags refused"), "{error}");
}

#[test]
fn an_unsealed_tree_has_no_flags_left_and_can_be_deleted() {
    let rig = Rig::new();
    let root = rig.tree();
    rig.sealer.seal(&root).unwrap();
    rig.sealer.unseal_for_delete(&root).unwrap();
    assert_eq!(rig.flags.flagged(), 0);
    assert_eq!(mode(&root.join("sub")) & 0o700, 0o700);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn unsealing_a_tree_that_was_never_sealed_changes_nothing_that_matters() {
    let rig = Rig::new();
    let root = rig.tree();
    rig.sealer.unseal_for_delete(&root).unwrap();
    rig.sealer.unseal_for_delete(&root).unwrap();
    assert_eq!(rig.flags.flagged(), 0);
    assert!(!rig.flags.touched(&root.join("link")));
}

#[test]
fn unsealing_a_tree_that_is_not_there_is_accepted() {
    let rig = Rig::new();
    rig.sealer
        .unseal_for_delete(&rig.dir.path().join("nowhere"))
        .unwrap();
}

#[test]
fn unsealing_a_path_that_runs_through_a_file_is_an_error() {
    let rig = Rig::new();
    let root = rig.tree();
    let error = rig
        .sealer
        .unseal_for_delete(&root.join("a.txt/inside"))
        .unwrap_err();
    assert!(error.0.contains("inside"), "{error}");
}

#[test]
fn unsealing_a_single_file_clears_its_flags() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    rig.sealer.seal(&root).unwrap();
    rig.sealer.unseal_for_delete(&root.join("a.txt")).unwrap();
    assert_eq!(rig.flags.of(&root.join("a.txt")), 0);
    assert_eq!(rig.flags.of(&root.join("run.sh")), IMMUTABLE);
}

#[test]
fn a_flag_that_cannot_be_cleared_is_reported_and_the_rest_are_cleared() {
    let rig = Rig::new();
    let root = rig.tree();
    let _clean = rig.guard(&root);
    rig.sealer.seal(&root).unwrap();
    rig.flags.fail.store(1, Ordering::SeqCst);
    let error = rig.sealer.unseal_for_delete(&root).unwrap_err();
    assert!(error.0.contains("fchflags refused"), "{error}");
    assert_eq!(rig.flags.flagged(), 1);
}

#[test]
fn unsealing_a_tree_deeper_than_the_limit_reports_it_after_clearing_what_it_reached() {
    let rig = Rig::new();
    let deep = rig.dir.path().join("deep");
    nest(&deep, js_bundle::MAX_DEPTH + 1);
    let error = rig.sealer.unseal_for_delete(&deep).unwrap_err();
    assert!(error.0.contains("deeper"), "{error}");
}

// Writable clones.

const TREE_PATHS: [&str; 7] = [
    "",
    "a.txt",
    "run.sh",
    "sub",
    "sub/b.txt",
    "sub/deeper",
    "sub/deeper/c.txt",
];

impl Rig {
    /// The tree of [`Rig::tree`], sealed, and a clone of it that a user can write.
    fn cloned(&self) -> (PathBuf, PathBuf) {
        let root = self.tree();
        self.sealer.seal(&root).unwrap();
        let dst = self.dir.path().join("writable");
        (root, dst)
    }
}

#[test]
fn a_clone_of_a_sealed_tree_has_no_flag_left_and_modes_that_let_its_owner_write() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    rig.sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap();
    for path in TREE_PATHS {
        assert_eq!(rig.flags.of(&dst.join(path)), 0, "flags of {path:?}");
    }
    for path in ["", "sub", "sub/deeper"] {
        assert_eq!(mode(&dst.join(path)), 0o700, "mode of {path:?}");
    }
    assert_eq!(mode(&dst.join("run.sh")), 0o700);
    for path in ["a.txt", "sub/b.txt", "sub/deeper/c.txt"] {
        assert_eq!(mode(&dst.join(path)), 0o600, "mode of {path:?}");
    }
    for path in TREE_PATHS {
        let meta = std::fs::metadata(dst.join(path)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), Owner::caller().ids(), "{path:?}");
    }
}

#[test]
fn a_clone_can_be_written_extended_and_deleted() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    rig.sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap();
    std::fs::write(dst.join("a.txt"), b"changed").unwrap();
    std::fs::write(dst.join("sub/deeper/new.txt"), b"new").unwrap();
    std::fs::remove_file(dst.join("sub/b.txt")).unwrap();
    std::fs::remove_dir_all(&dst).unwrap();
}

#[test]
fn cloning_leaves_the_sealed_original_sealed_and_unchanged() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    rig.sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap();
    std::fs::write(dst.join("a.txt"), b"changed").unwrap();
    for path in TREE_PATHS {
        assert_eq!(rig.flags.of(&root.join(path)), IMMUTABLE, "{path:?}");
    }
    assert_eq!(mode(&root), 0o555);
    assert_eq!(mode(&root.join("sub")), 0o555);
    assert_eq!(mode(&root.join("a.txt")), 0o444);
    assert_eq!(mode(&root.join("run.sh")), 0o555);
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"alpha");
}

#[test]
fn the_root_of_a_clone_is_freed_before_anything_in_it_and_each_directory_before_its_entries() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    rig.flags.forget_order();
    rig.sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap();
    let at = |path: &str| rig.flags.position(&dst.join(path));
    assert_eq!(at(""), 0);
    assert!(at("sub") < at("sub/b.txt"));
    assert!(at("sub") < at("sub/deeper"));
    assert!(at("sub/deeper") < at("sub/deeper/c.txt"));
}

#[test]
fn a_clone_keeps_links_as_links_and_never_follows_them() {
    let rig = Rig::new();
    let root = rig.tree();
    let outside = rig.dir.path().join("outside.txt");
    std::fs::write(&outside, b"not part of the tree").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o664)).unwrap();
    std::os::unix::fs::symlink("../../outside.txt", root.join("sub/out")).unwrap();
    std::os::unix::fs::symlink("../..", root.join("sub/up")).unwrap();
    rig.sealer.seal(&root).unwrap();
    let _clean = rig.guard(&root);
    let dst = rig.dir.path().join("writable");
    let _clean_dst = rig.guard(&dst);
    rig.sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap();
    assert_eq!(
        std::fs::read_link(dst.join("link")).unwrap(),
        Path::new("a.txt")
    );
    assert_eq!(
        std::fs::read_link(dst.join("sub/out")).unwrap(),
        Path::new("../../outside.txt")
    );
    assert!(!rig.flags.touched(&outside));
    assert_eq!(mode(&outside), 0o664);
    assert!(!rig.flags.touched(rig.dir.path()));
}

#[test]
fn a_set_user_id_bit_on_a_cloned_file_is_cleared() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o4755)).unwrap();
    rig.sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap();
    assert_eq!(mode(&dst.join("run.sh")), 0o700);
}

#[test]
fn a_file_with_a_second_hard_link_in_the_clone_is_refused() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    // A sealed directory takes no new entries: open it for the plant.
    std::fs::set_permissions(root.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::hard_link(root.join("a.txt"), root.join("sub/again")).unwrap();
    let error = rig
        .sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("hard link"), "{error}");
    assert!(!dst.exists(), "the part made copy is removed");
}

#[test]
fn a_pipe_in_the_clone_is_refused() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::ffi::CString::new(root.join("pipe").to_str().unwrap()).unwrap();
    // SAFETY: `path` is NUL-terminated and outlives the call.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let error = rig
        .sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("file type"), "{error}");
    assert!(!dst.exists(), "the part made copy is removed");
}

#[test]
fn a_tree_nested_to_the_depth_limit_is_cloned_and_one_level_deeper_is_refused() {
    let rig = Rig::new();
    let ok = rig.dir.path().join("ok");
    nest(&ok, js_bundle::MAX_DEPTH);
    rig.sealer.seal(&ok).unwrap();
    let _clean = rig.guard(&ok);
    let ok_dst = rig.dir.path().join("ok-writable");
    let _clean_ok = rig.guard(&ok_dst);
    rig.sealer
        .clone_writable(&ok, &ok_dst, Owner::caller())
        .unwrap();
    let deep = rig.dir.path().join("deep");
    nest(&deep, js_bundle::MAX_DEPTH + 1);
    let _clean_deep = rig.guard(&deep);
    let deep_dst = rig.dir.path().join("deep-writable");
    let _clean_deep_dst = rig.guard(&deep_dst);
    let error = rig
        .sealer
        .clone_writable(&deep, &deep_dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("deeper"), "{error}");
    assert!(!deep_dst.exists());
}

#[test]
fn a_flag_call_that_fails_fails_the_clone_and_the_error_names_both_paths() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    rig.flags.fail.store(1, Ordering::SeqCst);
    let error = rig
        .sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("fchflags refused"), "{error}");
    assert!(error.0.contains("tree"), "{error}");
    assert!(error.0.contains("writable"), "{error}");
    assert!(!dst.exists());
}

#[test]
fn a_destination_that_exists_is_refused_and_left_alone() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(dst.join("keep"), b"mine").unwrap();
    assert!(
        rig.sealer
            .clone_writable(&root, &dst, Owner::caller())
            .is_err()
    );
    assert_eq!(std::fs::read(dst.join("keep")).unwrap(), b"mine");
    assert!(!dst.join("a.txt").exists());
}

#[test]
fn a_source_that_is_not_there_is_an_error_naming_it() {
    let rig = Rig::new();
    let missing = rig.dir.path().join("nowhere");
    let dst = rig.dir.path().join("writable");
    let error = rig
        .sealer
        .clone_writable(&missing, &dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("nowhere"), "{error}");
    assert!(!dst.exists());
}

#[test]
fn a_source_that_is_a_link_or_a_pipe_is_refused() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let error = rig
        .sealer
        .clone_writable(&root.join("link"), &dst, Owner::caller());
    assert!(error.is_err());
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::ffi::CString::new(root.join("pipe").to_str().unwrap()).unwrap();
    // SAFETY: `path` is NUL-terminated and outlives the call.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let error = rig
        .sealer
        .clone_writable(&root.join("pipe"), &dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("neither"), "{error}");
    assert!(!dst.exists());
}

#[test]
fn a_clone_of_one_sealed_file_is_writable_and_keeps_its_exec_bit() {
    let rig = Rig::new();
    let (root, _) = rig.cloned();
    let _clean = rig.guard(&root);
    for (name, expected) in [("a.txt", 0o600), ("run.sh", 0o700)] {
        let dst = rig.dir.path().join(format!("copy-{name}"));
        rig.sealer
            .clone_writable(&root.join(name), &dst, Owner::caller())
            .unwrap();
        assert_eq!(rig.flags.of(&dst), 0, "{name}");
        assert_eq!(mode(&dst), expected, "{name}");
        assert_eq!(
            std::fs::read(&dst).unwrap(),
            std::fs::read(root.join(name)).unwrap()
        );
        std::fs::write(&dst, b"changed").unwrap();
        assert_eq!(rig.flags.of(&root.join(name)), IMMUTABLE, "{name}");
    }
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"alpha");
}

#[test]
fn a_clone_of_a_file_onto_a_name_that_exists_is_refused_and_leaves_it_alone() {
    let rig = Rig::new();
    let (root, _) = rig.cloned();
    let _clean = rig.guard(&root);
    let dst = rig.dir.path().join("taken");
    std::fs::write(&dst, b"mine").unwrap();
    assert!(
        rig.sealer
            .clone_writable(&root.join("a.txt"), &dst, Owner::caller())
            .is_err()
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"mine");
}

#[test]
fn a_clone_of_a_file_needs_a_destination_with_a_name() {
    let rig = Rig::new();
    let (root, _) = rig.cloned();
    let _clean = rig.guard(&root);
    let error = rig
        .sealer
        .clone_writable(&root.join("a.txt"), Path::new("/"), Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("no parent"), "{error}");
}

#[test]
fn an_owner_is_made_from_ids_but_not_from_the_value_chown_reads_as_unchanged() {
    assert_eq!(Owner::new(501, 20).map(Owner::ids), Some((501, 20)));
    assert_eq!(Owner::new(u32::MAX, 20), None);
    assert_eq!(Owner::new(501, u32::MAX), None);
}

#[test]
fn a_clone_whose_removal_fails_too_says_so_and_leaves_the_copy_to_the_caller() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    // The clone's first flag call fails, and so does the one that would open its root up
    // for removal: the root stays read-only.
    rig.flags.fail.store(2, Ordering::SeqCst);
    let error = rig
        .sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("fchflags refused"), "{error}");
    assert!(error.0.contains("removing the copy failed"), "{error}");
    assert!(dst.exists());
}

#[test]
fn a_failed_clone_of_one_file_leaves_no_file() {
    let rig = Rig::new();
    let (root, _) = rig.cloned();
    let _clean = rig.guard(&root);
    let dst = rig.dir.path().join("copy.txt");
    rig.flags.fail.store(1, Ordering::SeqCst);
    let error = rig
        .sealer
        .clone_writable(&root.join("a.txt"), &dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("fchflags refused"), "{error}");
    assert!(!dst.exists());
}

#[test]
fn a_directory_clone_needs_a_destination_with_a_name_too() {
    let rig = Rig::new();
    let (root, _) = rig.cloned();
    let _clean = rig.guard(&root);
    let error = rig
        .sealer
        .clone_writable(&root, Path::new("/"), Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("no parent"), "{error}");
}

#[test]
fn a_file_or_directory_with_an_acl_is_refused_by_sealing() {
    for planted in ["a.txt", "sub"] {
        let rig = Rig::new();
        let root = rig.tree();
        let _clean = rig.guard(&root);
        rig.flags.plant_acl(&root.join(planted));
        let error = rig.sealer.seal(&root).unwrap_err();
        assert!(error.0.contains("ACL"), "{planted}: {error}");
    }
}

#[test]
fn a_clone_has_no_acl_on_any_entry_and_the_original_keeps_its_own() {
    let rig = Rig::new();
    let (root, dst) = rig.cloned();
    let _clean = rig.guard(&root);
    let _clean_dst = rig.guard(&dst);
    // An ACL the sealed tree gained (or was made with) after sealing.
    for path in ["a.txt", "sub", "sub/deeper/c.txt", ""] {
        rig.flags.plant_acl(&root.join(path));
    }
    rig.sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap();
    for path in TREE_PATHS {
        assert!(!rig.flags.acl_of(&dst.join(path)), "clone {path:?}");
    }
    for path in ["a.txt", "sub", "sub/deeper/c.txt", ""] {
        assert!(rig.flags.acl_of(&root.join(path)), "original {path:?}");
    }
}

#[test]
fn unsealing_leaves_the_flags_of_a_file_with_another_hard_link_and_says_so() {
    let rig = Rig::new();
    let (root, _) = rig.cloned();
    let _clean = rig.guard(&root);
    std::fs::set_permissions(root.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::hard_link(root.join("a.txt"), root.join("sub/again")).unwrap();
    let error = rig.sealer.unseal_for_delete(&root).unwrap_err();
    assert!(error.0.contains("hard link"), "{error}");
    assert_eq!(rig.flags.of(&root.join("a.txt")), IMMUTABLE);
    assert_eq!(rig.flags.of(&root.join("run.sh")), 0);
    assert_eq!(rig.flags.of(&root.join("sub/b.txt")), 0);
}
