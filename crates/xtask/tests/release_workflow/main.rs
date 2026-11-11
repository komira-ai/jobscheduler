//! The rules the release workflow keeps (the comments at the top of
//! `.github/workflows/release.yml` and of `.github/workflows/artifacts.yml`, which it calls to
//! build and sign the files), read from the files as text: actions pinned to commit SHAs, no
//! permission at the top, hosted runners, a push to main as the only trigger that releases, a
//! read-only wait for `ci`, `id-token: write` and `contents: write` only where they are meant,
//! every attestation signed by artifacts.yml, the apt jobs' permissions, and the gamma step's
//! shape. The shape of each apt job is held by `tests/apt_workflow`.

use std::path::Path;

mod apt;
mod artifacts;
mod debs;
mod images;

/// The text of `.github/workflows/<file>`.
fn read_workflow(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.github/workflows")
        .join(file);
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {file}: {e}"))
}

/// The release workflow's text.
fn workflow() -> String {
    read_workflow("release.yml")
}

/// The text of the workflow that builds and signs the files, which the release calls.
fn artifacts_workflow() -> String {
    read_workflow("artifacts.yml")
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

/// The one workflow a job of the release calls, as its `uses:` line writes it.
const CALLED: &str = "uses: ./.github/workflows/artifacts.yml";

/// The `uses:` value of a job that calls a workflow (`uses:` at the job's own level), if it is one.
fn calls(lines: &[String]) -> Option<&str> {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("    uses: "))
        .map(str::trim)
}

/// Asserts, in both files, that every action `uses:` names a full 40-digit commit SHA with its
/// version in a comment (the one local call, the release's `artifacts` job, excepted), that the
/// top level grants nothing, and that every job runs on a GitHub-hosted runner image of a
/// pinned version, or is that call.
#[test]
fn actions_are_pinned_and_the_top_grants_nothing() {
    for (file, text, least) in [
        ("release.yml", workflow(), 7),
        ("artifacts.yml", artifacts_workflow(), 9),
    ] {
        let mut uses = 0;
        for line in text.lines().filter(|l| {
            l.trim_start().starts_with("- uses:") || l.trim_start().starts_with("uses:")
        }) {
            if line.trim() == CALLED {
                assert_eq!(file, "release.yml", "{line}");
                continue;
            }
            uses += 1;
            let (_, rest) = line.split_once('@').unwrap_or_else(|| panic!("{line}"));
            let (sha, comment) = rest.split_once(' ').unwrap_or_else(|| panic!("{line}"));
            assert!(
                sha.len() == 40 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{line}"
            );
            assert!(comment.starts_with("# v"), "{line}");
        }
        assert!(uses >= least, "{file}: {uses} uses: lines");
        assert!(text.lines().any(|l| l == "permissions: {}"), "{file}");
        for (name, lines) in jobs(&text) {
            let runs_on: Vec<&String> = lines
                .iter()
                .filter(|l| l.trim_start().starts_with("runs-on:"))
                .collect();
            if let Some(called) = calls(&lines) {
                assert_eq!(
                    (file, name.as_str(), called),
                    (
                        "release.yml",
                        "artifacts",
                        "./.github/workflows/artifacts.yml"
                    )
                );
                assert!(runs_on.is_empty(), "{name}");
                continue;
            }
            assert_eq!(runs_on.len(), 1, "{file}: {name}");
            assert!(
                runs_on[0].contains("ubuntu-24.04") || runs_on[0].contains("matrix.runner"),
                "{file}: {name}: {}",
                runs_on[0]
            );
        }
    }
}

/// Asserts that a push to main is the only trigger that releases (the other is a pull
/// request, the dry run), that the run's commit is its own `github.sha`, and that every job
/// that publishes or deploys, and the wait and the plan, tests for a push to main of its own
/// condition; the call of artifacts.yml follows `plan`'s output (its jobs that attest test for
/// a push to main themselves: `artifacts::attest_and_verify_run_only_for_a_push_to_main`).
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
    // The dry run still builds: with the wait skipped, the call of artifacts.yml (its build and
    // sums) runs on the plan's word alone.
    let all = jobs(&text);
    let ifs: Vec<&String> = job(&all, "artifacts")
        .iter()
        .filter(|l| l.starts_with("    if:"))
        .collect();
    assert_eq!(ifs.len(), 1);
    assert_eq!(
        ifs[0].as_str(),
        "    if: ${{ !cancelled() && needs.plan.outputs.release == 'true' }}"
    );
    let push = "github.event_name == 'push' && github.ref == 'refs/heads/main'";
    let jobs = jobs(&text);
    let mut gated: Vec<String> = ["ci-passed", "plan", "publish", "deploy-tool", "gamma"]
        .map(str::to_owned)
        .to_vec();
    gated.extend(apt::apt_jobs(&text));
    for name in &gated {
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
/// `ci` run of this commit's push and of its jobs, `macos` excluded by name, and a failure
/// naming the run and the jobs for any other job that did not succeed or skip.
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
    // The step's environment and strict shell.
    assert!(wait.contains("        env:\n          GH_TOKEN: ${{ github.token }}\n          REPO: ${{ github.repository }}\n        run: |\n          set -euo pipefail\n          failed=0\n          while true; do\n"));
    // The newest run is picked by creation time, and an absent run reads as empty.
    assert!(wait.contains(r#"--jq '.workflow_runs | sort_by(.created_at) | last // empty | "\(.id) \(.status) \(.conclusion // "none") \(.html_url)"'"#));
    assert!(wait.contains(
        "      if [ -z \"$run\" ]; then\n              echo \"no ci run for $COMMIT yet\""
    ));
    // The jobs of that run's latest attempt are read from the jobs API, and the job named
    // `macos` (by name, and only it) is left out of the count; of the others, a finished
    // one counts as bad unless it succeeded or was skipped, and one not finished is pending.
    assert!(wait.contains("actions/runs/$id/jobs?per_page=100&filter=latest"));
    assert!(wait.contains(r#"[.jobs[] | select(.name != "macos")]"#));
    assert_eq!(wait.matches(r#""macos""#).count(), 1, "{wait}");
    assert!(wait.contains(r#"select(.status != "completed")"#));
    assert!(wait.contains(r#"select(.status == "completed" and .conclusion != "success" and .conclusion != "skipped") | "\(.name)=\(.conclusion)""#));
    // Any other job that failed, was cancelled or timed out fails the release at once,
    // naming the jobs and the run, before the pass is considered.
    assert!(wait.contains("              if [ -n \"$bad\" ]; then\n                echo \"::error::ci did not pass for $COMMIT: $bad, $url; nothing is released\"\n                exit 1\n              fi\n"));
    // All the other jobs finished and none bad passes (exit 0), even while `macos` is
    // queued or after it failed; the pass comes after the failure check.
    assert!(wait.contains("              if [ \"$total\" -gt 0 ] && [ \"$pending\" -eq 0 ]; then\n                echo \"ci passed (all jobs but macos): $url\"\n                exit 0\n              fi\n"));
    assert!(
        wait.find("if [ -n \"$bad\" ]").expect("bad check") < wait.find("exit 0").expect("pass"),
        "{wait}"
    );
    // A finished run with no job besides `macos` is an error, not a pass.
    assert!(wait.contains("              if [ \"$status\" = completed ]; then\n                echo \"::error::ci did not pass for $COMMIT: no job besides macos in the run, $url; nothing is released\"\n                exit 1\n              fi\n"));
    // Otherwise the wait goes on, two minutes at a time.
    assert!(wait.contains(
        "echo \"ci is $status: $url ($pending of $total jobs but macos still to finish)\"\n"
    ));
    assert!(wait.contains("            sleep 120\n          done"));
    // Ten failed reads in a row (of the runs or of the jobs) fail the job; a good read
    // starts the count again.
    assert!(wait.contains("failed=$((failed + 1))\n              if [ \"$failed\" -ge 10 ]; then\n                echo \"::error::reading the ci runs of $COMMIT failed $failed times in a row; nothing is released\"\n                exit 1\n              fi\n"));
    assert!(wait.contains(
        "then\n              failed=0\n            else\n              failed=$((failed + 1))\n"
    ));
    assert!(wait.contains("              run=\"\"\n            fi\n"));
    // Nothing is planned before the wait has passed.
    assert!(job(&jobs, "plan").join("\n").contains("needs: [ci-passed]"));
}

/// Asserts that publish, deploy-tool and gamma run only on the plan's word that a release is
/// made, and never with a status function that would run them after a failed or skipped job,
/// and that publish needs the call of artifacts.yml (so a failed attest or verify there stops
/// it).
#[test]
fn the_release_jobs_need_the_plans_release_and_no_status_function() {
    let text = workflow();
    let all = jobs(&text);
    assert!(
        job(&all, "publish")
            .iter()
            .any(|l| l == "    needs: [plan, artifacts]")
    );
    for name in ["publish", "deploy-tool", "gamma"] {
        let ifs: Vec<&String> = job(&all, name)
            .iter()
            .filter(|l| l.starts_with("    if:"))
            .collect();
        assert_eq!(ifs.len(), 1, "{name}");
        assert!(
            ifs[0].contains("needs.plan.outputs.release == 'true'"),
            "{name}: {}",
            ifs[0]
        );
        assert!(
            !ifs[0].contains("!cancelled()") && !ifs[0].contains("always()"),
            "{name}: {}",
            ifs[0]
        );
    }
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

/// The jobs of `text` that hold `permission` (a `name: write` line).
fn holders(text: &str, permission: &str) -> Vec<String> {
    jobs(text)
        .into_iter()
        .filter(|(_, lines)| lines.iter().any(|l| l.trim() == permission))
        .map(|(name, _)| name)
        .collect()
}

/// Asserts the gamma step's shape: after `publish` and the tool's build, no more permission
/// than `id-token: write`, no checkout and no build (the job that holds the token runs the
/// built client, not the repository's code), the coordinator's address from a repository
/// variable, the version from the plan's output, and the service js-server alone (js-agent's
/// desired version is hostkeeper's, set by hostkeeper-desired.yml).
#[test]
fn gamma_is_off_by_variable_and_sets_the_lease_server_alone() {
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
    assert!(gamma.contains(r#"--version "$VERSION" --service js-server"#));
    assert!(!gamma.contains("--service js-agent"), "{gamma}");
    assert_eq!(gamma.matches("--service ").count(), 1, "{gamma}");
    assert!(gamma.contains("if: ${{ vars.TS_CLIENT_ID != '' && vars.JS_COORDINATOR_URL != '' }}"));
    // The build is in a job that holds nothing but a read of the repository.
    let tool = job(&jobs, "deploy-tool").join("\n");
    assert!(
        tool.contains("    permissions:\n      contents: read\n    steps:"),
        "{tool}"
    );
    assert!(tool.contains("cargo build -p xtask --locked --release"));
}

/// Asserts that the build matrix has the Apple-silicon leg (a `macos-15` runner building
/// `js`, `js-agent`, `js-bundle` and the two Mac host tools only, the lease server being
/// Linux-only) and that the archives are made with GNU tar, which a Mac has as `gtar`: the archive options are GNU's, and the
/// reproducibility of the files depends on them.
#[test]
fn the_macos_leg_builds_the_client_agent_bundle_and_mac_tools_and_packs_with_gnu_tar() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build").join("\n");
    assert!(
        build.contains(
            "- target: aarch64-apple-darwin\n            runner: macos-15\n            builds: js-cli:js js-agent:js-agent js-bundle-cli:js-bundle js-mac-session:js-mac-session js-mac-provision:js-mac-provision\n"
        ),
        "{build}"
    );
    assert!(
        build.contains("if command -v gtar > /dev/null; then\n            tar=gtar\n          fi")
    );
    assert!(build.contains(r#""$tar" -C "stage-$bin" --sort=name"#));
}

/// Asserts that the files of every archive are laid down by `cargo xtask release stage`, the one
/// list of what an archive holds (the Linux js-lease-server archive carries the updater's
/// declaration), and not copied by the step itself, so an archive cannot drift from what the tests read.
#[test]
fn the_archives_are_staged_by_the_xtask_list_and_nothing_else() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build").join("\n");
    assert!(
        build.contains(
            r#"cargo xtask release stage --bin "$bin" --target "$TARGET" --version "$VERSION" --out "stage-$bin""#
        ),
        "{build}"
    );
    assert_eq!(build.matches("release stage").count(), 1, "{build}");
    for copy in ["cp \"target/", "mkdir \"stage-", "LICENSE \"stage-"] {
        assert!(!build.contains(copy), "{copy} in {build}");
    }
}

/// Asserts that the `sums` job expects the `js-bundle` archive of every target (the Mac and both
/// Linux architectures), so a build that stops making one fails before anything is attested,
/// and that all three matrix legs build it. The two Mac host tools are expected for darwin only.
#[test]
fn the_js_bundle_archive_is_built_and_expected_for_both_targets() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let sums = job(&jobs, "sums").join("\n");
    for target in [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ] {
        let asset = format!("js-bundle-$VERSION-{target}.tar.gz");
        assert!(sums.contains(&asset), "{asset} is not expected: {sums}");
    }
    for tool in ["js-mac-provision", "js-mac-session"] {
        let asset = format!("{tool}-$VERSION-aarch64-apple-darwin.tar.gz");
        assert!(sums.contains(&asset), "{asset} is not expected: {sums}");
    }
    let build = job(&jobs, "build").join("\n");
    assert_eq!(
        build.matches("js-bundle-cli:js-bundle").count(),
        3,
        "{build}"
    );
}

/// Asserts that `js-mdm` is built for the two Linux targets only, by the same `build` entry as
/// `js-agent` (package and binary named `js-mdm`), and that the `sums` job expects exactly its
/// two Linux archives, so a build that stops making one fails before anything is attested.
/// There is no darwin archive.
#[test]
fn the_js_mdm_archive_is_built_and_expected_for_linux_only() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build").join("\n");
    let linux: Vec<&str> = build
        .lines()
        .filter(|line| {
            line.trim_start().starts_with("builds: ") && line.contains("js-lease-server")
        })
        .collect();
    assert_eq!(linux.len(), 2, "{build}");
    for line in linux {
        assert!(line.contains(" js-mdm:js-mdm"), "{line}");
    }
    assert_eq!(build.matches("js-mdm:js-mdm").count(), 2, "{build}");
    let sums = job(&jobs, "sums").join("\n");
    for target in ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"] {
        let asset = format!("js-mdm-$VERSION-{target}.tar.gz");
        assert!(sums.contains(&asset), "{asset} is not expected: {sums}");
    }
    assert_eq!(sums.matches("js-mdm-$VERSION").count(), 2, "{sums}");
    assert!(
        !sums.contains("js-mdm-$VERSION-aarch64-apple-darwin"),
        "{sums}"
    );
}

/// Asserts that the Apple-silicon build pins the deployment target to 11.0 and that the
/// check step fails a binary whose minimum macOS is above it, so a newer runner image's
/// SDK cannot silently raise what the binaries need.
#[test]
fn the_macos_leg_pins_the_deployment_target_and_checks_each_binary() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build").join("\n");
    assert!(
        build.contains("*-apple-darwin) export MACOSX_DEPLOYMENT_TARGET=11.0 ;;"),
        "{build}"
    );
    assert!(
        build.contains(r#"otool -l "target/$TARGET/release/$bin""#),
        "{build}"
    );
    assert!(build.contains("above the pinned 11.0"), "{build}");
    assert!(
        build.contains(r#"if [ -z "$minos" ]; then"#),
        "a binary with no minos must fail the check, not pass it: {build}"
    );
}

/// One leg of the `build` matrix: its target, its runner and its `package:binary` entries.
#[derive(Debug, PartialEq, Eq)]
struct Leg {
    target: String,
    runner: String,
    builds: Vec<String>,
    /// The platform of the .deb the leg makes (`linux-x86_64`), empty for the Mac leg.
    deb: String,
    /// The architecture in that .deb's file name (`amd64`), empty for the Mac leg.
    deb_arch: String,
}

/// The legs of the `build` job's matrix, in their order, read from its `- target:`, `runner:`,
/// `builds:`, `deb:` and `deb_arch:` lines.
fn legs(build: &[String]) -> Vec<Leg> {
    let mut legs: Vec<Leg> = Vec::new();
    for line in build {
        let line = line.trim_start();
        if let Some(target) = line.strip_prefix("- target: ") {
            legs.push(Leg {
                target: target.to_owned(),
                runner: String::new(),
                builds: Vec::new(),
                deb: String::new(),
                deb_arch: String::new(),
            });
        } else if let Some(leg) = legs.last_mut() {
            if let Some(runner) = line.strip_prefix("runner: ") {
                runner.clone_into(&mut leg.runner);
            } else if let Some(builds) = line.strip_prefix("builds: ") {
                leg.builds = builds.split_whitespace().map(str::to_owned).collect();
            } else if let Some(deb) = line.strip_prefix("deb: ") {
                deb.trim_matches('\'').clone_into(&mut leg.deb);
            } else if let Some(arch) = line.strip_prefix("deb_arch: ") {
                arch.trim_matches('\'').clone_into(&mut leg.deb_arch);
            }
        }
    }
    legs
}

/// The lines of the step named `name` in a job: from its `- name:` line to the next step.
fn step<'a>(job: &'a [String], name: &str) -> &'a [String] {
    let start = job
        .iter()
        .position(|l| l.trim_start() == format!("- name: {name}"))
        .unwrap_or_else(|| panic!("no step {name}"));
    let indent = job[start].len() - job[start].trim_start().len();
    let end = job[start + 1..]
        .iter()
        .position(|l| {
            let rest = l.trim_start();
            l.len() - rest.len() == indent && rest.starts_with("- ")
        })
        .map_or(job.len(), |n| start + 1 + n);
    &job[start..end]
}

/// The five Linux binaries, as `package:binary`, in the order the legs list them.
const LINUX_BUILDS: [&str; 5] = [
    "js-lease-server:js-lease-server",
    "js-cli:js",
    "js-agent:js-agent",
    "js-bundle-cli:js-bundle",
    "js-mdm:js-mdm",
];

/// Asserts that the `build` matrix has an arm64 Linux leg, `aarch64-unknown-linux-musl` on the
/// GitHub-hosted arm64 runner `ubuntu-24.04-arm` (a native build: the job runs on
/// `matrix.runner`), that builds entry for entry what the x86_64 leg builds, the five Linux
/// binaries. Catches a missing leg, a leg on another runner (an x86_64 one would cross-compile
/// and could not run the binaries) and a binary dropped from it.
#[test]
fn the_linux_arm64_leg_builds_the_five_linux_binaries_on_a_hosted_arm_runner() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build");
    assert!(
        build
            .iter()
            .any(|l| l.trim() == "runs-on: ${{ matrix.runner }}"),
        "{build:?}"
    );
    let legs = legs(build);
    let arm: Vec<&Leg> = legs
        .iter()
        .filter(|leg| leg.target == "aarch64-unknown-linux-musl")
        .collect();
    assert_eq!(arm.len(), 1, "{legs:?}");
    assert_eq!(arm[0].runner, "ubuntu-24.04-arm", "{legs:?}");
    let x86 = legs
        .iter()
        .find(|leg| leg.target == "x86_64-unknown-linux-musl")
        .expect("the x86_64 leg");
    assert_eq!(arm[0].builds, x86.builds);
    assert_eq!(arm[0].builds, LINUX_BUILDS);
    assert_eq!(
        legs.iter()
            .map(|leg| leg.target.as_str())
            .collect::<Vec<_>>(),
        [
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "aarch64-apple-darwin"
        ]
    );
}

/// The archive names `sums` expects, `$VERSION` as the workflow spells it.
fn expected_assets(sums: &[String]) -> Vec<String> {
    let line = sums
        .iter()
        .find_map(|l| l.trim_start().strip_prefix("expected=\""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("the expected= line");
    line.split_whitespace().map(str::to_owned).collect()
}

/// Asserts that `sums` expects exactly the fifteen archives (each of `js-lease-server`, `js`,
/// `js-agent`, `js-bundle` and `js-mdm` for both Linux targets, `js`, `js-agent` and `js-bundle`
/// and the two Mac host tools for darwin, which is also what the matrix builds), the two .debs
/// of js-agent (`js-agent_<version>_amd64.deb` and `js-agent_<version>_arm64.deb`) and the two
/// images of js-lease-server (`js-lease-server-<version>-<target>.oci.tar`); and that the
/// list is in `LC_ALL=C` order once `$VERSION` is a version, as `ls | LC_ALL=C sort` prints the
/// files. Catches a missing or extra name (a build that stops making an asset must fail here,
/// before attest) and an unsorted list, which would never equal the listing and would fail every
/// real release.
#[test]
fn the_sums_job_expects_every_linux_binary_for_both_architectures_and_the_list_is_sorted() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let expected = expected_assets(job(&jobs, "sums"));
    let mut wanted: Vec<String> = Vec::new();
    for target in ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"] {
        for bin in ["js-lease-server", "js", "js-agent", "js-bundle", "js-mdm"] {
            wanted.push(format!("{bin}-$VERSION-{target}.tar.gz"));
        }
    }
    for bin in [
        "js",
        "js-agent",
        "js-bundle",
        "js-mac-session",
        "js-mac-provision",
    ] {
        wanted.push(format!("{bin}-$VERSION-aarch64-apple-darwin.tar.gz"));
    }
    let archives = wanted.clone();
    // The two .debs, whose names the shell spells with braces (`$VERSION_amd64` would be another
    // variable).
    for arch in ["amd64", "arm64"] {
        wanted.push(format!("js-agent_${{VERSION}}_{arch}.deb"));
    }
    // The two images of js-lease-server, one per Linux target.
    for target in ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"] {
        wanted.push(format!("js-lease-server-$VERSION-{target}.oci.tar"));
    }
    assert_eq!(expected.len(), 19, "{expected:?}");
    let mut have = expected.clone();
    have.sort();
    wanted.sort();
    assert_eq!(have, wanted);
    let mut wanted = archives;
    wanted.sort();
    // What the matrix builds is what `sums` expects.
    let mut built: Vec<String> = legs(job(&jobs, "build"))
        .iter()
        .flat_map(|leg| {
            leg.builds.iter().map(|entry| {
                let (_, bin) = entry.split_once(':').expect("package:binary");
                format!("{bin}-$VERSION-{}.tar.gz", leg.target)
            })
        })
        .collect();
    built.sort();
    assert_eq!(built, wanted);
    // `ls | LC_ALL=C sort` orders the files by their bytes, with the version in the names.
    for version in ["0.1.0", "1.2.3", "10.20.30"] {
        let listed: Vec<String> = expected
            .iter()
            .map(|name| {
                name.replace("${VERSION}", version)
                    .replace("$VERSION", version)
            })
            .collect();
        let mut sorted = listed.clone();
        sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        assert_eq!(listed, sorted, "version {version}");
    }
}

/// The value of a GitHub Actions condition made of `||`-joined `matrix.target == '<t>'` and
/// `endsWith(matrix.target, '<suffix>')` terms, for `target`. Any other form is refused, so the
/// test cannot pass on a condition it does not understand.
fn target_condition(condition: &str, target: &str) -> bool {
    let inner = condition
        .strip_prefix("${{ ")
        .and_then(|c| c.strip_suffix(" }}"))
        .unwrap_or_else(|| panic!("not an expression: {condition}"));
    // Each term is checked whole before anything is evaluated: a literal is a plain target word
    // (no quote, space or operator), so `&&`, `!`, a nested call or a second comparison hidden
    // inside what looks like one term is refused, not read as part of a literal.
    let literal = |text: &str| -> String {
        assert!(
            !text.is_empty()
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
            "a literal this test does not read: {text:?} in {condition}"
        );
        text.to_owned()
    };
    let terms: Vec<(bool, String)> = inner
        .split(" || ")
        .map(|term| {
            if let Some(value) = term
                .strip_prefix("matrix.target == '")
                .and_then(|t| t.strip_suffix('\''))
            {
                (true, literal(value))
            } else if let Some(suffix) = term
                .strip_prefix("endsWith(matrix.target, '")
                .and_then(|t| t.strip_suffix("')"))
            {
                (false, literal(suffix))
            } else {
                panic!("a term this test does not read: {term}")
            }
        })
        .collect();
    terms.iter().any(|(equal, value)| {
        if *equal {
            target == value
        } else {
            target.ends_with(value.as_str())
        }
    })
}

/// The reader above, on the forms it takes and the one it refuses.
#[test]
fn the_condition_reader_reads_equality_suffix_and_or() {
    let cond = "${{ matrix.target == 'a-b' || endsWith(matrix.target, '-c') }}";
    assert!(target_condition(cond, "a-b"));
    assert!(target_condition(cond, "x-c"));
    assert!(!target_condition(cond, "a-bc"));
    assert!(!target_condition(cond, "x-d"));
    // Forms it does not read are refused, whatever the target, so a condition GitHub reads
    // differently cannot pass: another function, an `&&` hidden between the quotes of one
    // equality (GitHub reads this one as true on the Mac too), a negation, a stray quote, an
    // empty literal.
    for refused in [
        "${{ contains(matrix.target, 'x') }}",
        "${{ matrix.target == 'aarch64-apple-darwin' && matrix.target == 'aarch64-apple-darwin' || endsWith(matrix.target, '-linux-musl') }}",
        "${{ !endsWith(matrix.target, '-linux-musl') }}",
        "${{ matrix.target == 'a'b' }}",
        "${{ endsWith(matrix.target, '') }}",
        "matrix.target == 'x'",
    ] {
        for target in ["aarch64-apple-darwin", "x86_64-unknown-linux-musl"] {
            let read = std::panic::catch_unwind(|| target_condition(refused, target));
            assert!(read.is_err(), "{refused} was read for {target}");
        }
    }
}

/// Asserts that the "Install musl tools" step installs the pinned `musl-tools` packages the
/// steps before it restored or fetched, and that its condition and theirs are true for both
/// musl targets and false for the Mac target, evaluated for each, and that on the
/// arm64 leg only a link gives that package's `musl-gcc` the name the cc crate looks for there,
/// `aarch64-linux-musl-gcc`, with no compiler set through the environment. Catches an arm64 leg
/// that builds without `musl-tools` or without a C compiler the cc crate finds (ring needs one
/// for musl), and a Mac leg that would run `apt-get`.
#[test]
fn the_musl_tools_are_installed_for_both_musl_legs() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build");
    let install = step(build, "Install musl tools");
    assert!(
        install
            .iter()
            .any(|l| l.trim() == "run: sudo dpkg --install \"$RUNNER_TEMP\"/debs/musl-tools/*.deb"),
        "{install:?}"
    );
    for name in [
        "Restore the musl-tools packages",
        "Fetch the pinned musl-tools packages",
        "Install musl tools",
    ] {
        let condition = step(build, name)
            .iter()
            .find_map(|l| l.trim_start().strip_prefix("if: "))
            .expect("the step's condition");
        for (target, installs) in [
            ("x86_64-unknown-linux-musl", true),
            ("aarch64-unknown-linux-musl", true),
            ("aarch64-apple-darwin", false),
        ] {
            assert_eq!(
                target_condition(condition, target),
                installs,
                "{name}, {target}: {condition}"
            );
        }
    }
    let link = step(build, "Name musl-gcc as the arm64 musl compiler");
    let link_condition = link
        .iter()
        .find_map(|l| l.trim_start().strip_prefix("if: "))
        .expect("the link step's condition");
    for (target, links) in [
        ("x86_64-unknown-linux-musl", false),
        ("aarch64-unknown-linux-musl", true),
        ("aarch64-apple-darwin", false),
    ] {
        assert_eq!(
            target_condition(link_condition, target),
            links,
            "{target}: {link_condition}"
        );
    }
    assert!(
        link.iter().any(|l| l.trim()
            == "run: sudo ln -s /usr/bin/musl-gcc /usr/local/bin/aarch64-linux-musl-gcc"),
        "{link:?}"
    );
    // The link comes after the package that installs musl-gcc, and nothing sets a compiler
    // through the environment.
    let names: Vec<&str> = build
        .iter()
        .filter_map(|l| l.trim_start().strip_prefix("- name: "))
        .collect();
    let at = |name: &str| names.iter().position(|n| *n == name).expect(name);
    assert!(at("Install musl tools") < at("Name musl-gcc as the arm64 musl compiler"));
    assert!(
        !text.contains("CC_"),
        "a compiler set through the environment"
    );
    // Every leg is one of those three.
    for leg in legs(build) {
        assert!(
            [
                "x86_64-unknown-linux-musl",
                "aarch64-unknown-linux-musl",
                "aarch64-apple-darwin"
            ]
            .contains(&leg.target.as_str()),
            "{leg:?}"
        );
    }
}

/// Whether the shell `case` pattern `pattern` (only `*` and literal text) matches `word`.
fn glob(pattern: &str, word: &str) -> bool {
    assert!(
        !pattern.contains(['?', '[', '|', '"', '\'']),
        "a pattern this test does not read: {pattern}"
    );
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if parts.len() == 1 {
        return word == pattern;
    }
    if !word.starts_with(first) || word.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &word[first.len()..word.len() - last.len()];
    if !word.ends_with(last) {
        return false;
    }
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

/// The pattern matcher above.
#[test]
fn the_glob_reader_matches_stars_and_literals() {
    assert!(glob("*-linux-musl", "aarch64-unknown-linux-musl"));
    assert!(glob("*-linux-musl", "x86_64-unknown-linux-musl"));
    assert!(!glob("*-linux-musl", "aarch64-apple-darwin"));
    assert!(!glob("x86_64-*-linux-musl", "aarch64-unknown-linux-musl"));
    assert!(glob("x86_64-*-linux-musl", "x86_64-unknown-linux-musl"));
    assert!(glob("a*b*c", "aXbYc"));
    assert!(!glob("a*b*c", "aXcYb"));
    assert!(!glob("ab*ba", "aba"));
    assert!(glob("exact", "exact"));
    assert!(!glob("exact", "exactly"));
}

/// The arms of the first `case "$TARGET" in` of `lines`: each pattern with the lines of its body.
fn target_case(lines: &[String]) -> Vec<(String, Vec<String>)> {
    let start = lines
        .iter()
        .position(|l| l.trim() == r#"case "$TARGET" in"#)
        .expect("a case on $TARGET");
    let indent = lines[start].len() - lines[start].trim_start().len();
    let mut arms: Vec<(String, Vec<String>)> = Vec::new();
    for line in &lines[start + 1..] {
        let rest = line.trim_start();
        let at = line.len() - rest.len();
        if at == indent && rest == "esac" {
            return arms;
        }
        if at == indent + 2 && rest.ends_with(')') {
            arms.push((rest.trim_end_matches(')').to_owned(), Vec::new()));
        } else if let Some((_, body)) = arms.last_mut() {
            body.push(rest.to_owned());
        }
    }
    panic!("no esac")
}

/// Asserts that in "Check the binaries" the `case` arm that `aarch64-unknown-linux-musl` falls in
/// is the static check, the same arm as the x86_64 Linux target's: every binary of `$BUILDS` is
/// read with `readelf -d` and one with a `NEEDED` entry fails the job. Catches a pattern
/// narrowed to the x86_64 name, which would let a dynamically linked arm64 binary through.
#[test]
fn the_static_check_covers_the_arm64_linux_leg() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let check = step(job(&jobs, "build"), "Check the binaries");
    let arms = target_case(check);
    let matching = |target: &str| -> Vec<&(String, Vec<String>)> {
        arms.iter()
            .filter(|(pattern, _)| glob(pattern, target))
            .collect()
    };
    let arm = matching("aarch64-unknown-linux-musl");
    assert_eq!(arm.len(), 1, "{arms:?}");
    assert_eq!(arm, matching("x86_64-unknown-linux-musl"));
    let body = arm[0].1.join("\n");
    assert!(body.contains("for entry in $BUILDS; do"), "{body}");
    assert!(
        body.contains(r#"if readelf -d "target/$TARGET/release/$bin" | grep -q NEEDED; then"#),
        "{body}"
    );
    assert!(
        body.contains("echo \"::error::$bin links shared libraries\"\nexit 1"),
        "{body}"
    );
    // The Mac target takes the other arm, not this one.
    let mac = matching("aarch64-apple-darwin");
    assert_eq!(mac.len(), 1, "{arms:?}");
    assert_ne!(mac, arm);
    // And the version check after the case runs every binary on every leg.
    let all = check.join("\n");
    let esac = all.find("esac").expect("esac");
    assert!(
        all[esac..].contains(r#"printed="$("target/$TARGET/release/$bin" --version)""#),
        "{all}"
    );
}

/// The GitHub-hosted runner labels a job may run on (no self-hosted, no larger or paid runner).
const HOSTED_RUNNERS: [&str; 3] = ["ubuntu-24.04", "ubuntu-24.04-arm", "macos-15"];

/// Asserts that every job's `runs-on:` is one of the hosted labels above, or `matrix.runner`
/// with every leg's `runner:` one of them, and that no `self-hosted` label appears anywhere.
/// Catches a self-hosted or larger-runner label, in a job or in a matrix leg.
#[test]
fn no_self_hosted_or_paid_runner_labels() {
    for text in [workflow(), artifacts_workflow()] {
        no_self_hosted_or_paid_runner_label_in(&text);
    }
}

/// The check above, on one workflow's text; a job that calls a workflow has no runner of its own.
fn no_self_hosted_or_paid_runner_label_in(text: &str) {
    // `gh attestation verify --deny-self-hosted-runners` refuses such a runner; it is no label.
    assert!(
        !text
            .replace("--deny-self-hosted-runners", "")
            .contains("self-hosted")
    );
    for (name, lines) in jobs(text) {
        if calls(&lines).is_some() {
            continue;
        }
        let runs_on = lines
            .iter()
            .find_map(|l| l.trim_start().strip_prefix("runs-on: "))
            .unwrap_or_else(|| panic!("{name} has no runs-on"));
        if runs_on == "${{ matrix.runner }}" {
            let runners: Vec<&str> = lines
                .iter()
                .filter_map(|l| l.trim_start().strip_prefix("runner: "))
                .collect();
            assert!(!runners.is_empty(), "{name}");
            for runner in runners {
                assert!(HOSTED_RUNNERS.contains(&runner), "{name}: {runner}");
            }
        } else {
            assert!(HOSTED_RUNNERS.contains(&runs_on), "{name}: {runs_on}");
        }
    }
}
