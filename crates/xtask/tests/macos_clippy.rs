//! The pull request's macOS compile check, read from the workflows as text.
//!
//! ci.yml's `lint` job runs clippy for `aarch64-apple-darwin` on Linux, so that code under
//! `cfg(target_os = "macos")`, which no Linux build compiles, fails the pull request that
//! breaks it rather than the release's beta stage after the merge. The check covers what the
//! beta stage builds only when it compiles the same crates with the same features: these
//! tests hold the one `cargo-zigbuild clippy` of the `lint` job to the one `cargo test
//! --workspace` of beta.yml's `macos` job (the same `--exclude`s and `--features`), and to
//! `--workspace --all-targets --locked --target aarch64-apple-darwin ... -- -D warnings`, so
//! the tests of every crate are compiled and a warning fails it as on Linux.

use std::collections::BTreeSet;
use std::path::Path;

/// The target the check compiles for: the beta stage's Apple silicon.
const TARGET: &str = "aarch64-apple-darwin";

/// The text of `.github/workflows/<name>`.
fn workflow(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The lines of job `job` in `text`: from its `  <job>:` line to the next job's.
fn job_lines<'a>(text: &'a str, job: &str) -> Vec<&'a str> {
    let header = format!("  {job}:");
    text.lines()
        .skip_while(|l| *l != header)
        .skip(1)
        .take_while(|l| l.is_empty() || l.starts_with("   ") || l.starts_with("  #"))
        .collect()
}

/// The words of the one `run:` step of `job` in `text` that contains `command`.
fn the_run(text: &str, job: &str, command: &str) -> Result<Vec<String>, String> {
    let found: Vec<&str> = job_lines(text, job)
        .into_iter()
        .filter_map(|l| l.trim_start().strip_prefix("- run: "))
        .filter(|run| run.contains(command))
        .collect();
    match found.as_slice() {
        [run] => Ok(run.split_whitespace().map(str::to_owned).collect()),
        _ => Err(format!(
            "job `{job}` has {} `run:` steps with `{command}`; it needs exactly one",
            found.len()
        )),
    }
}

/// The values of every `flag <value>` in `words`.
fn values(words: &[String], flag: &str) -> BTreeSet<String> {
    words
        .windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1].clone())
        .collect()
}

/// Checks ci.yml's macOS clippy against beta.yml's macOS tests: every reason it fails.
fn check(ci: &str, beta: &str) -> Result<(), Vec<String>> {
    let clippy = the_run(ci, "lint", "cargo-zigbuild clippy").map_err(|e| vec![e])?;
    let tests = the_run(beta, "macos", "cargo test --workspace").map_err(|e| vec![e])?;
    let mut errors = Vec::new();
    for flag in ["--workspace", "--all-targets", "--locked"] {
        if !clippy.iter().any(|w| w == flag) {
            errors.push(format!("the macOS clippy lacks {flag}"));
        }
    }
    if values(&clippy, "--target") != BTreeSet::from([TARGET.to_owned()]) {
        errors.push(format!("the macOS clippy is not for --target {TARGET}"));
    }
    if !clippy.ends_with(&["--".to_owned(), "-D".to_owned(), "warnings".to_owned()]) {
        errors.push("the macOS clippy does not end with `-- -D warnings`".to_owned());
    }
    for flag in ["--exclude", "--features"] {
        let (linted, tested) = (values(&clippy, flag), values(&tests, flag));
        if linted != tested {
            errors.push(format!(
                "the macOS clippy's {flag} {linted:?} are not beta.yml's {tested:?}"
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// A `lint` job and a `macos` job in the shape the rule asks for.
const CI: &str = "jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      # The macOS clippy.
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings

      - run: cargo-zigbuild clippy --workspace --all-targets --locked --target aarch64-apple-darwin --features f/x --exclude a --exclude b -- -D warnings
  test:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test --workspace
";

const BETA: &str = "jobs:
  macos:
    runs-on: macos-15
    steps:
      - run: cargo build --workspace --locked --exclude a --exclude b
      - run: cargo test --workspace --locked --no-fail-fast --features f/x --exclude a --exclude b
";

/// `text` with `from` replaced by `to`, which must occur in it.
fn with(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from}");
    text.replacen(from, to, 1)
}

/// Asserts that the repository's ci.yml compiles for macOS what beta.yml tests on macOS.
#[test]
fn the_pull_request_compiles_for_macos_what_the_beta_stage_tests() {
    assert_eq!(check(&workflow("ci.yml"), &workflow("beta.yml")), Ok(()));
}

/// Asserts that the shape passes, with blank lines and comments inside the job.
#[test]
fn the_shape_passes() {
    assert_eq!(check(CI, BETA), Ok(()));
}

/// Asserts that a `lint` job with no macOS clippy is refused, and so is one whose macOS clippy
/// sits in another job, or appears twice.
#[test]
fn a_missing_misplaced_or_doubled_clippy_is_refused() {
    let line = "      - run: cargo-zigbuild clippy --workspace --all-targets --locked --target aarch64-apple-darwin --features f/x --exclude a --exclude b -- -D warnings\n";
    let none = "job `lint` has 0 `run:` steps with `cargo-zigbuild clippy`; it needs exactly one";
    assert_eq!(check(&with(CI, line, ""), BETA), Err(vec![none.to_owned()]));
    let moved = format!("{}{line}", with(CI, line, ""));
    assert_eq!(check(&moved, BETA), Err(vec![none.to_owned()]));
    let twice = with(CI, line, &format!("{line}{line}"));
    assert_eq!(
        check(&twice, BETA),
        Err(vec![
            "job `lint` has 2 `run:` steps with `cargo-zigbuild clippy`; it needs exactly one"
                .to_owned()
        ])
    );
}

/// Asserts that a beta.yml with no workspace test step in its `macos` job is refused.
#[test]
fn a_beta_stage_with_no_workspace_test_is_refused() {
    let beta = with(BETA, "cargo test --workspace", "cargo test -p a");
    assert_eq!(
        check(CI, &beta),
        Err(vec![
            "job `macos` has 0 `run:` steps with `cargo test --workspace`; it needs exactly one"
                .to_owned()
        ])
    );
}

/// Asserts that a macOS clippy that drops a whole-workspace flag, builds for another target
/// (or for none), or lets warnings pass is refused, naming each fault.
#[test]
fn a_clippy_that_compiles_less_or_lets_warnings_pass_is_refused() {
    let cases = [
        (
            " --all-targets --locked --target",
            " --locked --target",
            "the macOS clippy lacks --all-targets",
        ),
        (
            "zigbuild clippy --workspace",
            "zigbuild clippy",
            "the macOS clippy lacks --workspace",
        ),
        (
            "--locked --target",
            "--target",
            "the macOS clippy lacks --locked",
        ),
        (
            "--target aarch64-apple-darwin",
            "--target x86_64-apple-darwin",
            "the macOS clippy is not for --target aarch64-apple-darwin",
        ),
        (
            " --target aarch64-apple-darwin",
            "",
            "the macOS clippy is not for --target aarch64-apple-darwin",
        ),
        (
            "--exclude b -- -D warnings",
            "--exclude b",
            "the macOS clippy does not end with `-- -D warnings`",
        ),
    ];
    for (from, to, says) in cases {
        let ci = with(CI, from, to);
        assert_eq!(
            check(&ci, BETA),
            Err(vec![says.to_owned()]),
            "{from} -> {to}"
        );
    }
}

/// Asserts that a macOS clippy that excludes a crate beta.yml tests, or compiles a crate
/// beta.yml excludes, or takes other features, is refused, naming both lists.
#[test]
fn a_clippy_whose_crates_or_features_are_not_the_beta_stages_is_refused() {
    let excludes =
        "the macOS clippy's --exclude {\"a\", \"b\", \"c\"} are not beta.yml's {\"a\", \"b\"}";
    let ci = with(CI, "--exclude b -- -D", "--exclude b --exclude c -- -D");
    assert_eq!(check(&ci, BETA), Err(vec![excludes.to_owned()]));

    let fewer =
        "the macOS clippy's --exclude {\"a\", \"b\"} are not beta.yml's {\"a\", \"b\", \"c\"}";
    let beta = with(
        BETA,
        "--features f/x --exclude a --exclude b\n",
        "--features f/x --exclude a --exclude b --exclude c\n",
    );
    assert_eq!(check(CI, &beta), Err(vec![fewer.to_owned()]));

    let features = "the macOS clippy's --features {} are not beta.yml's {\"f/x\"}";
    let ci = with(CI, " --features f/x", "");
    assert_eq!(check(&ci, BETA), Err(vec![features.to_owned()]));
}
