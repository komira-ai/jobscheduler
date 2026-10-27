//! The umask is per process, so the test that changes it runs alone in this binary.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use js_testkit::dir::TestDir;
use js_vmm::offline_put::{Env, put, remove_path};
use js_vmm::system::OfflineFile;

fn mode_of(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).expect("stat").mode() & 0o7777
}

/// Catches a file or a created directory whose mode follows the process umask instead of
/// being set exactly on the open descriptor. It also writes again over what it wrote, and
/// a file over a directory: this binary has its own copy of the library, and the coverage
/// gate wants every branch of the code it runs executed here too.
#[test]
fn modes_are_exact_whatever_the_umask() {
    let root = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "offline-put-umask");
    let env = Env {
        remove: &remove_path,
        owner: None,
    };
    let f = OfflineFile {
        path: "a/b/f".to_owned(),
        contents: b"x".to_vec(),
        mode: 0o755,
    };
    // SAFETY: umask only changes this process's mask; this binary has one test.
    unsafe { libc::umask(0o277) };
    put(root.path(), &f, &env).expect("put");
    assert_eq!(mode_of(&root.path().join("a/b/f")), 0o755);
    assert_eq!(mode_of(&root.path().join("a")), 0o755);
    assert_eq!(mode_of(&root.path().join("a/b")), 0o755);
    put(root.path(), &f, &env).expect("put again");
    assert_eq!(mode_of(&root.path().join("a/b/f")), 0o755);
    let over = OfflineFile {
        path: "a".to_owned(),
        ..f
    };
    put(root.path(), &over, &env).expect("put over a directory");
    // SAFETY: getuid and getgid cannot fail.
    let me = unsafe { (libc::getuid(), libc::getgid()) };
    let owned = Env {
        remove: &remove_path,
        owner: Some(me),
    };
    let again = OfflineFile {
        path: "o".to_owned(),
        contents: b"x".to_vec(),
        mode: 0o600,
    };
    put(root.path(), &again, &owned).expect("put with an owner");
    assert_eq!(mode_of(&root.path().join("o")), 0o600);
    assert!(root.path().join("a").is_file());
    assert_eq!(mode_of(&root.path().join("a")), 0o755);
}
