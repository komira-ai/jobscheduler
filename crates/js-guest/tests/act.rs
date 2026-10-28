//! `js-guest-act`, the program the other tests give the agent to run: every other test
//! trusts its steps, so these show that each does what its name says, that a malformed
//! command line does nothing, and that a failing step is told apart from one that exits.

use std::process::{Command, Output};

const ACT: &str = env!("CARGO_BIN_EXE_js-guest-act");

fn act(args: &[&str]) -> Output {
    Command::new(ACT).args(args).output().expect("ran")
}

/// Catches an `exit` that ends the run with another status, or steps after it that
/// still run: the agent's tests read the status from here.
#[test]
fn exit_ends_the_run_with_its_status_before_later_steps() {
    let out = act(&["echo", "a", ";", "exit", "3", ";", "echo", "b"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(out.stdout, b"a\n");
}

/// Catches a run that does not end with 0 when every step ran, and an `echo` that does
/// not join its words with spaces.
#[test]
fn echo_joins_its_words_and_a_finished_run_exits_zero() {
    let out = act(&["echo", "x", "y"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"x y\n");
}

/// Catches a `sleep` that rejects a fraction (the tests sleep for tenths of a second) or
/// that does not go on to the next step.
#[test]
fn sleep_takes_fractions_and_goes_on() {
    let out = act(&["sleep", "0.01", ";", "echo", "after"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"after\n");
}

/// Catches `pwd` printing anything but the working directory the agent set.
#[test]
fn pwd_prints_the_working_directory() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .canonicalize()
        .expect("target tmp dir");
    let out = Command::new(ACT)
        .arg("pwd")
        .current_dir(&dir)
        .output()
        .expect("ran");
    assert_eq!(out.stdout, format!("{}\n", dir.display()).into_bytes());
}

/// Catches an `append` that truncates (the test of requested outputs expects the file
/// size to be what one call wrote plus a newline) or does not add the newline, and one
/// that fails silently when the file cannot be made.
#[test]
fn append_adds_a_line_and_fails_when_it_cannot() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("js-guest-act-append-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("f");
    let file_arg = file.to_str().expect("utf-8");
    let out = act(&[
        "append", file_arg, "one", "two", ";", "append", file_arg, "three",
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(std::fs::read(&file).expect("read"), b"one two\nthree\n");
    let missing = dir.join("no-such-dir").join("f");
    let out = act(&["append", missing.to_str().expect("utf-8"), "x"]);
    assert_eq!(out.status.code(), Some(1));
    std::fs::remove_dir_all(&dir).expect("clean up");
}

/// Catches a `kill-self` that does not end the process by the signal (the tests expect
/// `Signaled(15)`), and one that exits 0 when the signal was ignored: the run must fail
/// then, or a test could pass on a process nothing killed.
#[test]
fn kill_self_dies_by_the_signal_and_fails_when_it_is_ignored() {
    use std::os::unix::process::ExitStatusExt as _;
    let out = act(&["kill-self", "15"]);
    assert_eq!(out.status.signal(), Some(15));
    // Ignored by default, so the process outlives the signal.
    let ignored = libc::SIGCHLD.to_string();
    let out = act(&["kill-self", &ignored]);
    assert_eq!(out.status.code(), Some(1));
}

/// Catches a command line checked as it runs: a bad step after a good one must exit 2
/// before the good one prints or sleeps, and every kind of bad step must be refused.
#[test]
fn a_malformed_step_exits_2_having_done_nothing() {
    let bad: [&[&str]; 8] = [
        &["echo", "a", ";", "frobnicate"],
        &["echo", "a", ";", "exit", "300"],
        &["echo", "a", ";", "exit", "x"],
        &["echo", "a", ";", "sleep", "-1"],
        &["echo", "a", ";", "sleep", "soon"],
        &["echo", "a", ";", "kill-self", "0"],
        &["echo", "a", ";", "kill-self", "x"],
        &[],
    ];
    for args in bad {
        let out = act(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}
