//! `Scratch`: one test's directory is its own, whether the other tests run as threads of
//! this process (`cargo test`) or each in a process of its own (cargo-nextest).
//!
//! The process case runs this test binary again as two children at once, each running the
//! `#[ignore]`d `child_prints_its_scratch_dir`, which makes the directory the tests of
//! `reads.rs` make and prints its path.

use std::process::{Command, Stdio};

use crate::support::Scratch;

/// What a child prints before the path.
const MARK: &str = "scratch-dir: ";

/// Not a test of its own: the child of
/// `scratch_dirs_of_one_name_in_two_processes_at_once_are_distinct`.
#[test]
#[ignore = "run only as the child of a test in this file"]
fn child_prints_its_scratch_dir() {
    let scratch = Scratch::new("reads");
    assert!(scratch.dir().is_dir(), "{}", scratch.dir().display());
    println!("{MARK}{}", scratch.dir().display());
}

/// Catches a directory named only by a per-process counter or a fixed name: two
/// processes then both get `reads-0`, and one test's drop removes the key file another
/// is about to read (the failure was `Read { path: .../reads-0/key, why: "No such file or
/// directory" }`).
#[test]
fn scratch_dirs_of_one_name_in_two_processes_at_once_are_distinct() {
    let exe = std::env::current_exe().expect("this test binary");
    let children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(&exe)
                .args([
                    "--exact",
                    "scratch::child_prints_its_scratch_dir",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start a child")
        })
        .collect();
    let dirs: Vec<String> = children
        .into_iter()
        .map(|child| {
            let out = child.wait_with_output().expect("wait for a child");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "child failed: {stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            );
            // libtest prints `test <name> ... ` before the test's own output, on one line.
            let dirs: Vec<&str> = stdout
                .lines()
                .filter_map(|l| l.split_once(MARK).map(|(_, dir)| dir))
                .collect();
            assert_eq!(dirs.len(), 1, "{stdout}");
            dirs[0].to_owned()
        })
        .collect();
    assert_ne!(dirs[0], dirs[1]);
}

/// Catches two directories of one name in one process sharing a path (the counter part).
#[test]
fn scratch_dirs_of_one_name_in_one_process_are_distinct() {
    let a = Scratch::new("reads");
    let b = Scratch::new("reads");
    assert_ne!(a.dir(), b.dir());
    assert!(a.dir().is_dir() && b.dir().is_dir());
}

/// Catches a dropped directory left behind.
#[test]
fn a_dropped_scratch_dir_is_removed() {
    let scratch = Scratch::new("reads");
    scratch.key_file("key-one");
    let dir = scratch.dir().to_path_buf();
    drop(scratch);
    assert!(!dir.exists(), "{}", dir.display());
}
