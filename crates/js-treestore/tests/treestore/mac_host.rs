//! The macOS sealer against the real `fchflags`, as the user running the tests (no root):
//! the owner can set `uchg` on what it owns, and `uchg` stops even the owner. The cases
//! that need root and another user are in `tests/privileged.rs`.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use js_testkit::dir::TestDir;
use js_treestore::Sealer as _;
use js_treestore::mac::{
    Clonefile, Fchflags, IMMUTABLE, MacSealer, Owner, UserFlags as _, start_checks,
};

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
    let sealer = MacSealer::new(Owner::caller(), Fchflags, Clonefile);
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
    let sealer = MacSealer::new(Owner::caller(), Fchflags, Clonefile);
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

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

#[test]
fn a_clone_of_a_sealed_tree_is_writable_and_can_be_deleted_and_the_original_stays_sealed() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let root = tree(&dir);
    let sealer = MacSealer::new(Owner::caller(), Fchflags, Clonefile);
    sealer.seal(&root).unwrap();
    let dst = dir.path().join("writable");
    sealer.clone_writable(&root, &dst, Owner::caller()).unwrap();
    for path in ["", "a.txt", "sub", "sub/b.txt"] {
        assert_eq!(flags_of(&dst.join(path)) & IMMUTABLE, 0, "{path}");
    }
    assert_eq!((mode(&dst), mode(&dst.join("sub"))), (0o700, 0o700));
    assert_eq!(mode(&dst.join("a.txt")), 0o600);
    assert_eq!(
        std::fs::read_link(dst.join("link")).unwrap(),
        Path::new("a.txt")
    );
    std::fs::write(dst.join("a.txt"), b"changed").unwrap();
    std::fs::write(dst.join("sub/new.txt"), b"new").unwrap();
    std::fs::remove_file(dst.join("sub/b.txt")).unwrap();
    // The original is as it was.
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"alpha");
    assert_eq!(std::fs::read(root.join("sub/b.txt")).unwrap(), b"bravo");
    for path in ["", "a.txt", "sub", "sub/b.txt"] {
        assert_eq!(flags_of(&root.join(path)) & IMMUTABLE, IMMUTABLE, "{path}");
    }
    std::fs::remove_dir_all(&dst).unwrap();
    sealer.unseal_for_delete(&root).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_clone_of_a_sealed_file_is_writable_and_the_original_stays_sealed() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let root = tree(&dir);
    let sealer = MacSealer::new(Owner::caller(), Fchflags, Clonefile);
    sealer.seal(&root).unwrap();
    let dst = dir.path().join("copy.txt");
    sealer
        .clone_writable(&root.join("a.txt"), &dst, Owner::caller())
        .unwrap();
    assert_eq!(flags_of(&dst) & IMMUTABLE, 0);
    assert_eq!(mode(&dst), 0o600);
    std::fs::write(&dst, b"changed").unwrap();
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"alpha");
    assert_eq!(flags_of(&root.join("a.txt")) & IMMUTABLE, IMMUTABLE);
    std::fs::remove_file(&dst).unwrap();
    sealer.unseal_for_delete(&root).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_link_is_not_followed_when_it_is_the_source_of_a_clone() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let root = tree(&dir);
    let sealer = MacSealer::new(Owner::caller(), Fchflags, Clonefile);
    let dst = dir.path().join("copy");
    assert!(
        sealer
            .clone_writable(&root.join("link"), &dst, Owner::caller())
            .is_err()
    );
    assert!(!dst.exists());
}

fn set_flags(path: &Path, flags: u32) {
    use std::os::fd::AsFd as _;
    let file = std::fs::File::open(path).unwrap();
    Fchflags.set(file.as_fd(), flags).unwrap();
}

fn has_acl(path: &Path) -> bool {
    use std::os::fd::AsFd as _;
    let file = std::fs::File::open(path).unwrap();
    Fchflags.has_acl(file.as_fd()).unwrap()
}

/// Gives the file an access control list, as its owner can.
fn add_acl(path: &Path) {
    let done = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow read"])
        .arg(path)
        .status()
        .unwrap();
    assert!(done.success());
}

#[test]
fn a_file_with_an_acl_is_refused_by_sealing() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let root = tree(&dir);
    add_acl(&root.join("sub/b.txt"));
    assert!(has_acl(&root.join("sub/b.txt")));
    let sealer = MacSealer::new(Owner::caller(), Fchflags, Clonefile);
    let error = sealer.seal(&root).unwrap_err();
    assert!(error.0.contains("ACL"), "{error}");
    sealer.unseal_for_delete(&root).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_clone_has_no_acl_though_the_sealed_original_has_one() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let root = tree(&dir);
    let sealer = MacSealer::new(Owner::caller(), Fchflags, Clonefile);
    sealer.seal(&root).unwrap();
    // The ACL arrives after sealing: open the file's flag, add it, close it again.
    let file = root.join("a.txt");
    set_flags(&file, 0);
    add_acl(&file);
    set_flags(&file, IMMUTABLE);
    let dst = dir.path().join("writable");
    sealer.clone_writable(&root, &dst, Owner::caller()).unwrap();
    assert!(has_acl(&file));
    assert!(!has_acl(&dst.join("a.txt")));
    std::fs::write(dst.join("a.txt"), b"changed").unwrap();
    std::fs::remove_dir_all(&dst).unwrap();
    sealer.unseal_for_delete(&root).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_clone_that_fails_midway_leaves_nothing_behind() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "mac-host");
    let root = tree(&dir);
    let sealer = MacSealer::new(Owner::caller(), Fchflags, Clonefile);
    sealer.seal(&root).unwrap();
    // A branch nested past the depth limit, which the walk refuses wherever it meets it;
    // the rest of the tree is still sealed, so the clean-up has flags to clear.
    set_flags(&root, 0);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut deep = root.join("deep");
    for _ in 0..=js_bundle::MAX_DEPTH {
        deep.push("d");
    }
    std::fs::create_dir_all(&deep).unwrap();
    let dst = dir.path().join("writable");
    let error = sealer
        .clone_writable(&root, &dst, Owner::caller())
        .unwrap_err();
    assert!(error.0.contains("deeper"), "{error}");
    assert!(!dst.exists());
    // Too deep for the unsealing too: it clears what it reaches and says so.
    let _ = sealer.unseal_for_delete(&root);
    std::fs::remove_dir_all(&root).unwrap();
}
