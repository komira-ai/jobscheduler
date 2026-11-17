//! `cargo xtask release turn`, run as the built binary with a stand-in `gh` (a small shell
//! script in the scratch directory that answers the calls the verb makes): the exit statuses
//! (0 for a go and for a supersession, 1 for an answer that cannot be used, 2 for flags that do
//! not fit), what reaches stdout and stderr, and the lines appended to the output file. The
//! real `gh` is never started and nothing touches the network.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::sync::Mutex;

use js_testkit::dir::TestDir;

/// A script being written is a file open for writing: one test at a time writes and starts its
/// stand-in, or another test's child could hold the descriptor and fail the start ("text file
/// busy").
static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `release turn` for run 5 of `o/r` at `version`, in `dir`, with the stand-in `gh` that
/// runs `gh_body`.
fn turn(dir: &TestDir, gh_body: &str, version: &str) -> Output {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let gh = dir.join("gh");
    std::fs::write(&gh, format!("#!/bin/sh\n{gh_body}")).expect("script");
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).expect("mode");
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "release",
            "turn",
            "--repo",
            "o/r",
            "--workflow",
            "release.yml",
            "--run-number",
            "5",
            "--poll-seconds",
            "0",
            "--version",
            version,
        ])
        .arg("--output")
        .arg(dir.join("out"))
        .arg("--gh")
        .arg(&gh)
        .output()
        .expect("run xtask")
}

/// Answers the three calls: no runs in progress but this one, and `releases`.
fn gh_body(releases: &str) -> String {
    format!(
        "case \"$2\" in\n*workflows*) echo '{{\"total_count\":1,\"workflow_runs\":[{{\"id\":50,\"run_number\":5}}]}}';;\n*releases*) echo '{releases}';;\n*) exit 9;;\nesac\n"
    )
}

/// Asserts that a run with nothing in its way exits 0, prints the reason and `go=true`, and
/// appends `go=true` and the reason to the output file.
#[test]
fn a_run_with_nothing_in_its_way_goes() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-turn-go");
    let out = turn(&dir, &gh_body("[]"), "1.2.3");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8"),
        "no newer release, no newer run at its turn, no older run holding it\ngo=true\n"
    );
    assert_eq!(out.stderr, b"");
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        "go=true\nreason=no newer release, no newer run at its turn, no older run holding it\n"
    );
}

/// Asserts that a run a newer published release has overtaken exits 0 (it is not a failure),
/// prints the reason and `go=false`, and appends them to the output file.
#[test]
fn a_run_a_newer_release_overtook_exits_zero_with_go_false() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-turn-superseded");
    let out = turn(
        &dir,
        &gh_body("[{\"tag_name\":\"v1.2.4\",\"draft\":false,\"prerelease\":false}]"),
        "1.2.3",
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8"),
        "release v1.2.4 is published, newer than v1.2.3\ngo=false\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        "go=false\nreason=release v1.2.4 is published, newer than v1.2.3\n"
    );
}

/// Asserts that a `gh` that fails is an error, status 1, with `gh`'s reason on stderr, nothing
/// on stdout and nothing in the output file (the run neither publishes nor skips).
#[test]
fn a_gh_that_fails_exits_one_and_decides_nothing() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-turn-gh-fails");
    let out = turn(&dir, "echo 'gh: HTTP 502' >&2\nexit 1\n", "1.2.3");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    let stderr = String::from_utf8(out.stderr).expect("utf-8");
    assert!(
        stderr.starts_with("xtask: ") && stderr.ends_with("gh: HTTP 502\n"),
        "{stderr}"
    );
    assert!(!dir.join("out").exists());
}

/// Asserts that a `--version` that is not `X.Y.Z` exits 2 with the usage reason on stderr and
/// nothing on stdout or in the output file.
#[test]
fn a_version_that_is_not_x_y_z_exits_two() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-turn-usage");
    let out = turn(&dir, "exit 9\n", "1.2");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        String::from_utf8(out.stderr).expect("utf-8"),
        "xtask: --version \"1.2\" is not <major>.<minor>.<patch>, a release tag without its v\n"
    );
    assert!(!dir.join("out").exists());
}
