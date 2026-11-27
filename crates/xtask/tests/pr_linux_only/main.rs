//! The workflow lint of what a pull request runs: Linux x86_64 and nothing else, over every
//! workflow in `.github/workflows`, read as text.
//!
//! The owner's rule: x86_64 Linux tests are the pull request checks, because there are so many
//! of those hosts; every other platform (macOS, Linux arm64) is the beta stage of the release
//! pipeline (`release.yml` on a push to main, which calls `beta.yml` and the other workflows
//! the stage runs). So no job a pull request starts may name another runner. On a push to main
//! the stage holds back the macOS archives (`promote-macos` needs all four jobs), and the deploy
//! path through `blackbox` alone, the gate before the farm, which needs the first install
//! (`beta-macos-first-install`) among its black-box tests; besides, only `pkg-upload` (the
//! package job, passed) and the two reports (`release-red`, `release-green`) need a beta job.
//!
//! A job is one a pull request starts when
//!   - its workflow starts on `pull_request`, `pull_request_target` or `merge_group` (the merge
//!     queue's check of a queued pull request, docs/ci.md), or it is a job of a workflow that a
//!     job of such a workflow calls (`uses: ./.github/workflows/<file>`);
//!   - and its `if:` does not pin another event: no `&&`-joined part of it, outside
//!     parentheses, is `github.event_name == '<another event>'`, or
//!     `github.event_name != 'pull_request'` in a workflow that does not start on `merge_group`.
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
    for job in [
        "ci.yml/lint",
        "ci.yml/test",
        "ci.yml/coverage",
        "ci.yml/all-passed",
        "farm.yml/build",
    ] {
        assert!(jobs.iter().any(|j| j == job), "{job} in {jobs:?}");
    }
    // None of the beta stage's jobs is one.
    for job in &jobs {
        assert!(
            !job.starts_with("beta.yml/")
                && !job.starts_with("macos-probe.yml/")
                && !job.starts_with("js-agent-macos-pkg.yml/")
                && !job.starts_with("js-agent-macos-first-install.yml/")
                && !job.starts_with("js-agent-native-isolation.yml/")
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
fn the_merge_queue_check_moved_to_macos_is_refused() {
    // ci.yml's aggregate, the one check of a merge group, on macOS.
    let workflows = changed(
        "ci.yml",
        "    runs-on: ubuntu-latest\n    timeout-minutes: 5\n",
        "    runs-on: macos-15\n    timeout-minutes: 5\n",
    );
    assert_eq!(
        lint(&workflows).1,
        ["ci.yml/all-passed: runs on macos-15, not Linux x86_64 (ubuntu-*)"]
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

/// The first-install call without its push condition would start a macOS job on a pull
/// request's dry run of release.yml, and the lint names the job in the called workflow.
#[test]
fn the_first_install_job_without_its_push_condition_is_refused() {
    let gate = "  beta-macos-first-install:\n    needs: [plan, beta-macos-pkg]\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}\n";
    let ungated = "  beta-macos-first-install:\n    needs: [plan, beta-macos-pkg]\n";
    let (jobs, problems) = lint(&changed("release.yml", gate, ungated));
    let job =
        "release.yml/beta-macos-first-install -> js-agent-macos-first-install.yml/first-install";
    assert!(jobs.iter().any(|j| j == job), "{jobs:?}");
    assert_eq!(
        problems,
        [format!(
            "{job}: runs on macos-15, not Linux x86_64 (ubuntu-*)"
        )]
    );
}

/// The isolation test's call without its push condition would start a macOS job on a pull
/// request's dry run of release.yml, and the lint names the job in the called workflow.
#[test]
fn the_isolation_job_without_its_push_condition_is_refused() {
    let gate = "  beta-macos-isolation:\n    needs: [plan, artifacts]\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}\n";
    let ungated = "  beta-macos-isolation:\n    needs: [plan, artifacts]\n";
    let (jobs, problems) = lint(&changed("release.yml", gate, ungated));
    let job = "release.yml/beta-macos-isolation -> js-agent-native-isolation.yml/isolation";
    assert!(jobs.iter().any(|j| j == job), "{jobs:?}");
    assert_eq!(
        problems,
        [format!(
            "{job}: runs on macos-15, not Linux x86_64 (ubuntu-*)"
        )]
    );
}

/// The first-install workflow with a pull request trigger is reported as a macOS job.
#[test]
fn the_first_install_workflow_started_by_a_pull_request_is_refused() {
    let workflows = changed(
        "js-agent-macos-first-install.yml",
        "on:\n  workflow_call:\n",
        "on:\n  pull_request:\n  workflow_call:\n",
    );
    let (_, problems) = lint(&workflows);
    assert_eq!(
        problems,
        [
            "js-agent-macos-first-install.yml/first-install: runs on macos-15, not Linux x86_64 (ubuntu-*)"
        ]
    );
}

#[test]
fn the_release_dry_run_builds_nothing_on_arm_or_macos() {
    // artifacts.yml's build matrix names an arm64 runner, its `build-macos` job a macOS one and
    // its `macos-farm` call a workflow the lint does not follow; only the `if:` of the
    // `artifacts` call keeps a pull request's dry run off them.
    let call = "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}\n";
    let ungated = "    if: ${{ !cancelled() && needs.plan.outputs.release == 'true' }}\n";
    let (_, problems) = lint(&changed("release.yml", call, ungated));
    assert_eq!(
        problems,
        [
            "release.yml/artifacts -> artifacts.yml/build: runs on ubuntu-24.04-arm, not Linux x86_64 (ubuntu-*)",
            "release.yml/artifacts -> artifacts.yml/build-macos: runs on macos-15, not Linux x86_64 (ubuntu-*)",
            "release.yml/artifacts -> artifacts.yml/macos-farm: calls a workflow again, which the lint does not follow"
        ]
    );
}

#[test]
fn the_beta_stage_workflows_are_called_not_started_by_a_pull_request() {
    for file in [
        "beta.yml",
        "js-agent-macos-pkg.yml",
        "js-agent-macos-first-install.yml",
        "js-agent-native-isolation.yml",
        "macos-probe.yml",
    ] {
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

/// The jobs of release.yml that need a beta job but are not one of the six that may (the
/// first install is a beta job that needs `beta-macos-pkg`), and the jobs of the deploy path that
/// need one; that `blackbox`, the gate before the farm through which the deploy path waits for
/// the beta stage, needs `beta-macos-first-install` and no other beta job; what `pkg-upload`
/// needs, and that it runs only when its package job passed.
fn beta_need_problems(text: &str) -> Vec<String> {
    let w = Workflow::parse("release.yml", text);
    let mut problems = Vec::new();
    let mut waiting = Vec::new();
    for (name, lines) in &w.jobs {
        let needs = block(lines, 4, "needs").unwrap_or_default().join(" ");
        if needs.contains("beta-macos") {
            waiting.push(name.as_str());
        }
        let deploy_path = [
            "turn",
            "publish",
            "gamma",
            "cp-deploy-digest",
            "cp-deploy",
            "desired",
            "deployed",
        ]
        .contains(&name.as_str())
            || name.contains("-apt-")
            || name == "check-apt";
        if deploy_path && needs.contains("beta-macos") {
            problems.push(format!(
                "{name} (the deploy path) needs a beta job: {needs}"
            ));
        }
    }
    if waiting
        != [
            "beta-macos-first-install",
            "promote-macos",
            "pkg-upload",
            "blackbox",
            "release-red",
            "release-green",
        ]
    {
        problems.push(format!("jobs that need a beta job: {waiting:?}"));
    }
    let find = |name: &str| w.jobs.iter().find(|(n, _)| n == name).map(|(_, l)| l);
    let upload = find("pkg-upload").expect("pkg-upload");
    let upload_if = block(upload, 4, "if").unwrap_or_default().join(" ");
    if block(upload, 4, "needs").unwrap_or_default()
        != ["[plan, deploy-tool, publish, beta-macos-pkg]"]
        || !upload_if.contains("needs.beta-macos-pkg.result == 'success'")
    {
        problems.push(format!("pkg-upload: {upload_if}"));
    }
    let gate = find("blackbox").expect("blackbox");
    let gate_needs = block(gate, 4, "needs").unwrap_or_default().join(" ");
    let beta: Vec<&str> = gate_needs
        .trim_matches(['[', ']'])
        .split(", ")
        .filter(|n| n.starts_with("beta-macos"))
        .collect();
    if beta != ["beta-macos-first-install"] {
        problems.push(format!("blackbox needs the beta jobs {beta:?}"));
    }
    problems
}

#[test]
fn the_beta_stage_runs_after_the_build_and_holds_back_only_the_macos_archives() {
    // The jobs that need a beta job are `promote-macos`, `pkg-upload`, `blackbox` and the two
    // reports; the deploy path needs none but through `blackbox`.
    let text = read("release.yml");
    let w = Workflow::parse("release.yml", &text);
    let job = |name: &str| &w.jobs.iter().find(|(n, _)| n == name).expect(name).1;
    for (name, file) in [
        ("beta-macos", "beta.yml"),
        ("beta-macos-pkg", "js-agent-macos-pkg.yml"),
        (
            "beta-macos-first-install",
            "js-agent-macos-first-install.yml",
        ),
        ("beta-macos-probe", "macos-probe.yml"),
        ("beta-macos-isolation", "js-agent-native-isolation.yml"),
    ] {
        let lines = job(name);
        // The first install installs the package `beta-macos-pkg` built, so it waits for it.
        let needs = if name == "beta-macos-first-install" {
            "[plan, beta-macos-pkg]"
        } else {
            "[plan, artifacts]"
        };
        assert_eq!(block(lines, 4, "needs").expect("needs"), [needs], "{name}");
        assert_eq!(
            block(lines, 4, "uses").expect("uses"),
            [format!("./.github/workflows/{file}")],
            "{name}"
        );
        // `beta-macos-pkg` also grants the token, which its called workflow's call of
        // macos-farm.yml holds (a called job can hold no more than its call grants).
        let permissions: &[&str] = if name == "beta-macos-pkg" {
            &["contents: read", "id-token: write"]
        } else {
            &["contents: read"]
        };
        assert_eq!(
            block(lines, 4, "permissions").expect("permissions"),
            permissions,
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
    assert_eq!(beta_need_problems(&text), Vec::<String>::new());
    let promote = job("promote-macos");
    assert_eq!(
        block(promote, 4, "needs").expect("needs"),
        ["[plan, publish, beta-macos, beta-macos-pkg, beta-macos-first-install, beta-macos-probe]"]
    );
    // `publish` leaves the macOS archives to it.
    assert!(text.contains(
        "            case \"$name\" in\n              *-aarch64-apple-darwin.tar.gz) continue ;;\n            esac\n"
    ));
}

#[test]
fn the_package_call_passes_the_release_version_and_asks_for_the_signature() {
    let w = Workflow::parse("release.yml", &read("release.yml"));
    let lines = &w
        .jobs
        .iter()
        .find(|(n, _)| n == "beta-macos-pkg")
        .expect("job")
        .1;
    assert_eq!(
        block(lines, 4, "with").expect("with"),
        ["version: ${{ needs.plan.outputs.version }}", "sign: true"]
    );
    // The called workflow takes the two inputs and starts on no pull request.
    let called = Workflow::parse("js-agent-macos-pkg.yml", &read("js-agent-macos-pkg.yml"));
    let on = called.top_block("on").expect("on");
    assert!(on.iter().any(|l| l == "workflow_call:"));
    assert!(on.iter().any(|l| l == "sign:"));
    assert!(!called.on_pull_request());
}

#[test]
fn the_package_upload_waits_for_the_package_job_to_pass_and_runs_for_a_push_alone() {
    let text = read("release.yml");
    let w = Workflow::parse("release.yml", &text);
    let upload = &w
        .jobs
        .iter()
        .find(|(n, _)| n == "pkg-upload")
        .expect("job")
        .1;
    // It waits for `publish` and for the package job to pass, and for no test of the beta stage.
    assert_eq!(
        block(upload, 4, "needs").expect("needs"),
        ["[plan, deploy-tool, publish, beta-macos-pkg]"]
    );
    let condition = block(upload, 4, "if").expect("if").join(" ");
    for passed in ["publish", "beta-macos-pkg"] {
        assert!(
            condition.contains(&format!("needs.{passed}.result == 'success'")),
            "{condition}"
        );
    }
    assert_eq!(
        block(upload, 4, "runs-on").expect("runs-on"),
        ["ubuntu-24.04"]
    );
    assert!(
        block(upload, 4, "if")
            .expect("if")
            .join(" ")
            .contains("github.event_name == 'push' && github.ref == 'refs/heads/main'")
    );
    // Not a job a pull request starts.
    let (jobs, problems) = lint(&repository_workflows());
    assert_eq!(problems, Vec::<String>::new());
    assert!(!jobs.iter().any(|j| j.contains("pkg-upload")), "{jobs:?}");
}

#[test]
fn a_package_upload_without_its_push_condition_is_a_pull_request_job() {
    // The lint lists the job once its `if:` no longer keeps a pull request's dry run off it.
    let gate = "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' && needs.publish.result == 'success' && needs.deploy-tool.result == 'success' && needs.beta-macos-pkg.result == 'success' }}\n";
    let ungated = "";
    let (jobs, _) = lint(&changed("release.yml", gate, ungated));
    assert!(
        jobs.iter().any(|j| j == "release.yml/pkg-upload"),
        "{jobs:?}"
    );
}

#[test]
fn the_package_workflow_started_by_a_pull_request_is_refused() {
    let workflows = changed(
        "js-agent-macos-pkg.yml",
        "on:\n  workflow_call:\n",
        "on:\n  pull_request:\n  workflow_call:\n",
    );
    let (_, problems) = lint(&workflows);
    for job in ["build", "build-update", "sign"] {
        let want =
            format!("js-agent-macos-pkg.yml/{job}: runs on macos-15, not Linux x86_64 (ubuntu-*)");
        assert!(problems.contains(&want), "{problems:?}");
    }
}

#[test]
fn a_deploy_job_that_needs_a_beta_job_is_refused() {
    let text = read("release.yml");
    for (from, to) in [
        (
            "    needs: [plan, artifacts, unit-tests, turn]\n",
            "    needs: [plan, artifacts, unit-tests, turn, beta-macos]\n",
        ),
        (
            "    needs: [plan, deploy-tool, publish, beta-macos-pkg]\n",
            "    needs: [plan, deploy-tool, publish, beta-macos-pkg, beta-macos-probe]\n",
        ),
        (
            "    needs: [turn, publish, gamma,",
            "    needs: [turn, beta-macos-probe, publish, gamma,",
        ),
        (
            "    needs: [plan, publish, check-apt, beta-macos-first-install]\n",
            "    needs: [plan, publish, check-apt, beta-macos-first-install, beta-macos-probe]\n",
        ),
        (
            "    needs: [plan, publish, check-apt, beta-macos-first-install]\n",
            "    needs: [plan, publish, check-apt, beta-macos-pkg]\n",
        ),
    ] {
        assert_eq!(beta_need_problems(&text), Vec::<String>::new());
        assert_eq!(text.matches(from).count(), 1, "{from}");
        let problems = beta_need_problems(&text.replacen(from, to, 1));
        assert!(!problems.is_empty(), "{to}");
    }
    // The package upload that waits for no package job, or for its end and not its pass, is
    // refused.
    for (from, to) in [
        ("publish, beta-macos-pkg]", "publish]"),
        (" && needs.beta-macos-pkg.result == 'success' }}", " }}"),
    ] {
        assert_eq!(text.matches(from).count(), 1, "{from}");
        assert!(
            !beta_need_problems(&text.replacen(from, to, 1)).is_empty(),
            "{from}"
        );
    }
}
