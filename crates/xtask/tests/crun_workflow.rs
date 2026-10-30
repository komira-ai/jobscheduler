//! The workflow that runs the real-`crun` tests (`.github/workflows/crun-tests.yml`), read as
//! text: what starts it, what it may do to the repository, what it installs and runs. A
//! workflow cannot be run from a test, so these are checks on the file; the run on a branch is
//! the proof that it works.

use std::path::Path;

use xtask::crun_tests::TARGETS;

const WORKFLOW: &str = ".github/workflows/crun-tests.yml";

fn root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines of the workflow that are not comments or blank, as they are.
fn code(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect()
}

/// The indented lines of the top-level block `key`.
fn block<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
    let head = format!("{key}:");
    lines
        .iter()
        .copied()
        .skip_while(|l| *l != head)
        .skip(1)
        .take_while(|l| l.starts_with(' '))
        .collect()
}

/// The commands of the `run:` steps: the text after `run:` on its line.
fn runs(text: &str) -> Vec<String> {
    code(text)
        .iter()
        .filter_map(|l| l.trim_start().trim_start_matches("- ").strip_prefix("run: "))
        .map(str::to_owned)
        .collect()
}

/// Only a push to main, a push to a `crun-try/` branch and a manual run start the workflow.
/// A pull request (or `pull_request_target`, a schedule, another workflow's run) would run
/// the tests, which need the network and `sudo`, on code from outside the repository.
#[test]
fn the_workflow_starts_only_on_a_push_to_main_a_try_branch_or_by_hand() {
    let text = read(WORKFLOW);
    let lines = code(&text);
    assert!(text.contains("name: crun-tests"));
    let on = block(&lines, "on");
    let events: Vec<&str> = on
        .iter()
        .filter(|l| !l.starts_with("   "))
        .map(|l| l.trim())
        .collect();
    assert_eq!(events, ["push:", "workflow_dispatch:"]);
    let branches: Vec<&str> = on
        .iter()
        .filter_map(|l| l.trim().strip_prefix("- "))
        .collect();
    assert_eq!(branches, ["main", "'crun-try/**'"]);
}

/// The token has no scope at the top, and the one job reads the repository's contents and
/// nothing else, in any spelling of a write.
#[test]
fn the_token_is_read_only_and_only_the_job_reads_contents() {
    let text = read(WORKFLOW);
    let lines = code(&text);
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.trim_start().starts_with("permissions:"))
            .map(|l| l.trim())
            .collect::<Vec<_>>(),
        ["permissions: {}", "permissions:"]
    );
    assert!(
        text.contains("    permissions:\n      contents: read\n    strategy:"),
        "the job's scopes"
    );
    assert!(!text.contains("write"), "a write scope or write command");
}

/// Every action is pinned to a commit, one of the three this repository's workflows use, and
/// the checkout leaves no credentials in the clone.
#[test]
fn every_action_is_pinned_and_the_checkout_keeps_no_credentials() {
    let text = read(WORKFLOW);
    let uses: Vec<String> = code(&text)
        .iter()
        .filter_map(|l| l.trim_start().trim_start_matches("- ").strip_prefix("uses: "))
        .map(str::to_owned)
        .collect();
    assert_eq!(uses.len(), 3, "{uses:?}");
    for action in &uses {
        let (name, rest) = action.split_once('@').expect("a version");
        assert!(
            [
                "actions/checkout",
                "actions/cache/restore",
                "actions/cache/save"
            ]
            .contains(&name),
            "{name}"
        );
        let sha = rest.split(' ').next().expect("a sha");
        assert!(
            sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "{action}"
        );
    }
    assert_eq!(text.matches("persist-credentials: false").count(), 1);
}

/// The commands are an allowlist, and none chains another, redirects or takes text from an
/// expression. The runner user is given its id ranges before the tests run, and the tests are
/// run last, by the verb that builds them and runs each target in its own unit.
#[test]
fn the_commands_are_these_and_the_tests_run_last() {
    let text = read(WORKFLOW);
    let runs = runs(&text);
    assert_eq!(
        runs,
        [
            "rustup toolchain install",
            "sudo apt-get update",
            "sudo apt-get install --yes --no-install-recommends crun uidmap",
            "sudo usermod --add-subuids 300000-365535 --add-subgids 300000-365535 \"$USER\"",
            "cargo xtask crun-tests",
        ]
    );
    for run in &runs {
        for chain in ["&&", "||", ";", "|", "`", "$(", ">", "${{"] {
            assert!(!run.contains(chain), "{chain} in {run}");
        }
    }
}

/// The tests run on both architectures, each on a hosted runner of its own, and one failing
/// does not cancel the other.
#[test]
fn both_architectures_run_and_one_failing_does_not_cancel_the_other() {
    let text = read(WORKFLOW);
    assert!(text.contains("    runs-on: ${{ matrix.runner }}"));
    assert!(text.contains("      fail-fast: false"));
    let runners: Vec<&str> = code(&text)
        .iter()
        .filter_map(|l| l.trim().strip_prefix("runner: "))
        .collect();
    assert_eq!(runners, ["ubuntu-24.04", "ubuntu-24.04-arm"]);
    // A hung job ends: the unit's bound is the harness's, this is the job's.
    assert!(text.contains("    timeout-minutes: 45"));
}

/// Every target the verb runs is a test target of `js-contain` that exists, a file or a
/// directory with a `main.rs` (a target renamed or deleted would otherwise fail only on the
/// runner), and every real-`crun` test file of the package is under one of them.
#[test]
fn every_target_is_a_test_target_of_the_package_and_each_crun_file_is_in_one() {
    let tests = root().join("crates/js-contain/tests");
    for target in TARGETS {
        let file = tests.join(format!("{target}.rs"));
        let main = tests.join(target).join("main.rs");
        assert!(file.is_file() || main.is_file(), "{target}");
    }
    let mut names: Vec<String> = std::fs::read_dir(&tests)
        .expect("tests directory")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("crun"))
        .map(|n| n.trim_end_matches(".rs").to_owned())
        .collect();
    names.sort();
    let mut want: Vec<String> = TARGETS.iter().map(|t| (*t).to_owned()).collect();
    want.sort();
    assert_eq!(names, want);
}
