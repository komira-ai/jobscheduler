//! The workflow lint of what a pull request runs: Linux x86_64 and nothing else, over every
//! workflow in `.github/workflows`, read as text.
//!
//! The owner's rule: x86_64 Linux tests are the pull request checks, because there are so many
//! of those hosts; every other platform (macOS, Linux arm64) is the beta stage of the release
//! pipeline (`release.yml` on a push to main, which calls `beta.yml` and the other workflows
//! the stage runs). So no job a pull request starts may name another runner.
//!
//! A job is one a pull request starts when
//!   - its workflow starts on `pull_request` or `pull_request_target`, or it is a job of a
//!     workflow that a job of such a workflow calls (`uses: ./.github/workflows/<file>`);
//!   - and its `if:` does not pin another event: no `&&`-joined part of it, outside
//!     parentheses, is `github.event_name == '<another event>'` or
//!     `github.event_name != 'pull_request'`.
//!
//! Such a job must run on `ubuntu-latest` or `ubuntu-<YY>.<MM>`: its `runs-on:` value (one
//! line, a list, or `labels:`) names only those, and a `${{ matrix.<key> }}` runner is read
//! through the values of `<key>` in the job's text. A `macos-*`, `windows-*`, `*-arm` or larger
//! runner, `self-hosted`, any other label, and an expression the text does not settle (inputs,
//! variables, the event, another job's outputs) are refused.
//!
//! A pull request workflow may not call a workflow of another repository, and a called
//! workflow may not call one again: the lint does not follow either. The negative cases run the
//! same check on workflows written here, and on the repository's own workflows with one line
//! changed.

mod cases;
mod lint;

use lint::{Workflow, block, lint};
use std::path::Path;

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

/// The repository's workflows with one line of `file` replaced.
fn changed(file: &str, from: &str, to: &str) -> Vec<(String, String)> {
    let mut workflows = repository_workflows();
    let w = workflows
        .iter_mut()
        .find(|(f, _)| f == file)
        .unwrap_or_else(|| panic!("{file}"));
    assert_eq!(w.1.matches(from).count(), 1, "{file}: {from}");
    w.1 = w.1.replace(from, to);
    workflows
}

#[test]
fn every_job_a_pull_request_starts_runs_on_linux_x86_64() {
    let (jobs, problems) = lint(&repository_workflows());
    assert_eq!(problems, Vec::<String>::new());
    // The lint is not vacuous: it sees the checks a pull request runs.
    for job in ["ci.yml/lint", "ci.yml/test", "ci.yml/coverage"] {
        assert!(jobs.iter().any(|j| j == job), "{job} in {jobs:?}");
    }
    // None of the beta stage's jobs is one.
    for job in &jobs {
        assert!(
            !job.starts_with("beta.yml/")
                && !job.starts_with("macos-probe.yml/")
                && !job.starts_with("js-agent-macos-pkg.yml/")
                && !job.contains("beta-macos")
                && !job.contains("artifacts.yml/build"),
            "{job}"
        );
    }
}

#[test]
fn a_pull_request_check_moved_back_to_macos_is_refused() {
    // ci.yml's `lint` job on macOS.
    let workflows = changed(
        "ci.yml",
        "  lint:\n    runs-on: ubuntu-latest\n",
        "  lint:\n    runs-on: macos-15\n",
    );
    assert_eq!(
        lint(&workflows).1,
        ["ci.yml/lint: runs on macos-15, not Linux x86_64 (ubuntu-*)"]
    );
}

#[test]
fn a_beta_job_without_its_push_condition_is_refused() {
    // release.yml starts on a pull request (its dry run); the beta jobs call workflows that run
    // on macOS and are kept off it only by the `if:` that pins a push.
    let gate = "  beta-macos:\n    needs: [plan, artifacts]\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}\n";
    let ungated = "  beta-macos:\n    needs: [plan, artifacts]\n";
    let (jobs, problems) = lint(&changed("release.yml", gate, ungated));
    assert!(
        jobs.iter()
            .any(|j| j == "release.yml/beta-macos -> beta.yml/macos"),
        "{jobs:?}"
    );
    assert_eq!(
        problems,
        ["release.yml/beta-macos -> beta.yml/macos: runs on macos-15, not Linux x86_64 (ubuntu-*)"]
    );
}

#[test]
fn the_release_dry_run_builds_nothing_on_arm_or_macos() {
    // artifacts.yml's build matrix names arm64 and macOS runners; only the `if:` of the
    // `artifacts` call keeps a pull request's dry run off it.
    let call = "    if: ${{ !cancelled() && github.event_name == 'push' && needs.plan.outputs.release == 'true' }}\n";
    let ungated = "    if: ${{ !cancelled() && needs.plan.outputs.release == 'true' }}\n";
    let (_, problems) = lint(&changed("release.yml", call, ungated));
    assert_eq!(
        problems,
        [
            "release.yml/artifacts -> artifacts.yml/build: runs on ubuntu-24.04-arm, macos-15, not Linux x86_64 (ubuntu-*)"
        ]
    );
}

#[test]
fn the_beta_stage_workflows_are_called_not_started_by_a_pull_request() {
    for file in ["beta.yml", "js-agent-macos-pkg.yml", "macos-probe.yml"] {
        let w = Workflow::parse(file, &read(file));
        assert!(!w.on_pull_request(), "{file}");
        assert!(
            w.top.iter().any(|l| l == "  workflow_call:"),
            "{file} is called by release.yml"
        );
        assert!(!w.top.iter().any(|l| l.contains("pull_request")), "{file}");
    }
    // release.yml's pull request trigger is its dry run, on a change to release.yml alone.
    let release = Workflow::parse("release.yml", &read("release.yml"));
    assert_eq!(
        release.top_block("on").expect("on"),
        [
            "push:",
            "branches: [main]",
            "pull_request:",
            "paths: ['.github/workflows/release.yml']",
        ]
    );
}

#[test]
fn the_beta_stage_runs_after_the_build_and_holds_back_only_the_macos_archives() {
    let text = read("release.yml");
    let w = Workflow::parse("release.yml", &text);
    let job = |name: &str| &w.jobs.iter().find(|(n, _)| n == name).expect(name).1;
    for (name, file) in [
        ("beta-macos", "beta.yml"),
        ("beta-macos-pkg", "js-agent-macos-pkg.yml"),
        ("beta-macos-probe", "macos-probe.yml"),
    ] {
        let lines = job(name);
        assert_eq!(
            block(lines, 4, "needs").expect("needs"),
            ["[plan, artifacts]"],
            "{name}"
        );
        assert_eq!(
            block(lines, 4, "uses").expect("uses"),
            [format!("./.github/workflows/{file}")],
            "{name}"
        );
        assert_eq!(
            block(lines, 4, "permissions").expect("permissions"),
            ["contents: read"],
            "{name}"
        );
        assert!(
            block(lines, 4, "if")
                .expect("if")
                .join(" ")
                .contains("github.event_name == 'push' && github.ref == 'refs/heads/main'"),
            "{name}"
        );
    }
    // Only `promote-macos` waits for the beta jobs; nothing else needs them.
    for (name, lines) in &w.jobs {
        let needs = block(lines, 4, "needs").unwrap_or_default().join(" ");
        let waits = needs.contains("beta-macos");
        assert_eq!(waits, name == "promote-macos", "{name}: needs {needs}");
    }
    let promote = job("promote-macos");
    assert_eq!(
        block(promote, 4, "needs").expect("needs"),
        ["[plan, publish, beta-macos, beta-macos-pkg, beta-macos-probe]"]
    );
    // `publish` leaves the macOS archives to it.
    assert!(text.contains(
        "            case \"$name\" in\n              *-aarch64-apple-darwin.tar.gz) continue ;;\n            esac\n"
    ));
}
