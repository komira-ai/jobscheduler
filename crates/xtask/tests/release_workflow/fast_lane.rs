//! The jobs that make release.yml a fast lane, as the text of the file shows them: no wait for
//! `ci` before the release and no concurrency group, `unit-tests` beside the build, `turn`,
//! `desired`, `deployed`, and `release-red` / `release-green`, with the permission each holds.
//! Each rule is a function over the workflow's text that returns its problems, so a test can feed
//! it text with a defect and see it refused.

use super::{holders, job, jobs, workflow};

/// The condition of a job that runs for a push to main alone.
const PUSH: &str = "github.event_name == 'push' && github.ref == 'refs/heads/main'";

/// The `needs:` list of a job (empty when it has none).
fn needs(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("    needs: ["))
        .and_then(|rest| rest.strip_suffix(']'))
        .map(|list| list.split(", ").map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The job-level `if:` lines.
fn ifs(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .filter(|l| l.starts_with("    if:"))
        .map(String::as_str)
        .collect()
}

/// The job-level permissions as `name: level` lines in sorted order, or `None` when the job has
/// no `permissions:` key; `permissions: {}` is the empty list.
fn permissions(lines: &[String]) -> Option<Vec<String>> {
    let at = lines
        .iter()
        .position(|l| l.starts_with("    permissions:"))?;
    if lines[at] == "    permissions: {}" {
        return Some(Vec::new());
    }
    let mut grants: Vec<String> = lines[at + 1..]
        .iter()
        .take_while(|l| l.starts_with("      "))
        .map(|l| l.trim().to_owned())
        .collect();
    grants.sort();
    Some(grants)
}

/// The lines of a job that are not comments, joined.
fn code(lines: &[String]) -> String {
    lines
        .iter()
        .filter(|l| !l.trim_start().starts_with('#'))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// The lines of the step that starts at line `at`: up to the next step.
fn step_at(lines: &[String], at: usize) -> &[String] {
    let end = lines[at + 1..]
        .iter()
        .position(|l| l.starts_with("      - "))
        .map_or(lines.len(), |n| at + 1 + n);
    &lines[at..end]
}

/// The number of steps of a job.
fn steps(lines: &[String]) -> usize {
    lines.iter().filter(|l| l.starts_with("      - ")).count()
}

/// The text with the first `from` at or after the start of job `name` replaced by `to`.
fn alter(text: &str, name: &str, from: &str, to: &str) -> String {
    let start = text
        .find(&format!("\n  {name}:\n"))
        .unwrap_or_else(|| panic!("no job {name}"));
    let at = start
        + text[start..]
            .find(from)
            .unwrap_or_else(|| panic!("no {from:?} in {name}"));
    format!("{}{to}{}", &text[..at], &text[at + from.len()..])
}

/// Asserts that `check` finds nothing in `text`, and something in `text` once `from` is
/// replaced by `to` in job `name`.
fn refused(check: fn(&str) -> Vec<String>, text: &str, name: &str, from: &str, to: &str) {
    assert_eq!(check(text), Vec::<String>::new());
    let altered = alter(text, name, from, to);
    assert!(
        !check(&altered).is_empty(),
        "{name}: {from:?} -> {to:?} was not refused"
    );
}

/// What is wrong with a job that holds a token and runs only the downloaded `xtask` or `gh`:
/// a checkout, a build, or an action other than the artifact download.
fn runs_no_repository_code(name: &str, lines: &[String]) -> Vec<String> {
    let code = code(lines);
    let mut problems = Vec::new();
    if code.contains("actions/checkout") || code.contains("cargo ") {
        problems.push(format!("{name} checks out or builds the repository"));
    }
    for line in code.lines().filter(|l| l.contains("uses:")) {
        if !line.contains("actions/download-artifact@") {
            problems.push(format!("{name} uses {line}"));
        }
    }
    problems
}

/// What is wrong with release.yml's concurrency: any `concurrency:` or `cancel-in-progress`
/// line outside a comment.
fn concurrency_problems(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter(|l| l.contains("concurrency:") || l.contains("cancel-in-progress"))
        .map(|l| format!("concurrency: {l}"))
        .collect()
}

/// Asserts that release.yml has no concurrency group (GitHub's group cancels the waiting run, so
/// a release would be dropped; the runs take turns with `turn` instead), and that the old group
/// is refused.
#[test]
fn the_release_has_no_concurrency_group() {
    let text = workflow();
    assert_eq!(concurrency_problems(&text), Vec::<String>::new());
    let old = "concurrency:\n  group: release-${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && 'main' || github.run_id }}\n  cancel-in-progress: false\n\n";
    let with_group = text.replacen("env:\n", &format!("{old}env:\n"), 1);
    assert_eq!(concurrency_problems(&with_group).len(), 2);
    let job_level = alter(
        &text,
        "plan",
        "    runs-on:",
        "    concurrency: plan\n    runs-on:",
    );
    assert_eq!(concurrency_problems(&job_level).len(), 1);
}

/// What is wrong with the jobs that need `ci-passed` and with `plan`'s needs.
fn ci_passed_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let mut problems = Vec::new();
    let waiting: Vec<&str> = all
        .iter()
        .filter(|(_, lines)| needs(lines).iter().any(|n| n == "ci-passed"))
        .map(|(name, _)| name.as_str())
        .collect();
    if waiting != ["release-red", "release-green"] && waiting != ["release-green", "release-red"] {
        problems.push(format!("jobs that need ci-passed: {waiting:?}"));
    }
    if !needs(job(&all, "plan")).is_empty() {
        problems.push("plan has needs".to_owned());
    }
    problems
}

/// Asserts that no job but `release-red` and `release-green` needs `ci-passed` and that `plan`
/// has no needs, so the release does not wait for `ci`; a job that needs `ci-passed` and a plan
/// that waits are refused.
#[test]
fn only_the_release_reports_need_ci_passed_and_plan_waits_for_nothing() {
    let text = workflow();
    refused(
        ci_passed_problems,
        &text,
        "plan",
        "    if: ${{ github.event_name == 'pull_request'",
        "    needs: [ci-passed]\n    if: ${{ github.event_name == 'pull_request'",
    );
    refused(
        ci_passed_problems,
        &text,
        "publish",
        "    needs: [plan, artifacts, unit-tests, turn]",
        "    needs: [plan, artifacts, unit-tests, turn, ci-passed]",
    );
    refused(
        ci_passed_problems,
        &text,
        "release-green",
        "needs: [plan, ci-passed, ",
        "needs: [plan, ",
    );
}

/// What is wrong with the `plan` job's condition.
fn plan_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let want = format!("    if: ${{{{ github.event_name == 'pull_request' || ({PUSH}) }}}}");
    if ifs(job(&all, "plan")) == [want.as_str()] {
        Vec::new()
    } else {
        vec!["plan's condition".to_owned()]
    }
}

/// Asserts that `plan` runs at once for a push to main and for a pull request.
#[test]
fn plan_runs_for_a_push_to_main_or_a_pull_request_at_once() {
    let text = workflow();
    refused(
        plan_problems,
        &text,
        "plan",
        " || (github.event_name == 'push' && github.ref == 'refs/heads/main') }}",
        " }}",
    );
    refused(
        plan_problems,
        &text,
        "plan",
        "github.event_name == 'pull_request' || ",
        "",
    );
}

/// What is wrong with `publish`'s needs and condition.
fn publish_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let lines = job(&all, "publish");
    let mut problems = Vec::new();
    if needs(lines) != ["plan", "artifacts", "unit-tests", "turn"] {
        problems.push(format!("publish needs {:?}", needs(lines)));
    }
    let condition = ifs(lines);
    if condition.len() != 1
        || !condition[0].contains(PUSH)
        || !condition[0].contains("needs.plan.outputs.release == 'true'")
        || !condition[0].contains("needs.turn.outputs.go == 'true'")
        || condition[0].contains("!cancelled()")
        || condition[0].contains("always()")
    {
        problems.push(format!("publish if {condition:?}"));
    }
    problems
}

/// Asserts that `publish` needs the plan, the build, the unit tests and the turn, and runs only
/// when the turn said go, with no status function.
#[test]
fn publish_waits_for_the_build_the_unit_tests_and_the_turn() {
    let text = workflow();
    refused(
        publish_problems,
        &text,
        "publish",
        "    needs: [plan, artifacts, unit-tests, turn]",
        "    needs: [plan, artifacts, turn]",
    );
    refused(
        publish_problems,
        &text,
        "publish",
        " && needs.turn.outputs.go == 'true' }}",
        " }}",
    );
    refused(
        publish_problems,
        &text,
        "publish",
        "if: ${{ github.event_name == 'push'",
        "if: ${{ !cancelled() && github.event_name == 'push'",
    );
}

/// What is wrong with the `unit-tests` job.
fn unit_tests_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let lines = job(&all, "unit-tests");
    let body = code(lines);
    let mut problems = Vec::new();
    if permissions(lines) != Some(vec!["contents: read".to_owned()]) {
        problems.push("unit-tests permissions".to_owned());
    }
    if ifs(lines) != [format!("    if: ${{{{ {PUSH} }}}}").as_str()] {
        problems.push("unit-tests condition".to_owned());
    }
    if !lines.iter().any(|l| l == "        shard: [1, 2, 3, 4]") {
        problems.push("unit-tests shards".to_owned());
    }
    if !lines.iter().any(|l| l == "    runs-on: ubuntu-24.04") {
        problems.push("unit-tests runner".to_owned());
    }
    if !lines.iter().any(|l| l == "        image: postgres@sha256:2a0d0fe14825b0939f78a8cad5cd4e6aa68bf94d0e5dd96e24b6d23af4315545") {
        problems.push("unit-tests postgres is not pinned by digest".to_owned());
    }
    let run = "        run: cargo nextest run --workspace --locked --no-fail-fast --partition \"slice:${{ matrix.shard }}/${{ strategy.job-total }}\"";
    if !lines.iter().any(|l| l == run) {
        problems.push("unit-tests run line".to_owned());
    }
    if !body.contains(
        "cargo xtask fetch --url \"https://github.com/nextest-rs/nextest/releases/download/",
    ) || !body.contains("--sha256 \"$NEXTEST_SHA256\"")
    {
        problems.push("unit-tests nextest is not fetched pinned".to_owned());
    }
    // The only cache it writes is the nextest tarball; the Cargo cache (`target` among its paths)
    // is restored read-only.
    if body.contains("actions/cache@") {
        problems.push("unit-tests uses the saving cache action".to_owned());
    }
    let mut saves = 0;
    let mut restores_cargo = false;
    for (at, line) in lines.iter().enumerate() {
        let step = step_at(lines, at).join("\n");
        if line.contains("uses: actions/cache/save@") {
            saves += 1;
            if !step.contains("path: ${{ runner.temp }}/pinned/cargo-nextest.tar.gz\n") {
                problems.push("unit-tests saves more than the nextest tarball".to_owned());
            }
        }
        if line.contains("uses: actions/cache/restore@") && step.contains("            target\n") {
            restores_cargo = true;
        }
    }
    if saves != 1 {
        problems.push(format!("unit-tests has {saves} cache saves"));
    }
    if !restores_cargo {
        problems.push("unit-tests does not restore the cargo cache".to_owned());
    }
    problems
}

/// Asserts the shape of `unit-tests`: push to main only, `contents: read` alone, four nextest
/// shards, a digest-pinned Postgres, the Cargo cache restored and never saved, and only the
/// pinned nextest tarball saved.
#[test]
fn unit_tests_run_four_nextest_shards_on_main_and_save_only_the_nextest_tarball() {
    let text = workflow();
    let name = "unit-tests";
    for (from, to) in [
        ("      contents: read", "      contents: write"),
        ("github.ref == 'refs/heads/main' }}", "true }}"),
        ("shard: [1, 2, 3, 4]", "shard: [1, 2]"),
        ("--no-fail-fast", "--locked"),
        ("--partition", "--ignored --partition"),
        ("postgres@sha256:", "postgres:"),
        ("--sha256 \"$NEXTEST_SHA256\"", "--cache x"),
        ("uses: actions/cache/restore@", "uses: actions/cache@"),
        (
            "path: ${{ runner.temp }}/pinned/cargo-nextest.tar.gz\n          key: ${{ steps.nextest-cache.outputs",
            "path: target\n          key: ${{ steps.nextest-cache.outputs",
        ),
    ] {
        refused(unit_tests_problems, &text, name, from, to);
    }
    // The Cargo cache is not restored: its `target` path is gone.
    refused(
        unit_tests_problems,
        &text,
        name,
        "            target\n",
        "            other\n",
    );
    // A second save.
    refused(
        unit_tests_problems,
        &text,
        name,
        "      - run: cargo fetch --locked",
        "      - uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n        with:\n          path: target\n          key: k\n      - run: cargo fetch --locked",
    );
}

/// What is wrong with the `turn` job.
fn turn_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let lines = job(&all, "turn");
    let mut problems = runs_no_repository_code("turn", lines);
    if permissions(lines)
        != Some(vec![
            "actions: read".to_owned(),
            "contents: read".to_owned(),
        ])
    {
        problems.push("turn permissions".to_owned());
    }
    if needs(lines) != ["plan", "artifacts", "unit-tests", "deploy-tool"] {
        problems.push(format!("turn needs {:?}", needs(lines)));
    }
    let condition = ifs(lines);
    if condition.len() != 1
        || !condition[0].contains(PUSH)
        || !condition[0].contains("needs.plan.outputs.release == 'true'")
    {
        problems.push(format!("turn if {condition:?}"));
    }
    let run = "        run: tool/xtask release turn --repo \"$REPO\" --workflow release.yml --run-number \"$GITHUB_RUN_NUMBER\" --version \"$VERSION\" --output \"$GITHUB_OUTPUT\"";
    if lines.iter().filter(|l| l.contains("run: ")).count() != 2 || !lines.iter().any(|l| l == run)
    {
        problems.push("turn run line".to_owned());
    }
    if !lines
        .iter()
        .any(|l| l == "      go: ${{ steps.turn.outputs.go }}")
    {
        problems.push("turn output go".to_owned());
    }
    problems
}

/// Asserts the shape of `turn`: the two reads it holds, its needs, the exact `xtask release turn`
/// line, and no checkout, build or repository action.
#[test]
fn turn_holds_two_reads_and_runs_only_the_deploy_tools_turn_verb() {
    let text = workflow();
    for (from, to) in [
        (
            "      contents: read\n    outputs:\n      go:",
            "      contents: write\n    outputs:\n      go:",
        ),
        (
            "      actions: read\n      contents: read\n    outputs:\n      go:",
            "      contents: read\n    outputs:\n      go:",
        ),
        (
            "needs: [plan, artifacts, unit-tests, deploy-tool]",
            "needs: [plan, artifacts, deploy-tool]",
        ),
        ("--workflow release.yml", "--workflow ci.yml"),
        ("--run-number \"$GITHUB_RUN_NUMBER\" ", ""),
        (
            "run: tool/xtask release turn",
            "run: cargo xtask release turn",
        ),
        (
            "      - name: Wait for this run's turn",
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n      - name: Wait for this run's turn",
        ),
        (
            "      - run: chmod +x tool/xtask",
            "      - uses: actions/cache@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n      - run: chmod +x tool/xtask",
        ),
        ("&& needs.plan.outputs.release == 'true' }}", "}}"),
        ("go: ${{ steps.turn.outputs.go }}", "go: 'true'"),
    ] {
        refused(turn_problems, &text, "turn", from, to);
    }
}

/// What is wrong with the `desired` job.
fn desired_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let lines = job(&all, "desired");
    let mut problems = runs_no_repository_code("desired", lines);
    if permissions(lines)
        != Some(vec![
            "actions: write".to_owned(),
            "contents: read".to_owned(),
        ])
    {
        problems.push("desired permissions".to_owned());
    }
    if needs(lines)
        != [
            "plan",
            "turn",
            "pkg-upload",
            "stable-apt-write",
            "stable-apt-write-2",
            "stable-apt-write-3",
        ]
    {
        problems.push(format!("desired needs {:?}", needs(lines)));
    }
    let condition = ifs(lines);
    let wanted = [
        "!cancelled()",
        PUSH,
        "needs.plan.outputs.release == 'true'",
        "needs.turn.outputs.go == 'true'",
        "vars.JS_AGENT_SETS_DESIRED == 'true'",
        "needs.stable-apt-write.outputs.conflict == 'false'",
        "needs.stable-apt-write-2.outputs.conflict == 'false'",
        "needs.stable-apt-write-3.outputs.conflict == 'false'",
    ];
    if condition.len() != 1 || wanted.iter().any(|w| !condition[0].contains(w)) {
        problems.push(format!("desired if {condition:?}"));
    }
    // Two steps: the check that the release is still the newest published one, then the start
    // of hostkeeper-desired.yml, only on that check's word.
    let run = "        run: gh workflow run hostkeeper-desired.yml --repo \"$REPO\" --ref main -f version=\"$VERSION\"";
    let start = format!("        if: ${{{{ steps.newest.outputs.newest == 'true' }}}}\n{run}");
    let body = lines.join("\n");
    if steps(lines) != 2 || !body.contains(&start) {
        problems.push("desired steps".to_owned());
    }
    let check = body.find("        id: newest\n");
    let started = body.find(&start);
    if check.is_none() || started.is_none() || check > started {
        problems.push("desired does not check the newest release before the start".to_owned());
    }
    for needed in [
        "select(.draft == false and .prerelease == false)",
        "| sort -V | tail -n 1)\"",
        "if [ \"$newest\" = \"v$VERSION\" ]; then\n            echo \"newest=true\" >> \"$GITHUB_OUTPUT\"",
    ] {
        if !body.contains(needed) {
            problems.push(format!("desired's newest check lacks {needed:?}"));
        }
    }
    problems
}

/// Asserts the shape of `desired`: `actions: write` and `contents: read`, a check that the
/// release is the newest published one (a re-run of an older run's failed jobs must not move the
/// farm back), then one `gh workflow run` of hostkeeper-desired.yml on main with the plan's
/// version on that check's word alone, and the turn's go, the variable and a stable write
/// without conflict in its condition.
#[test]
fn desired_starts_hostkeeper_desired_with_actions_write_alone() {
    let text = workflow();
    for (from, to) in [
        (
            "      actions: write\n      contents: read\n",
            "      actions: write\n      contents: write\n",
        ),
        ("      actions: write\n", "      actions: read\n"),
        (
            "        if: ${{ steps.newest.outputs.newest == 'true' }}\n",
            "",
        ),
        ("[ \"$newest\" = \"v$VERSION\" ]", "[ -n \"$newest\" ]"),
        ("| sort -V | tail -n 1)", "| head -n 1)"),
        ("select(.draft == false and .prerelease == false) | ", ""),
        ("        id: newest\n", ""),
        ("needs: [plan, turn, pkg-upload,", "needs: [plan, turn,"),
        ("needs: [plan, turn,", "needs: [plan,"),
        ("--ref main -f", "--ref dev -f"),
        (
            "workflow run hostkeeper-desired.yml",
            "workflow run other.yml",
        ),
        (
            "&& needs.turn.outputs.go == 'true' && vars.JS_AGENT",
            "&& vars.JS_AGENT",
        ),
        ("&& vars.JS_AGENT_SETS_DESIRED == 'true'", ""),
        (
            "needs.stable-apt-write-3.outputs.conflict == 'false'",
            "true",
        ),
        ("if: ${{ !cancelled() && ", "if: ${{ "),
        (
            "      - name: Start hostkeeper-desired.yml",
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n      - name: Start hostkeeper-desired.yml",
        ),
    ] {
        refused(desired_problems, &text, "desired", from, to);
    }
}

/// What is wrong with the `deployed` job.
fn deployed_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let lines = job(&all, "deployed");
    let mut problems = runs_no_repository_code("deployed", lines);
    if permissions(lines) != Some(Vec::new()) {
        problems.push("deployed permissions".to_owned());
    }
    if ifs(lines) != [format!("    if: ${{{{ always() && {PUSH} }}}}").as_str()] {
        problems.push("deployed condition".to_owned());
    }
    let want = [
        "turn",
        "publish",
        "gamma",
        "cp-deploy",
        "pkg-upload",
        "desired",
        "stable-apt-write",
        "stable-apt-write-2",
        "stable-apt-write-3",
    ];
    let have = needs(lines);
    if want.iter().any(|w| !have.iter().any(|h| h == w)) {
        problems.push(format!("deployed needs {have:?}"));
    }
    if steps(lines) != 1 || code(lines).contains("gh ") {
        problems.push("deployed does more than echo".to_owned());
    }
    problems
}

/// Asserts that `deployed` runs whatever the results of the publish and deploy jobs (`always()`),
/// for a push to main, holds no permission and only echoes.
#[test]
fn deployed_always_ends_after_the_deploy_jobs_and_holds_nothing() {
    let text = workflow();
    for (from, to) in [
        ("if: ${{ always() && ", "if: ${{ !cancelled() && "),
        (
            "    permissions: {}\n    steps:\n      - run: echo \"the publish",
            "    permissions:\n      contents: read\n    steps:\n      - run: echo \"the publish",
        ),
        ("needs: [turn, publish,", "needs: [turn,"),
        (
            "      - run: echo \"the publish",
            "      - run: gh run list\n      - run: echo \"the publish",
        ),
    ] {
        refused(deployed_problems, &text, "deployed", from, to);
    }
}

/// What is wrong with the `release-red` and `release-green` jobs.
fn report_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let mut problems = Vec::new();
    for name in ["release-red", "release-green"] {
        let lines = job(&all, name);
        problems.extend(runs_no_repository_code(name, lines));
        if permissions(lines) != Some(vec!["actions: read".to_owned(), "issues: write".to_owned()])
        {
            problems.push(format!("{name} permissions"));
        }
        let body = code(lines);
        if !body.contains("--label main-red") || !body.contains("RELEASE-RED: jobscheduler") {
            problems.push(format!("{name} label or title"));
        }
        let condition = ifs(lines);
        let status = if name == "release-red" {
            "failure()"
        } else {
            "!cancelled() && !failure()"
        };
        if condition.len() != 1
            || !condition[0].contains(status)
            || !condition[0].contains(PUSH)
            || (name == "release-red" && condition[0].contains("!failure()"))
        {
            problems.push(format!("{name} if {condition:?}"));
        }
    }
    let red = code(job(&all, "release-red"));
    let create = red
        .lines()
        .find(|l| l.contains("gh issue create"))
        .unwrap_or("");
    if !create.contains("--label main-red")
        || !create.contains("--title \"RELEASE-RED: jobscheduler ")
    {
        problems.push(
            "release-red does not open a labelled RELEASE-RED: jobscheduler issue".to_owned(),
        );
    }
    let green = code(job(&all, "release-green"));
    // A run whose commit is no longer main's tip closes nothing: a newer run may have opened the
    // issue.
    let tip = "if [ \"$tip\" != \"$COMMIT\" ]; then";
    if !green.contains("tip=\"$(gh api \"repos/$REPO/commits/main\" --jq .sha)\"")
        || green.find(tip).is_none_or(|at| {
            green.find("gh issue close").is_none_or(|close| close < at)
                || !green[at..].contains("exit 0\n          fi")
        })
    {
        problems
            .push("release-green closes without checking that its commit is main's tip".to_owned());
    }
    if !green.contains("gh issue close")
        || !job(&all, "release-green")
            .iter()
            .any(|l| l.contains("needs.plan.outputs.release == 'true'"))
    {
        problems.push("release-green closes nothing or runs without a release".to_owned());
    }
    problems
}

/// Asserts that `release-red` opens (or comments on) a `RELEASE-RED: jobscheduler` issue with the
/// label `main-red` on a failure and `release-green` closes them, each holding `actions: read`
/// and `issues: write` alone and running only `gh`.
#[test]
fn the_release_reports_hold_issues_write_and_run_only_gh() {
    let text = workflow();
    for (name, from, to) in [
        (
            "release-red",
            "      issues: write",
            "      issues: write\n      contents: write",
        ),
        ("release-red", "      actions: read\n", ""),
        ("release-green", "      issues: write", "      issues: read"),
        (
            "release-red",
            "if: ${{ failure() && ",
            "if: ${{ always() && ",
        ),
        (
            "release-green",
            "!cancelled() && !failure() && ",
            "!cancelled() && ",
        ),
        ("release-red", "--label main-red --title", "--title"),
        (
            "release-red",
            "--title \"RELEASE-RED: jobscheduler ",
            "--title \"RED: ",
        ),
        ("release-green", "--label main-red", "--label bug"),
        (
            "release-red",
            "    steps:\n",
            "    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n",
        ),
        (
            "release-green",
            "    steps:\n",
            "    steps:\n      - run: cargo build\n",
        ),
        ("release-green", "gh issue close", "gh issue list"),
        (
            "release-green",
            "if [ \"$tip\" != \"$COMMIT\" ]; then",
            "if false; then",
        ),
        (
            "release-green",
            "repos/$REPO/commits/main",
            "repos/$REPO/commits/$COMMIT",
        ),
    ] {
        refused(report_problems, &text, name, from, to);
    }
}

/// What is wrong with who holds `actions: write` and `issues: write`.
fn write_grant_problems(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if holders(text, "actions: write") != ["desired"] {
        problems.push(format!(
            "actions: write {:?}",
            holders(text, "actions: write")
        ));
    }
    if holders(text, "issues: write") != ["release-red", "release-green"] {
        problems.push(format!(
            "issues: write {:?}",
            holders(text, "issues: write")
        ));
    }
    problems
}

/// Asserts that `actions: write` is held by `desired` alone and `issues: write` by `release-red`
/// and `release-green` alone.
#[test]
fn actions_write_is_held_by_desired_and_issues_write_by_the_two_reports() {
    let text = workflow();
    refused(
        write_grant_problems,
        &text,
        "turn",
        "      actions: read\n      contents: read\n    outputs",
        "      actions: write\n      contents: read\n    outputs",
    );
    refused(
        write_grant_problems,
        &text,
        "plan",
        "    permissions:\n      contents: read\n",
        "    permissions:\n      issues: write\n",
    );
    refused(
        write_grant_problems,
        &text,
        "release-green",
        "      issues: write\n",
        "      issues: read\n",
    );
    refused(
        write_grant_problems,
        &text,
        "desired",
        "      actions: write\n",
        "      actions: read\n",
    );
}
