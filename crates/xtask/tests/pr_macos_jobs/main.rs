//! The workflow lint of the macOS jobs a pull request runs, over every workflow in
//! `.github/workflows`, read as text.
//!
//! A job is a pull request macOS job when
//!   - its workflow starts on `pull_request` or `pull_request_target`, or it is a job of a
//!     workflow that a job of such a workflow calls (`uses: ./.github/workflows/<file>`);
//!   - its runner names macOS: its `runs-on:` value (one line or a list under it) has `macos`
//!     in it, or it is an expression and the job's text (its matrix) has `macos` in it. A
//!     `runs-on:` expression of inputs, variables, the event or another job's outputs, with no
//!     `macos` in the job, is refused: the lint cannot tell what it runs on;
//!   - and its `if:` does not pin another event: no `&&`-joined part of it, outside
//!     parentheses, is `github.event_name == '<another event>'` or
//!     `github.event_name != 'pull_request'`.
//!
//! Such a job must
//!   - name no secret (the word `secrets` in any spelling: `secrets.X`, `secrets['X']`,
//!     `toJSON(secrets)`, a `secrets:` key) and not the job's token (`github.token`,
//!     `github['token']`, `toJSON(github)`, `GITHUB_TOKEN`): not in its own lines, not in its workflow's top-level lines, and not in
//!     the job that calls it;
//!   - run in no deployment environment (`environment:`), whose secrets it would get;
//!   - hold no write permission, `id-token: write` (OIDC) included, under a permissions block
//!     of its own or its workflow's (without either it gets the repository's default token,
//!     which may write); values are read without quotes and trailing comments, and a block
//!     written as a `{ ... }` mapping is refused;
//!   - be in a workflow whose concurrency cancels the older run of the same pull request (a
//!     group with the pull request's number and nothing that differs per run; for a called
//!     workflow, the caller's), except the release dry run (`RELEASE_DRY_RUN`).
//!
//! A pull request workflow may not call a workflow of another repository, and a called
//! workflow may not call one again: the lint does not follow either. The set of pull request macOS jobs is pinned (`EXPECTED`). The negative cases run
//! the same check on workflows written here, and on the repository's own workflows with one
//! line changed.

mod cases;
mod lint;

use lint::{RELEASE_DRY_RUN, Workflow, block, cancels_older_pull_request_runs, lint};
use std::path::Path;

/// The pull request macOS jobs of the repository: `<file>/<job>`, and for a called workflow's
/// job `<file>/<calling job> -> <called file>/<job>`.
const EXPECTED: [&str; 4] = [
    "ci.yml/macos",
    "js-agent-macos-pkg.yml/build",
    "macos-probe.yml/probe",
    "release.yml/artifacts -> artifacts.yml/build",
];

fn repo() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

/// Every workflow of the repository: file name and text, sorted by name.
fn repository_workflows() -> Vec<(String, String)> {
    let dir = repo().join(".github/workflows");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read .github/workflows") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if name.ends_with(".yml") || name.ends_with(".yaml") {
            found.push((name, std::fs::read_to_string(&path).expect("read workflow")));
        }
    }
    found.sort();
    assert!(found.len() > 5, "{found:?}");
    found
}

fn read(file: &str) -> String {
    std::fs::read_to_string(repo().join(".github/workflows").join(file)).expect("read workflow")
}

#[test]
fn the_repository_s_pull_request_macos_jobs_are_the_expected_ones_and_hold_nothing() {
    let (jobs, problems) = lint(&repository_workflows());
    assert_eq!(problems, Vec::<String>::new());
    assert_eq!(jobs, EXPECTED);
}

#[test]
fn the_repository_s_macos_job_with_a_secret_added_is_refused() {
    let mut workflows = repository_workflows();
    let ci = workflows
        .iter_mut()
        .find(|(f, _)| f == "ci.yml")
        .expect("ci.yml");
    let at = "      MACOSX_DEPLOYMENT_TARGET: \"11.0\"\n";
    assert_eq!(ci.1.matches(at).count(), 1);
    ci.1 = ci.1.replace(
        at,
        &format!("{at}      NOTARY: ${{{{ secrets.MACOS_NOTARY_KEY_P8 }}}}\n"),
    );
    assert_eq!(lint(&workflows).1, ["ci.yml/macos: the job names a secret"]);
}

#[test]
fn the_package_s_sign_job_on_a_pull_request_is_refused() {
    // The sign job holds the signing secrets and the environment; only its `if:` keeps it off
    // the pull requests the workflow runs on.
    let mut workflows = repository_workflows();
    let pkg = workflows
        .iter_mut()
        .find(|(f, _)| f == "js-agent-macos-pkg.yml")
        .expect("pkg");
    let gate = "    if: ${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' }}\n";
    assert_eq!(pkg.1.matches(gate).count(), 1);
    pkg.1 = pkg.1.replace(gate, "");
    let (jobs, problems) = lint(&workflows);
    let sign = "js-agent-macos-pkg.yml/sign";
    assert!(jobs.iter().any(|j| j == sign), "{jobs:?}");
    assert_eq!(
        problems,
        [
            format!("{sign}: the job names a secret"),
            format!("{sign}: runs in a deployment environment"),
        ]
    );
}

#[test]
fn the_release_dry_run_starts_only_on_a_change_to_release_yml() {
    let text = read(RELEASE_DRY_RUN);
    let w = Workflow::parse(RELEASE_DRY_RUN, &text);
    assert!(w.on_pull_request());
    assert_eq!(
        w.top_block("on").expect("on"),
        [
            "push:",
            "branches: [main]",
            "pull_request:",
            "paths: ['.github/workflows/release.yml']",
        ]
    );
    // It is the one workflow excused from cancelling, and it would be refused without that.
    assert!(!cancels_older_pull_request_runs(&w));
}

#[test]
fn every_pull_request_runs_the_two_macos_checks_whatever_it_changes() {
    // ci.yml's macos job: no filter job, no condition, no paths on the trigger.
    let ci = Workflow::parse("ci.yml", &read("ci.yml"));
    assert!(!ci.jobs.iter().any(|(n, _)| n == "changes"), "a filter job");
    let macos = &ci.jobs.iter().find(|(n, _)| n == "macos").expect("macos").1;
    assert_eq!(block(macos, 4, "if"), None);
    assert_eq!(block(macos, 4, "needs"), None);
    assert_eq!(block(macos, 4, "runs-on").expect("runs-on"), ["macos-15"]);
    assert_eq!(
        ci.top_block("on").expect("on"),
        [
            "pull_request:",
            "types: [opened, synchronize, reopened]",
            "push:",
            "branches: [main]",
        ]
    );
    // It runs the workspace's tests, all but the three Linux-only crates.
    let text = read("ci.yml");
    assert!(text.contains(
        "      - run: cargo test --workspace --locked --no-fail-fast --exclude js-contain --exclude js-host-step --exclude js-mdm\n"
    ));
    // The package build: a pull request trigger with no paths, a job with no condition, and the
    // check name the branch protection requires.
    let pkg = Workflow::parse("js-agent-macos-pkg.yml", &read("js-agent-macos-pkg.yml"));
    assert_eq!(
        pkg.top_block("on").expect("on")[..2],
        ["pull_request:", "workflow_dispatch:"]
    );
    let build = &pkg
        .jobs
        .iter()
        .find(|(n, _)| n == "build")
        .expect("build")
        .1;
    assert_eq!(block(build, 4, "if"), None);
    assert_eq!(
        block(build, 4, "name").expect("name"),
        ["js-agent-macos-pkg"]
    );
}

#[test]
fn the_required_checks_are_the_names_the_two_macos_jobs_report() {
    // The held script makes main require these two names, and they are the check names of the
    // two macOS jobs every pull request runs.
    let script =
        std::fs::read_to_string(repo().join("tools/held/required-checks/require_macos_checks.sh"))
            .expect("held script");
    assert!(script.contains("checks=(macos js-agent-macos-pkg)\n"));
    assert!(script.contains(r#""contexts":["macos","js-agent-macos-pkg"]"#));
    let ci = Workflow::parse("ci.yml", &read("ci.yml"));
    let macos = &ci.jobs.iter().find(|(n, _)| n == "macos").expect("macos").1;
    // A job with no `name:` reports its key.
    assert_eq!(block(macos, 4, "name"), None);
    let pkg = Workflow::parse("js-agent-macos-pkg.yml", &read("js-agent-macos-pkg.yml"));
    let build = &pkg
        .jobs
        .iter()
        .find(|(n, _)| n == "build")
        .expect("build")
        .1;
    assert_eq!(
        block(build, 4, "name").expect("name"),
        ["js-agent-macos-pkg"]
    );
}

/// The value of `key:` on the first line under `under` (a line of `text`) that has it.
fn value_after<'a>(text: &'a str, under: &str, key: &str) -> &'a str {
    let rest = &text[text.find(under).unwrap_or_else(|| panic!("{under}"))..];
    let line = rest
        .lines()
        .find(|l| l.trim_start().starts_with(key))
        .unwrap_or_else(|| panic!("{key} under {under}"));
    line.trim_start()[key.len()..].trim()
}

#[test]
fn the_pull_request_build_makes_the_binaries_of_the_release_s_macos_leg() {
    let ci = read("ci.yml");
    // The release's legs are artifacts.yml's `build`, which release.yml calls.
    let release = read("artifacts.yml");
    let pr = value_after(
        &ci,
        "      - name: Build, stage and package the release's macOS binaries\n",
        "BUILDS:",
    );
    let leg = value_after(
        &release,
        "          - target: aarch64-apple-darwin\n",
        "builds:",
    );
    assert_eq!(pr, leg);
    assert_eq!(
        value_after(
            &ci,
            "      - name: Build, stage and package the release's macOS binaries\n",
            "TARGET:"
        ),
        "aarch64-apple-darwin"
    );
    // Only on a pull request: on main the release's own leg makes the same files.
    assert_eq!(
        value_after(
            &ci,
            "      - name: Build, stage and package the release's macOS binaries\n",
            "if:"
        ),
        "${{ !cancelled() && github.event_name == 'pull_request' }}"
    );
    // One command, the xtask verb (tests/release_macos_leg.rs pins what it runs), against the
    // pinned macOS; the leg's artifacts.yml steps run the same verbs and checks.
    assert_eq!(
        value_after(
            &ci,
            "      - name: Build, stage and package the release's macOS binaries\n",
            "run:"
        ),
        "cargo xtask release macos-leg --target \"$TARGET\" --builds \"$BUILDS\" --min-macos \"$MACOSX_DEPLOYMENT_TARGET\" --out \"$RUNNER_TEMP/release-leg\""
    );
    for needle in [
        "cargo xtask release stage --bin \"$bin\" --target \"$TARGET\"",
        "cargo xtask release hostkeeper-package --binary \"target/$TARGET/release/js-agent\" --target \"$TARGET\"",
        "otool -l",
        "--version",
    ] {
        assert!(release.contains(needle), "artifacts.yml: {needle}");
    }
    assert!(ci.contains("  MACOSX_DEPLOYMENT_TARGET: \"11.0\"\n"));
}
