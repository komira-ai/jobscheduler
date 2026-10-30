//! The macOS sealer against the real `fchflags`, as the user running the tests (no root):
//! the owner can set `uchg` on what it owns, and `uchg` stops even the owner. The cases
//! that need root and another user are in `tests/privileged.rs`.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use js_testkit::dir::TestDir;
use js_treestore::Sealer as _;
use js_treestore::mac::{Fchflags, IMMUTABLE, MacSealer, Owner, start_checks};

fn flags_of(path: &Path) -> u32 {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `stat` is plain data; the path is NUL-terminated and outlives the call.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above; `lstat` does not follow a link.
    assert_eq!(unsafe { libc::lstat(path.as_ptr(), &raw mut stat) }, 0);
    stat.st_flags
}

fn tree(dir: &TestDir) -> std::path::PathBuf {
    let root = dir.path().join("tree");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), b"alpha").unwrap();
    std::fs::write(root.join("sub/b.txt"), b"bravo").unwrap();
    std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    root
}

#[test]
fn a_sealed_tree_refuses_writes_even_from_its_owner_and_an_unsealed_one_can_be_deleted() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let root = tree(&dir);
    let sealer = MacSealer::new(Owner::caller(), Fchflags);
    sealer.seal(&root).unwrap();
    assert!(std::fs::write(root.join("a.txt"), b"changed").is_err());
    assert!(std::fs::write(root.join("new.txt"), b"new").is_err());
    assert!(std::fs::remove_file(root.join("sub/b.txt")).is_err());
    assert!(std::fs::rename(root.join("sub"), root.join("moved")).is_err());
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"alpha");
    sealer.unseal_for_delete(&root).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn sealing_sets_uchg_on_files_and_directories_and_unsealing_clears_it() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let root = tree(&dir);
    let sealer = MacSealer::new(Owner::caller(), Fchflags);
    sealer.seal(&root).unwrap();
    for path in ["", "a.txt", "sub", "sub/b.txt"] {
        assert_eq!(flags_of(&root.join(path)) & IMMUTABLE, IMMUTABLE, "{path}");
    }
    sealer.unseal_for_delete(&root).unwrap();
    for path in ["", "a.txt", "sub", "sub/b.txt"] {
        assert_eq!(flags_of(&root.join(path)) & IMMUTABLE, 0, "{path}");
    }
}

#[test]
fn the_temp_volume_passes_the_start_checks() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let outcome = start_checks(dir.path(), &scratch);
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[test]
fn a_path_that_is_not_there_fails_the_start_checks() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    assert!(start_checks(&dir.path().join("nowhere"), dir.path()).is_err());
}
