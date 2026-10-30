//! The sealer against what only root on a Mac can show: a tree sealed as root refuses
//! another user, and unsealing lets it be deleted; and what a sealed, root-owned tree does
//! to `clonefile`, `link` and ownership, recorded as `PROBE` lines.
//!
//! Every test is `#[ignore]`d: they run only on a provisioned Mac, as root, through the
//! `js-treestore-privileged-tests` workflow's binary. They read three variables (a
//! flag-free test binary has no other way to take them):
//!
//! - `JS_TREESTORE_PRIV_ROOT`: an empty directory on the APFS volume to work in;
//! - `JS_TREESTORE_PRIV_UID` and `JS_TREESTORE_PRIV_GID`: an unprivileged user and group.
//!
//! Run as root: `./privileged --ignored --test-threads=1`.
#![cfg(target_os = "macos")]

use std::ffi::{CString, c_char, c_int};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use js_treestore::Sealer as _;
use js_treestore::mac::{IMMUTABLE, MacSealer, Owner, start_checks};

unsafe extern "C" {
    fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> c_int;
}

struct Work {
    dir: PathBuf,
    uid: u32,
    gid: u32,
    sealer: MacSealer,
}

impl Work {
    fn new(name: &str) -> Work {
        assert_eq!(rustix::process::geteuid().as_raw(), 0, "run as root");
        let base =
            PathBuf::from(std::env::var("JS_TREESTORE_PRIV_ROOT").expect("JS_TREESTORE_PRIV_ROOT"));
        let number = |var: &str| -> u32 { std::env::var(var).expect(var).parse().expect(var) };
        let work = Work {
            dir: base.join(format!("{name}-{}", std::process::id())),
            uid: number("JS_TREESTORE_PRIV_UID"),
            gid: number("JS_TREESTORE_PRIV_GID"),
            sealer: MacSealer::new(Owner::root(), js_treestore::mac::Fchflags),
        };
        work.clear();
        std::fs::create_dir(&work.dir).unwrap();
        std::fs::set_permissions(&work.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        work
    }

    /// Removes the work directory, clearing `uchg` first.
    fn clear(&self) {
        if self.dir.exists() {
            self.sealer.unseal_for_delete(&self.dir).unwrap();
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    /// `root/{a.txt, sub/b.txt}`, readable by everyone.
    fn tree(&self) -> PathBuf {
        let root = self.dir.join("tree");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"alpha").unwrap();
        std::fs::write(root.join("sub/b.txt"), b"bravo").unwrap();
        root
    }

    /// Runs `program` as the unprivileged user; true if it exited zero.
    fn as_user(&self, program: &str, args: &[&Path]) -> bool {
        Command::new(program)
            .args(args)
            .uid(self.uid)
            .gid(self.gid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        self.clear();
    }
}

fn flags_of(path: &Path) -> u32 {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `stat` is plain data; the path is NUL-terminated and outlives the call.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above; `lstat` does not follow a link.
    assert_eq!(unsafe { libc::lstat(path.as_ptr(), &raw mut stat) }, 0);
    stat.st_flags
}

#[test]
#[ignore = "needs root on a provisioned Mac (js-treestore-privileged-tests)"]
fn a_tree_sealed_as_root_refuses_a_write_by_another_user_and_by_root() {
    let work = Work::new("sealed-write");
    let root = work.tree();
    work.sealer.seal(&root).unwrap();
    let file = root.join("a.txt");
    // Owned by root, no write bit, immutable.
    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!((meta.uid(), meta.mode() & 0o7777), (0, 0o444));
    assert_eq!(flags_of(&file) & IMMUTABLE, IMMUTABLE);
    // Another user can neither append, nor create, nor remove.
    assert!(!work.as_user("/usr/bin/tee", &[Path::new("-a"), &file]));
    assert!(!work.as_user("/usr/bin/touch", &[&root.join("new")]));
    assert!(!work.as_user("/bin/rm", &[&file]));
    // The flag stops root too, until it is cleared.
    assert!(std::fs::write(&file, b"changed").is_err());
    assert!(std::fs::remove_file(root.join("sub/b.txt")).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"alpha");
}

#[test]
#[ignore = "needs root on a provisioned Mac (js-treestore-privileged-tests)"]
fn unsealing_a_tree_sealed_as_root_lets_it_be_deleted() {
    let work = Work::new("unseal-delete");
    let root = work.tree();
    work.sealer.seal(&root).unwrap();
    assert!(std::fs::remove_dir_all(&root).is_err());
    work.sealer.unseal_for_delete(&root).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
#[ignore = "needs root on a provisioned Mac (js-treestore-privileged-tests)"]
fn the_work_volume_passes_the_start_checks() {
    let work = Work::new("start-checks");
    let outcome = start_checks(&work.dir, &work.dir);
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[test]
#[ignore = "needs root on a provisioned Mac (js-treestore-privileged-tests)"]
fn ownership_given_to_another_user_is_honoured_on_the_work_volume() {
    let work = Work::new("ownership");
    let file = work.dir.join("mine");
    std::fs::write(&file, b"x").unwrap();
    std::os::unix::fs::chown(&file, Some(work.uid), Some(work.gid)).unwrap();
    assert_eq!(std::fs::metadata(&file).unwrap().uid(), work.uid);
    // And the user can write what it owns, which it could not if ownership were ignored
    // in favour of root's mode.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(work.as_user("/usr/bin/tee", &[Path::new("-a"), &file]));
}

#[test]
#[ignore = "needs root on a provisioned Mac (js-treestore-privileged-tests)"]
fn probe_what_a_clone_of_a_sealed_file_and_directory_carries() {
    let work = Work::new("clone");
    let root = work.tree();
    work.sealer.seal(&root).unwrap();
    for (name, from) in [("file", root.join("a.txt")), ("dir", root.join("sub"))] {
        let to = work.dir.join(format!("clone-{name}"));
        let (src, dst) = (
            CString::new(from.as_os_str().as_bytes()).unwrap(),
            CString::new(to.as_os_str().as_bytes()).unwrap(),
        );
        // SAFETY: both paths are NUL-terminated and outlive the call.
        let done = unsafe { clonefile(src.as_ptr(), dst.as_ptr(), 0) };
        let error = std::io::Error::last_os_error();
        if done == 0 {
            let meta = std::fs::symlink_metadata(&to).unwrap();
            eprintln!(
                "PROBE clone of sealed {name}: ok, owner {}:{}, mode {:o}, flags {:#x}",
                meta.uid(),
                meta.gid(),
                meta.mode() & 0o7777,
                flags_of(&to)
            );
        } else {
            eprintln!("PROBE clone of sealed {name}: refused, {error}");
        }
    }
}

#[test]
#[ignore = "needs root on a provisioned Mac (js-treestore-privileged-tests)"]
fn probe_whether_another_user_can_link_a_sealed_file() {
    let work = Work::new("link");
    let root = work.tree();
    work.sealer.seal(&root).unwrap();
    let open = work.dir.join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    let linked = work.as_user("/bin/ln", &[&root.join("a.txt"), &open.join("kept")]);
    eprintln!(
        "PROBE link of a sealed file by uid {}: {}",
        work.uid,
        if linked { "allowed" } else { "refused" }
    );
}
