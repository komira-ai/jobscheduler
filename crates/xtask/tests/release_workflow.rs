//! The rules the release workflow keeps (the comments at the top of
//! `.github/workflows/release.yml`), read from the file as text: actions pinned to commit
//! SHAs, no permission at the top, hosted runners, a push to main as the only trigger that
//! releases, a read-only wait for `ci`, `id-token: write` and `contents: write` only where
//! they are meant, and the gamma step's shape.

use std::path::Path;

/// The workflow's text.
fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/release.yml");
    std::fs::read_to_string(path).expect("read release.yml")
}

/// The jobs: name and the lines of its block (the lines under `jobs:` split at the
/// two-space keys).
fn jobs(text: &str) -> Vec<(String, Vec<String>)> {
    let mut jobs: Vec<(String, Vec<String>)> = Vec::new();
    let mut in_jobs = false;
    for line in text.lines() {
        if line == "jobs:" {
            in_jobs = true;
        } else if in_jobs {
            let key = line
                .strip_prefix("  ")
                .filter(|rest| !rest.starts_with([' ', '#']))
                .and_then(|rest| rest.strip_suffix(':'));
            if let Some(name) = key {
                jobs.push((name.to_owned(), Vec::new()));
            } else if let Some((_, lines)) = jobs.last_mut() {
                lines.push(line.to_owned());
            }
        }
    }
    jobs
}

fn job<'a>(jobs: &'a [(String, Vec<String>)], name: &str) -> &'a [String] {
    &jobs
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no job {name}"))
        .1
}

/// Asserts that every `uses:` names a full 40-digit commit SHA with its version in a
/// comment, that the top level grants nothing, and that every job runs on a GitHub-hosted
/// runner image of a pinned version.
#[test]
fn actions_are_pinned_and_the_top_grants_nothing() {
    let text = workflow();
    let mut uses = 0;
    for line in text
        .lines()
        .filter(|l| l.trim_start().starts_with("- uses:") || l.trim_start().starts_with("uses:"))
    {
        uses += 1;
        let (_, rest) = line.split_once('@').unwrap_or_else(|| panic!("{line}"));
        let (sha, comment) = rest.split_once(' ').unwrap_or_else(|| panic!("{line}"));
        assert!(
            sha.len() == 40 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{line}"
        );
        assert!(comment.starts_with("# v"), "{line}");
    }
    assert!(uses >= 8, "{uses} uses: lines");
    assert!(text.lines().any(|l| l == "permissions: {}"));
    for (name, lines) in jobs(&text) {
        let runs_on: Vec<&String> = lines
            .iter()
            .filter(|l| l.trim_start().starts_with("runs-on:"))
            .collect();
        assert_eq!(runs_on.len(), 1, "{name}");
        assert!(
            runs_on[0].contains("ubuntu-24.04") || runs_on[0].contains("matrix.runner"),
            "{name}: {}",
            runs_on[0]
        );
    }
}

/// Asserts that a push to main is the only trigger that releases (the other is a pull
/// request, the dry run), that the run's commit is its own `github.sha`, and that every job
/// that attests, publishes or deploys, and the wait and the plan, tests for a push to main
/// of its own condition; the other jobs follow `plan`'s output.
#[test]
fn a_push_to_main_is_the_only_releasing_trigger() {
    let text = workflow();
    // The whole trigger block: nothing else starts a run.
    let on = text
        .split_once("\non:\n")
        .and_then(|(_, rest)| rest.split_once("\npermissions:"))
        .map(|(on, _)| on)
        .expect("on block");
    assert_eq!(
        on,
        "  push:\n    branches: [main]\n  pull_request:\n    paths: ['.github/workflows/release.yml']\n"
    );
    assert!(text.contains("\n  COMMIT: ${{ github.sha }}\n"));
    assert!(!text.contains("workflow_run."));
    // The dry run still builds: with the wait skipped, the build and the sums run on the
    // plan's word alone.
    for name in ["build", "sums"] {
        let ifs: Vec<&String> = job(&jobs(&text), name)
            .iter()
            .filter(|l| l.starts_with("    if:"))
            .collect();
        assert_eq!(ifs.len(), 1, "{name}");
        assert!(
            ifs[0].contains("!cancelled() && needs.plan.outputs.release == 'true'"),
            "{name}: {}",
            ifs[0]
        );
    }
    let push = "github.event_name == 'push' && github.ref == 'refs/heads/main'";
    let jobs = jobs(&text);
    for name in [
        "ci-passed",
        "plan",
        "attest",
        "verify",
        "publish",
        "deploy-tool",
        "gamma",
    ] {
        let ifs: Vec<&String> = job(&jobs, name)
            .iter()
            .filter(|l| l.starts_with("    if:"))
            .collect();
        assert_eq!(ifs.len(), 1, "{name}");
        assert!(ifs[0].contains(push), "{name}: {}", ifs[0]);
    }
    // The dry run: the wait is skipped on a pull request and `plan` runs anyway.
    let plan = job(&jobs, "plan").join("\n");
    assert!(
        plan.contains("github.event_name == 'pull_request'"),
        "{plan}"
    );
    assert!(
        plan.contains("needs.ci-passed.result == 'success'"),
        "{plan}"
    );
    assert!(!plan.contains("--tip"));
    // One group for the runs that release, never cancelled.
    assert!(text.contains("group: release-${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && 'main' || github.run_id }}"));
    assert!(text.contains("cancel-in-progress: false"));
}

/// Asserts the wait for `ci`: a job with exactly `actions: read` and nothing else, no
/// checkout (no repository code runs in it), a four-hour limit, a poll every 120 s of the
/// `ci` runs of this commit's push, and a failure naming the run for any conclusion but
/// success.
#[test]
fn the_wait_for_ci_reads_actions_and_runs_no_repository_code() {
    let text = workflow();
    let jobs = jobs(&text);
    let wait = job(&jobs, "ci-passed").join("\n");
    assert!(
        wait.contains("    permissions:\n      actions: read\n    steps:"),
        "{wait}"
    );
    assert_eq!(wait.matches("permissions:").count(), 1, "{wait}");
    assert!(!wait.contains("uses:") && !wait.contains("checkout") && !wait.contains("cargo "));
    assert!(wait.contains("timeout-minutes: 240"));
    assert!(wait.contains("workflows/ci.yml/runs?head_sha=$COMMIT&event=push"));
    assert!(wait.contains("sleep 120"));
    assert!(wait.contains("completed)"));
    assert!(wait.contains(r#"if [ "$conclusion" = success ]"#));
    assert!(wait.contains("::error::ci did not pass for $COMMIT: conclusion $conclusion, $url"));
    // Nothing is planned before the wait has passed.
    assert!(job(&jobs, "plan").join("\n").contains("needs: [ci-passed]"));
}

/// Asserts that only `publish` holds `contents: write`, and that it checks the latest
/// release tag is a proper ancestor of the commit before the publishing call.
#[test]
fn only_publish_holds_contents_write_and_checks_the_tag_order() {
    let text = workflow();
    let holders: Vec<String> = jobs(&text)
        .into_iter()
        .filter(|(_, lines)| lines.iter().any(|l| l.trim() == "contents: write"))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(holders, ["publish"]);
    let jobs = jobs(&text);
    let publish = job(&jobs, "publish").join("\n");
    let compare = publish
        .find(r#"gh api "$api/compare/$latest_sha...$COMMIT" --jq .status"#)
        .expect("compare");
    let patch = publish.find("--method PATCH").expect("publishing call");
    assert!(compare < patch);
    assert!(publish.contains(r#"[ "$relation" != ahead ]"#));
}

/// Asserts that `id-token: write` is held by `attest` and `gamma` and by no other job.
#[test]
fn only_attest_and_gamma_hold_id_token() {
    let text = workflow();
    let holders: Vec<String> = jobs(&text)
        .into_iter()
        .filter(|(_, lines)| lines.iter().any(|l| l.trim() == "id-token: write"))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(holders, ["attest", "gamma"]);
}

/// Asserts the gamma step's shape: after `publish` and the tool's build, no more permission
/// than `id-token: write`, no checkout and no build (the job that holds the token runs the
/// built client, not the repository's code), the coordinator's address from a repository
/// variable, the version from the plan's output, and the services js-server then js-agent.
#[test]
fn gamma_is_off_by_variable_and_sets_the_server_before_the_agents() {
    let text = workflow();
    let jobs = jobs(&text);
    let gamma = job(&jobs, "gamma").join("\n");
    assert!(
        gamma.contains("needs: [plan, publish, deploy-tool]"),
        "{gamma}"
    );
    assert!(
        gamma.contains("    permissions:\n      id-token: write\n    steps:"),
        "{gamma}"
    );
    assert!(
        !gamma.contains("actions/checkout") && !gamma.contains("cargo "),
        "{gamma}"
    );
    assert!(gamma.contains("COORDINATOR_URL: ${{ vars.JS_COORDINATOR_URL }}"));
    assert!(gamma.contains("COORDINATOR_CA: ${{ vars.JS_COORDINATOR_CA }}"));
    assert!(gamma.contains("VERSION: ${{ needs.plan.outputs.version }}"));
    assert!(gamma.contains(r#"--version "$VERSION" --service js-server --service js-agent"#));
    assert!(gamma.contains("if: ${{ vars.TS_CLIENT_ID != '' && vars.JS_COORDINATOR_URL != '' }}"));
    // The build is in a job that holds nothing but a read of the repository.
    let tool = job(&jobs, "deploy-tool").join("\n");
    assert!(
        tool.contains("    permissions:\n      contents: read\n    steps:"),
        "{tool}"
    );
    assert!(tool.contains("cargo build -p xtask --locked --release"));
}

/// Asserts that the build matrix has the Apple-silicon leg (a `macos-14` runner building
/// `js` and `js-agent` only, the lease server being Linux-only) and that the archives are
/// made with GNU tar, which a Mac has as `gtar`: the archive options are GNU's, and the
/// reproducibility of the files depends on them.
#[test]
fn the_macos_leg_builds_the_client_and_agent_and_packs_with_gnu_tar() {
    let text = workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build").join("\n");
    assert!(
        build.contains(
            "- target: aarch64-apple-darwin\n            runner: macos-14\n            builds: js-cli:js js-agent:js-agent\n"
        ),
        "{build}"
    );
    assert!(
        build.contains("if command -v gtar > /dev/null; then\n            tar=gtar\n          fi")
    );
    assert!(build.contains(r#""$tar" -C "stage-$bin" --sort=name"#));
}
