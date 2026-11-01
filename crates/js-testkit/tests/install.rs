//! [`install_copy`] and [`install_script`]: a program made for a library to start is
//! whole, has the mode asked for, and starts. (Whether it could be busy depends on a
//! fork in another thread landing while a descriptor is open, which no test can order
//! without the helper's cooperation; the helper's rule is that its writer is another,
//! finished, process, and these tests pin that the result is complete and runnable.)

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_testkit::spawn::{install_copy, install_script};

/// A fresh directory for one test, under Cargo's per-target temporary directory.
fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("old dir");
    }
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("meta").permissions().mode() & 0o7777
}

/// Catches: a script that is cut short, left unexecutable, or written with the wrong
/// mode: it runs and prints what it was given, byte for byte (quotes and `%` kept).
#[test]
fn an_installed_script_runs_with_the_mode_asked_for() {
    let path = dir("install-script").join("tool");
    install_script(&path, "#!/bin/sh\necho 'a \"b\" %s'\n", "755");
    assert_eq!(mode(&path), 0o755);
    let out = Command::new(&path).output().expect("runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a \"b\" %s\n");
}

/// Catches: a copy that is not the source, or loses the mode asked for (a set-user-id
/// bit included).
#[test]
fn an_installed_copy_has_the_source_bytes_and_the_mode_asked_for() {
    let from = dir("install-copy").join("from");
    std::fs::write(&from, b"#!/bin/true\n").expect("source");
    let to = from.with_file_name("to");
    install_copy(&from, &to, "4755");
    assert_eq!(std::fs::read(&to).expect("copy"), b"#!/bin/true\n");
    assert_eq!(mode(&to), 0o4755);
    assert!(Command::new(&to).status().expect("runs").success());
}

/// Catches: a failure of the write passed over (the test would go on with no program).
#[test]
#[should_panic(expected = "installing a program")]
fn an_install_that_fails_panics() {
    let path = dir("install-fails").join("no-such-dir/tool");
    install_script(&path, "x", "755");
}
