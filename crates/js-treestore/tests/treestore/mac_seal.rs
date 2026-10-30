//! The flag sealer, over a fake that records the flags it is asked to set, so that the walk
//! runs on every platform. What `uchg` does to a write is for the macOS tests.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_testkit::dir::TestDir;
use js_treestore::Sealer;
use js_treestore::mac::{FlagSealer, IMMUTABLE, Owner, UserFlags};

#[derive(Clone, Default)]
struct FakeFlags {
    flags: Arc<Mutex<BTreeMap<u64, u32>>>,
    touched: Arc<Mutex<BTreeSet<u64>>>,
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
        Ok(())
    }
}

impl FakeFlags {
    fn of(&self, path: &Path) -> u32 {
        let ino = std::fs::symlink_metadata(path).unwrap().ino();
        self.flags.lock().unwrap().get(&ino).copied().unwrap_or(0)
    }

    fn touched(&self, path: &Path) -> bool {
        let ino = std::fs::symlink_metadata(path).unwrap().ino();
        self.touched.lock().unwrap().contains(&ino)
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

struct Rig {
    dir: TestDir,
    flags: FakeFlags,
    sealer: Arc<FlagSealer<FakeFlags>>,
}

impl Rig {
    fn new() -> Rig {
        let flags = FakeFlags::default();
        Rig {
            dir: TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-seal"),
            sealer: Arc::new(FlagSealer::new(Owner::caller(), flags.clone())),
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
    sealer: Arc<FlagSealer<FakeFlags>>,
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
    assert_eq!(format!("{:?}", Owner::root()), "Owner { uid: 0, gid: 0 }");
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
