//! The longest test binaries of `js-sim-cell` stay out of a pull request's `test` job and
//! run everywhere else.
//!
//! `crates/js-sim-cell/Cargo.toml` gates four test binaries behind the feature
//! `slow-families` (`required-features`). Cargo builds none of them unless the feature is
//! on, so the `test` job of `ci.yml`, which does not turn it on, leaves them out. The runs
//! that must still hold them turn it on: the coverage shards and the coverage report's
//! build in `ci.yml` (every pull request), the release's `unit-tests` (every push to main,
//! whose red `release-red` reports) and the beta stage's workspace tests. A run that
//! dropped the feature would lose those binaries without a failure, so this reads the
//! four files as text and refuses each such change.

use std::collections::BTreeSet;
use std::path::Path;

/// The test binaries the feature gates.
const GATED: [&str; 4] = ["faults", "smoke", "replay", "whole_host_prepare"];

/// The flag that builds the gated binaries over the workspace.
const FLAG: &str = "--features js-sim-cell/slow-families";

/// The text of job `name` in `workflow`: its key line and every line under it, up to the
/// next job (a line indented two spaces) or the end of the file.
fn job<'a>(workflow: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("\n  {name}:\n");
    let start = workflow.find(&key)? + 1;
    let body = &workflow[start + key.len() - 1..];
    let end = body
        .match_indices('\n')
        .map(|(i, _)| i + 1)
        .find(|&i| {
            let next = &body[i..];
            next.starts_with("  ") && !next.starts_with("   ") && !next.starts_with("  #")
        })
        .unwrap_or(body.len());
    Some(&workflow[start..start + key.len() - 1 + end])
}

/// The lines of `text` that run `command` (a `run:` line holding it), comments left out.
fn runs<'a>(text: &'a str, command: &str) -> Vec<&'a str> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#') && l.contains(command))
        .collect()
}

/// What is wrong with the four files; empty when they agree.
fn check(
    ci: &str,
    release: &str,
    beta: &str,
    manifest: &str,
    tests: &BTreeSet<String>,
) -> Vec<String> {
    let mut errors = Vec::new();

    // The manifest gates exactly the four binaries, each a file under tests/.
    let gated: BTreeSet<String> = manifest
        .split("[[test]]")
        .skip(1)
        .filter(|block| {
            let block = block.split("\n[").next().unwrap_or(block);
            block.contains("required-features = [\"slow-families\"]")
        })
        .filter_map(|block| {
            block
                .lines()
                .find_map(|l| l.trim().strip_prefix("name = \""))
                .and_then(|l| l.strip_suffix('"'))
                .map(str::to_owned)
        })
        .collect();
    let wanted: BTreeSet<String> = GATED.iter().map(|s| (*s).to_owned()).collect();
    if gated != wanted {
        errors.push(format!(
            "js-sim-cell gates {gated:?} behind slow-families, expected {wanted:?}"
        ));
    }
    for name in &gated {
        if !tests.contains(name) {
            errors.push(format!("the gated binary {name} has no tests/{name}.rs"));
        }
    }
    if !manifest.lines().any(|l| l == "slow-families = []") {
        errors.push("js-sim-cell has no feature `slow-families = []`".to_owned());
    }

    // The pull request's `test` job does not build them.
    match job(ci, "test") {
        None => errors.push("ci.yml has no test job".to_owned()),
        Some(test) => {
            if !runs(test, "slow-families").is_empty() || !runs(test, "--all-features").is_empty() {
                errors.push("ci.yml's test job turns slow-families on".to_owned());
            }
            if !test.lines().any(|l| {
                let l = l.trim();
                l.starts_with("- run: cargo test --workspace --locked --no-fail-fast")
                    && !l.contains("--tests")
            }) {
                errors.push("ci.yml's test job no longer runs the workspace's tests".to_owned());
            }
        }
    }

    // Every run that must hold them turns the feature on, on each of its test runs.
    let mut need = |what: &str, text: Option<&str>, command: &str| match text {
        None => errors.push(format!("no {what}")),
        Some(text) => {
            let lines = runs(text, command);
            if lines.is_empty() {
                errors.push(format!("{what} has no `{command}` run"));
            }
            for line in lines {
                if !line.contains(FLAG) {
                    errors.push(format!("{what} runs `{command}` without {FLAG}"));
                }
            }
        }
    };
    need(
        "ci.yml's coverage-shard job",
        job(ci, "coverage-shard"),
        "llvm-cov nextest --workspace",
    );
    need(
        "ci.yml's coverage job",
        job(ci, "coverage"),
        "llvm-cov nextest --workspace",
    );
    need(
        "release.yml's unit-tests job",
        job(release, "unit-tests"),
        "nextest run --workspace",
    );
    need(
        "ci.yml's lint job",
        job(ci, "lint"),
        "cargo clippy --workspace",
    );
    need("beta.yml", Some(beta), "cargo test --workspace");
    errors
}

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn tests_dir() -> BTreeSet<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../js-sim-cell/tests");
    std::fs::read_dir(dir)
        .expect("read tests/")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".rs").map(str::to_owned))
        .collect()
}

struct Files {
    ci: String,
    release: String,
    beta: String,
    manifest: String,
    tests: BTreeSet<String>,
}

impl Files {
    fn load() -> Files {
        Files {
            ci: read(".github/workflows/ci.yml"),
            release: read(".github/workflows/release.yml"),
            beta: read(".github/workflows/beta.yml"),
            manifest: read("crates/js-sim-cell/Cargo.toml"),
            tests: tests_dir(),
        }
    }

    fn errors(&self) -> Vec<String> {
        check(
            &self.ci,
            &self.release,
            &self.beta,
            &self.manifest,
            &self.tests,
        )
    }
}

/// Asserts that the repository's own files agree: the four binaries are gated, the pull
/// request's `test` job leaves them out, and every other run turns them on.
#[test]
fn the_repository_runs_the_slow_families_everywhere_but_the_test_job() {
    assert_eq!(Files::load().errors(), Vec::<String>::new());
}

/// Asserts that turning the feature on in the `test` job (which would put the binaries
/// back into the job the pull request waits for) is refused.
#[test]
fn the_test_job_turning_the_feature_on_is_refused() {
    let mut f = Files::load();
    let run = "      - run: cargo test --workspace --locked --no-fail-fast\n";
    assert!(f.ci.contains(run));
    f.ci = f.ci.replacen(
        run,
        &format!("      - run: cargo test --workspace --locked --no-fail-fast {FLAG}\n"),
        1,
    );
    assert_eq!(
        f.errors(),
        vec!["ci.yml's test job turns slow-families on".to_owned()]
    );
}

/// Asserts that a `test` job that stops running the workspace's tests is refused, so
/// leaving the feature out cannot pass by running nothing.
#[test]
fn a_test_job_that_runs_no_workspace_tests_is_refused() {
    let mut f = Files::load();
    let run = "      - run: cargo test --workspace --locked --no-fail-fast\n";
    f.ci =
        f.ci.replacen(run, "      - run: cargo test -p js-state --locked\n", 1);
    assert_eq!(
        f.errors(),
        vec!["ci.yml's test job no longer runs the workspace's tests".to_owned()]
    );
}

/// Asserts that a clippy run of the lint job without the feature is refused.
#[test]
fn a_clippy_run_without_the_feature_is_refused() {
    let mut f = Files::load();
    f.ci = f.ci.replacen(
        &format!("--locked {FLAG} -- -D warnings"),
        "--locked -- -D warnings",
        1,
    );
    let errors = f.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].starts_with("ci.yml's lint job runs `cargo clippy --workspace` without"),
        "{errors:?}"
    );
}

/// Asserts that a coverage shard, the coverage report's build, the release's `unit-tests`
/// and the beta stage that drop the feature are each refused by name.
#[test]
fn a_run_that_drops_the_feature_is_refused() {
    let cases = [
        (
            "ci",
            "ci.yml's coverage-shard job runs `llvm-cov nextest --workspace` without",
        ),
        (
            "ci-report",
            "ci.yml's coverage job runs `llvm-cov nextest --workspace` without",
        ),
        (
            "release",
            "release.yml's unit-tests job runs `nextest run --workspace` without",
        ),
        ("beta", "beta.yml runs `cargo test --workspace` without"),
    ];
    for (which, expect) in cases {
        let mut f = Files::load();
        let flagged = |text: &str, from: usize| -> String {
            // Drop the flag from the first run after `from`.
            let at = from + text[from..].find(FLAG).expect("flag");
            format!("{}{}", &text[..at - 1], &text[at + FLAG.len()..])
        };
        match which {
            "ci" => {
                let from = f.ci.find("\n  coverage-shard:\n").expect("shard");
                f.ci = flagged(&f.ci, from);
            }
            "ci-report" => {
                let from = f.ci.find("\n  coverage:\n").expect("coverage");
                f.ci = flagged(&f.ci, from);
            }
            "release" => {
                let from = f.release.find("\n  unit-tests:\n").expect("unit-tests");
                f.release = flagged(&f.release, from);
            }
            _ => f.beta = flagged(&f.beta, 0),
        }
        let errors = f.errors();
        assert_eq!(errors.len(), 1, "{which}: {errors:?}");
        assert!(errors[0].starts_with(expect), "{which}: {errors:?}");
    }
}

/// Asserts that a gated binary that loses its gate (so it runs in every `test` job again
/// without the check seeing it), one gated without a file, and a fifth gated binary are
/// each refused.
#[test]
fn a_manifest_that_changes_the_gated_set_is_refused() {
    let gate = "required-features = [\"slow-families\"]";

    let mut f = Files::load();
    f.manifest = f.manifest.replacen(gate, "", 1);
    let errors = f.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].starts_with("js-sim-cell gates"), "{errors:?}");

    let mut f = Files::load();
    f.tests.remove("faults");
    assert_eq!(
        f.errors(),
        vec!["the gated binary faults has no tests/faults.rs".to_owned()]
    );

    let mut f = Files::load();
    f.manifest.push_str(&format!(
        "\n[[test]]\nname = \"usage\"\npath = \"tests/usage.rs\"\n{gate}\n"
    ));
    let errors = f.errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].starts_with("js-sim-cell gates"), "{errors:?}");

    let mut f = Files::load();
    f.manifest = f.manifest.replacen("slow-families = []\n", "", 1);
    assert_eq!(
        f.errors(),
        vec!["js-sim-cell has no feature `slow-families = []`".to_owned()]
    );
}
