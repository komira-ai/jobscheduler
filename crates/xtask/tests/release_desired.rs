//! `cargo xtask release desired`, run as the built binary with a stand-in `gh` (a small shell
//! script in the scratch directory that answers the calls the verb makes): the exit statuses
//! (0 for a set or a deliberate no-set, 1 for a refusal, 2 for flags that do not fit), what
//! reaches stdout and stderr, and the lines appended to the output file. The real `gh` and
//! `curl` are never started and nothing touches the network.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::sync::Mutex;

use js_testkit::dir::TestDir;

/// A script being written is a file open for writing: one test at a time writes and starts its
/// stand-in, or another test's child could hold the descriptor and fail the start ("text file
/// busy").
static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `release desired` with `flags`, in `dir`, with the stand-in `gh` that runs `gh_body`.
fn desired(dir: &TestDir, gh_body: &str, flags: &[&str]) -> Output {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let gh = dir.join("gh");
    std::fs::write(&gh, format!("#!/bin/sh\n{gh_body}")).expect("script");
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).expect("mode");
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["release", "desired", "--repo", "o/r", "--dir"])
        .arg(dir.join("d"))
        .arg("--output")
        .arg(dir.join("out"))
        .arg("--gh")
        .arg(&gh)
        .args(flags)
        .output()
        .expect("run xtask")
}

/// Asserts that a run whose `publish` job was skipped exits 0, prints why nothing is set and
/// `set=false`, and appends `set=false` to the output file: most release runs publish nothing.
#[test]
fn a_run_that_published_nothing_exits_zero_with_set_false() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-desired-none");
    let out = desired(
        &dir,
        "echo '{\"total_count\":1,\"jobs\":[{\"name\":\"publish\",\"conclusion\":\"skipped\"}]}'\n",
        &["--version", "", "--after-run", "42"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8"),
        "run 42 of o/r did not publish a release (publish did not succeed): nothing is set\nset=false\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        "set=false\n"
    );
}

/// Asserts that both flags empty, and both given, exit 2 with the usage reason on stderr and
/// nothing on stdout or in the output file.
#[test]
fn both_and_neither_of_version_and_after_run_exit_two() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-desired-usage");
    for flags in [
        ["--version", "", "--after-run", ""],
        ["--version", "1.2.3", "--after-run", "9"],
    ] {
        let out = desired(&dir, "exit 9\n", &flags);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(out.stdout, b"");
        assert_eq!(
            String::from_utf8(out.stderr).expect("utf-8"),
            "xtask: give one of --version and --after-run, not both and not neither\n"
        );
        assert!(!dir.join("out").exists());
    }
}

/// Asserts that a release `gh` cannot find is a refusal, status 1, with `gh`'s own reason on
/// stderr, and that nothing is appended to the output file (the coordinator is never called).
#[test]
fn a_release_that_cannot_be_read_exits_one_and_sets_nothing() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-desired-refused");
    let out = desired(
        &dir,
        "echo 'gh: Not Found (HTTP 404)' >&2\nexit 1\n",
        &["--version", "1.2.3", "--after-run", ""],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    let stderr = String::from_utf8(out.stderr).expect("utf-8");
    assert!(
        stderr.starts_with("xtask: ") && stderr.ends_with("gh: Not Found (HTTP 404)\n"),
        "{stderr}"
    );
    assert!(!dir.join("out").exists());
}

/// Asserts that a draft release named by `--version` is a refusal even though `gh` finds it.
#[test]
fn a_draft_release_exits_one() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-desired-draft");
    let out = desired(
        &dir,
        "echo '{\"draft\":true,\"assets\":[]}'\n",
        &["--version", "1.2.3", "--after-run", ""],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).expect("utf-8"),
        "xtask: release v1.2.3 of o/r is a draft or a pre-release, not a released version\n"
    );
}

/// Asserts that a release without `SHA256SUMS` exits 1 with the refusal naming it: the stable
/// index has nothing to be held to.
#[test]
fn a_release_without_sums_exits_one_naming_them() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-desired-nosums");
    let out = desired(
        &dir,
        "echo '{\"draft\":false,\"prerelease\":false,\"assets\":[{\"name\":\"js-agent_1.2.3_amd64.deb\"}]}'\n",
        &["--version", "1.2.3", "--after-run", ""],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).expect("utf-8"),
        "xtask: release v1.2.3 has no SHA256SUMS: there are no digests to hold the stable suite to\n"
    );
}
