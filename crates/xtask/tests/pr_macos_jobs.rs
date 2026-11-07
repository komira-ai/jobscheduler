//! The workflow lint of the macOS jobs a pull request runs, over every workflow in
//! `.github/workflows`, read as text.
//!
//! A job is a pull request macOS job when
//!   - its workflow starts on `pull_request` or `pull_request_target`, or it is a job of a
//!     workflow that a job of such a workflow calls (`uses: ./.github/workflows/<file>`);
//!   - its runner names macOS: a `runs-on:` or a matrix `runner:` line with `macos` in it;
//!   - and its `if:` does not pin another event: no `&&`-joined part of it, outside
//!     parentheses, is `github.event_name == '<another event>'` or
//!     `github.event_name != 'pull_request'`.
//!
//! Such a job must
//!   - name no secret (`secrets.`, a `secrets:` key) and not the job's token (`github.token`,
//!     `GITHUB_TOKEN`): not in its own lines, not in its workflow's top-level lines, and not in
//!     the job that calls it;
//!   - run in no deployment environment (`environment:`), whose secrets it would get;
//!   - hold no write permission, `id-token: write` (OIDC) included, under a permissions block
//!     of its own or its workflow's (without either it gets the repository's default token,
//!     which may write);
//!   - be in a workflow whose concurrency cancels the older run of the same pull request (for a
//!     called workflow, the caller's), except the release dry run (`RELEASE_DRY_RUN`).
//!
//! A pull request workflow may not call a workflow of another repository: the lint cannot read
//! its jobs. The set of pull request macOS jobs is pinned (`EXPECTED`). The negative cases run
//! the same check on workflows written here, and on the repository's own workflows with one
//! line changed.

use std::path::Path;
use std::process::Command;

/// The pull request macOS jobs of the repository: `<file>/<job>`, and for a called workflow's
/// job `<file>/<calling job> -> <called file>/<job>`.
const EXPECTED: [&str; 4] = [
    "ci.yml/macos",
    "ci.yml/macos-pkg -> js-agent-macos-pkg.yml/build",
    "macos-probe.yml/probe",
    "release.yml/build",
];

/// The workflow whose pull request run is not cancelled by a newer push: the release dry run,
/// which a pull request starts only when it changes release.yml itself
/// (`the_release_dry_run_starts_only_on_a_change_to_release_yml` pins that), and which this
/// change leaves as it is.
const RELEASE_DRY_RUN: &str = "release.yml";

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

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// A workflow cut into its top-level lines (before `jobs:`) and its jobs, comments dropped.
struct Workflow {
    file: String,
    top: Vec<String>,
    jobs: Vec<(String, Vec<String>)>,
}

impl Workflow {
    fn parse(file: &str, text: &str) -> Workflow {
        let mut top = Vec::new();
        let mut jobs: Vec<(String, Vec<String>)> = Vec::new();
        let mut in_jobs = false;
        for line in text.lines() {
            if line.trim_start().starts_with('#') || line.trim().is_empty() {
                continue;
            }
            if line == "jobs:" {
                in_jobs = true;
            } else if in_jobs && indent(line) == 0 {
                in_jobs = false;
                top.push(line.to_owned());
            } else if in_jobs && indent(line) == 2 {
                let name = line.trim().trim_end_matches(':').to_owned();
                jobs.push((name, Vec::new()));
            } else if in_jobs {
                jobs.last_mut()
                    .expect("a job line before any job")
                    .1
                    .push(line.to_owned());
            } else {
                top.push(line.to_owned());
            }
        }
        Workflow {
            file: file.to_owned(),
            top,
            jobs,
        }
    }

    /// Whether the workflow starts on a pull request: `on: pull_request`,
    /// `on: [..., pull_request]` or an `on:` block with a `pull_request` key.
    fn on_pull_request(&self) -> bool {
        let mut in_on = false;
        for line in &self.top {
            if indent(line) == 0 {
                in_on = line == "on:";
                if line
                    .strip_prefix("on:")
                    .is_some_and(|value| value.contains("pull_request"))
                {
                    return true;
                }
            } else if in_on && indent(line) == 2 && line.trim().starts_with("pull_request") {
                return true;
            }
        }
        false
    }

    /// The value of the top-level key `key` and the lines under it.
    fn top_block(&self, key: &str) -> Option<Vec<String>> {
        block(&self.top, 0, key)
    }
}

/// The key `key` at `at` spaces in `lines`: its own value (if any) and the trimmed lines
/// indented under it.
fn block(lines: &[String], at: usize, key: &str) -> Option<Vec<String>> {
    let head = format!("{key}:");
    let start = lines.iter().position(|l| {
        indent(l) == at && (l.trim() == head || l.trim().starts_with(&format!("{head} ")))
    })?;
    let mut out = Vec::new();
    let value = lines[start].trim()[head.len()..].trim();
    if !value.is_empty() {
        out.push(value.to_owned());
    }
    out.extend(
        lines[start + 1..]
            .iter()
            .take_while(|l| indent(l) > at)
            .map(|l| l.trim().to_owned()),
    );
    Some(out)
}

/// The `&&`-joined parts of an `if:` expression, outside parentheses.
fn conjuncts(cond: &str) -> Vec<String> {
    let e = cond.trim();
    let e = e
        .strip_prefix("${{")
        .and_then(|c| c.strip_suffix("}}"))
        .unwrap_or(e);
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut chars = e.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '&' if depth == 0 && chars.peek() == Some(&'&') => {
                chars.next();
                parts.push(current.trim().to_owned());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current.trim().to_owned());
    parts
}

/// Whether a job's `if:` keeps it off pull requests.
fn pins_another_event(job: &[String]) -> bool {
    let Some(cond) = block(job, 4, "if") else {
        return false;
    };
    conjuncts(&cond.join(" ")).iter().any(|c| {
        let c = c.replace('"', "'");
        // The whole part is one comparison of the event name with one quoted word.
        let event = c
            .strip_prefix("github.event_name == '")
            .and_then(|rest| rest.strip_suffix('\''))
            .filter(|word| !word.contains(['\'', ' ']));
        c == "github.event_name != 'pull_request'"
            || event.is_some_and(|e| e != "pull_request" && e != "pull_request_target")
    })
}

fn on_macos(job: &[String]) -> bool {
    job.iter().any(|l| {
        let t = l.trim().trim_start_matches("- ");
        (t.starts_with("runs-on:") || t.starts_with("runner:"))
            && t.to_lowercase().contains("macos")
    })
}

/// What a job's lines say about secrets and the job token.
fn secret_problems(id: &str, whose: &str, lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if lines
        .iter()
        .any(|l| l.contains("secrets.") || l.trim().starts_with("secrets:"))
    {
        out.push(format!("{id}: {whose} names a secret"));
    }
    if lines
        .iter()
        .any(|l| l.contains("github.token") || l.contains("GITHUB_TOKEN"))
    {
        out.push(format!("{id}: {whose} names the job token"));
    }
    out
}

/// What a permissions block grants that a pull request macOS job may not hold.
fn permission_problems(id: &str, perms: Option<Vec<String>>) -> Vec<String> {
    let Some(perms) = perms else {
        return vec![format!(
            "{id}: no permissions block, so the job gets the repository's default token"
        )];
    };
    let mut out = Vec::new();
    for p in perms {
        if p == "id-token: write" {
            out.push(format!("{id}: holds id-token: write (OIDC)"));
        } else if p == "write-all" || p.ends_with(": write") {
            out.push(format!("{id}: holds {p}"));
        }
    }
    out
}

/// Whether a workflow's concurrency cancels the older run of a pull request.
fn cancels_older_pull_request_runs(w: &Workflow) -> bool {
    let Some(c) = w.top_block("concurrency") else {
        return false;
    };
    let group = c
        .iter()
        .any(|l| l.starts_with("group:") && l.contains("github.event.pull_request.number"));
    let cancel = c.iter().any(|l| {
        l == "cancel-in-progress: true"
            || l == "cancel-in-progress: ${{ github.event_name == 'pull_request' }}"
    });
    group && cancel
}

/// The pull request macOS jobs of `workflows` (file name and text) and what is wrong with them.
fn lint(workflows: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    let parsed: Vec<Workflow> = workflows
        .iter()
        .map(|(file, text)| Workflow::parse(file, text))
        .collect();
    let by_file = |file: &str| parsed.iter().find(|w| w.file == file);
    let mut jobs = Vec::new();
    let mut problems = Vec::new();
    for w in parsed.iter().filter(|w| w.on_pull_request()) {
        let cancels = w.file == RELEASE_DRY_RUN || cancels_older_pull_request_runs(w);
        for (name, lines) in &w.jobs {
            if pins_another_event(lines) {
                continue;
            }
            let id = format!("{}/{name}", w.file);
            // A job that calls a workflow: that workflow's jobs run for this pull request.
            if let Some(uses) = block(lines, 4, "uses") {
                let target = uses.first().cloned().unwrap_or_default();
                let Some(called) = target
                    .strip_prefix("./.github/workflows/")
                    .and_then(by_file)
                else {
                    problems.push(format!(
                        "{id}: calls {target}, a workflow the lint cannot read"
                    ));
                    continue;
                };
                for (inner, inner_lines) in &called.jobs {
                    if pins_another_event(inner_lines) || !on_macos(inner_lines) {
                        continue;
                    }
                    let inner_id = format!("{id} -> {}/{inner}", called.file);
                    problems.extend(secret_problems(&inner_id, "the calling job", lines));
                    problems.extend(secret_problems(&inner_id, "the job", inner_lines));
                    problems.extend(secret_problems(
                        &inner_id,
                        "the called workflow",
                        &called.top,
                    ));
                    problems.extend(secret_problems(&inner_id, "the workflow", &w.top));
                    if block(inner_lines, 4, "environment").is_some() {
                        problems.push(format!("{inner_id}: runs in a deployment environment"));
                    }
                    let perms = block(inner_lines, 4, "permissions")
                        .or_else(|| called.top_block("permissions"))
                        .or_else(|| block(lines, 4, "permissions"));
                    problems.extend(permission_problems(&inner_id, perms));
                    if !cancels {
                        problems.push(format!(
                            "{inner_id}: a newer push to the pull request does not cancel it"
                        ));
                    }
                    jobs.push(inner_id);
                }
                continue;
            }
            if !on_macos(lines) {
                continue;
            }
            problems.extend(secret_problems(&id, "the job", lines));
            problems.extend(secret_problems(&id, "the workflow", &w.top));
            if block(lines, 4, "environment").is_some() {
                problems.push(format!("{id}: runs in a deployment environment"));
            }
            let perms = block(lines, 4, "permissions").or_else(|| w.top_block("permissions"));
            problems.extend(permission_problems(&id, perms));
            if !cancels {
                problems.push(format!(
                    "{id}: a newer push to the pull request does not cancel it"
                ));
            }
            jobs.push(id);
        }
    }
    jobs.sort();
    (jobs, problems)
}

/// One workflow written here: `on`, the top-level lines after it, and the jobs.
fn one(file: &str, text: &str) -> Vec<(String, String)> {
    vec![(file.to_owned(), text.to_owned())]
}

/// A pull request workflow that cancels older runs and grants nothing at the top, with `jobs`
/// appended.
fn pr_workflow(jobs: &str) -> String {
    format!(
        "name: t\non:\n  pull_request:\npermissions: {{}}\nconcurrency:\n  group: t-${{{{ github.event.pull_request.number || github.ref }}}}\n  cancel-in-progress: ${{{{ github.event_name == 'pull_request' }}}}\njobs:\n{jobs}"
    )
}

/// A macOS job named `mac` with read access and `extra` lines in its block (four spaces).
fn mac_job(extra: &str) -> String {
    format!(
        "  mac:\n    runs-on: macos-15\n    permissions:\n      contents: read\n{extra}    steps:\n      - run: cargo test\n"
    )
}

#[test]
fn the_repository_s_pull_request_macos_jobs_are_the_expected_ones_and_hold_nothing() {
    let (jobs, problems) = lint(&repository_workflows());
    assert_eq!(problems, Vec::<String>::new());
    assert_eq!(jobs, EXPECTED);
}

#[test]
fn a_pull_request_macos_job_with_a_secret_is_refused() {
    let w = pr_workflow(&mac_job(
        "    env:\n      KEY: ${{ secrets.SIGNING_KEY }}\n",
    ));
    let (jobs, problems) = lint(&one("t.yml", &w));
    assert_eq!(jobs, ["t.yml/mac"]);
    assert_eq!(problems, ["t.yml/mac: the job names a secret"]);
    // The same in a step's environment, and in the workflow's own environment.
    let w = pr_workflow(&format!(
        "{}        env:\n          KEY: ${{{{ secrets.SIGNING_KEY }}}}\n",
        mac_job("")
    ));
    assert_eq!(
        lint(&one("t.yml", &w)).1,
        ["t.yml/mac: the job names a secret"]
    );
    let w = pr_workflow(&mac_job("")).replace(
        "permissions: {}\n",
        "permissions: {}\nenv:\n  KEY: ${{ secrets.SIGNING_KEY }}\n",
    );
    assert_eq!(
        lint(&one("t.yml", &w)).1,
        ["t.yml/mac: the workflow names a secret"]
    );
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
fn the_package_s_sign_job_called_on_a_pull_request_is_refused() {
    // The sign job holds the signing secrets and the environment; only its `if:` keeps it off
    // the pull request that ci.yml's macos-pkg job runs it for.
    let mut workflows = repository_workflows();
    let pkg = workflows
        .iter_mut()
        .find(|(f, _)| f == "js-agent-macos-pkg.yml")
        .expect("pkg");
    let gate = "    if: ${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' }}\n";
    assert_eq!(pkg.1.matches(gate).count(), 1);
    pkg.1 = pkg.1.replace(gate, "");
    let (jobs, problems) = lint(&workflows);
    let sign = "ci.yml/macos-pkg -> js-agent-macos-pkg.yml/sign";
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
fn a_secret_passed_to_a_called_workflow_is_refused() {
    let caller = pr_workflow(
        "  call:\n    permissions:\n      contents: read\n    uses: ./.github/workflows/called.yml\n    secrets: inherit\n",
    );
    let called = format!(
        "name: c\non:\n  workflow_call:\npermissions: {{}}\njobs:\n{}",
        mac_job("")
    );
    let workflows = vec![
        ("called.yml".to_owned(), called.clone()),
        ("t.yml".to_owned(), caller),
    ];
    let (jobs, problems) = lint(&workflows);
    assert_eq!(jobs, ["t.yml/call -> called.yml/mac"]);
    assert_eq!(
        problems,
        ["t.yml/call -> called.yml/mac: the calling job names a secret"]
    );
    // Named secrets in a `secrets:` block too.
    let caller = pr_workflow(
        "  call:\n    uses: ./.github/workflows/called.yml\n    secrets:\n      KEY: ${{ secrets.KEY }}\n",
    );
    let workflows = vec![
        ("called.yml".to_owned(), called),
        ("t.yml".to_owned(), caller),
    ];
    assert_eq!(
        lint(&workflows).1,
        ["t.yml/call -> called.yml/mac: the calling job names a secret"]
    );
}

#[test]
fn a_called_macos_job_with_a_secret_or_a_write_permission_is_refused() {
    let caller = pr_workflow("  call:\n    uses: ./.github/workflows/called.yml\n");
    let called = "name: c\non:\n  workflow_call:\nenv:\n  K: ${{ secrets.K }}\njobs:\n  mac:\n    runs-on: macos-15\n    permissions:\n      id-token: write\n    steps:\n      - run: echo \"$GITHUB_TOKEN\"\n  linux:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: echo ${{ secrets.K }}\n";
    let workflows = vec![
        ("called.yml".to_owned(), called.to_owned()),
        ("t.yml".to_owned(), caller),
    ];
    let (jobs, problems) = lint(&workflows);
    let id = "t.yml/call -> called.yml/mac";
    assert_eq!(jobs, [id]);
    assert_eq!(
        problems,
        [
            format!("{id}: the job names the job token"),
            format!("{id}: the called workflow names a secret"),
            format!("{id}: holds id-token: write (OIDC)"),
        ]
    );
    // Without a block of its own or the called workflow's, the caller's applies.
    let called = "name: c\non:\n  workflow_call:\njobs:\n  mac:\n    runs-on: macos-15\n    steps:\n      - run: true\n".to_owned();
    let caller = pr_workflow(
        "  call:\n    permissions:\n      contents: write\n    uses: ./.github/workflows/called.yml\n",
    );
    let workflows = vec![
        ("called.yml".to_owned(), called),
        ("t.yml".to_owned(), caller),
    ];
    assert_eq!(lint(&workflows).1, [format!("{id}: holds contents: write")]);
}

#[test]
fn a_call_to_a_workflow_of_another_repository_is_refused() {
    let w = pr_workflow(
        "  call:\n    uses: someone/else/.github/workflows/x.yml@0123456789012345678901234567890123456789\n",
    );
    let (jobs, problems) = lint(&one("t.yml", &w));
    assert!(jobs.is_empty());
    assert_eq!(
        problems,
        [
            "t.yml/call: calls someone/else/.github/workflows/x.yml@0123456789012345678901234567890123456789, a workflow the lint cannot read"
        ]
    );
}

#[test]
fn a_pull_request_macos_job_with_oidc_or_any_write_permission_is_refused() {
    for (perms, want) in [
        (
            "    permissions:\n      contents: read\n      id-token: write\n",
            "t.yml/mac: holds id-token: write (OIDC)",
        ),
        (
            "    permissions:\n      contents: write\n",
            "t.yml/mac: holds contents: write",
        ),
        ("    permissions: write-all\n", "t.yml/mac: holds write-all"),
    ] {
        let job = format!("  mac:\n    runs-on: macos-15\n{perms}    steps:\n      - run: true\n");
        assert_eq!(lint(&one("t.yml", &pr_workflow(&job))).1, [want], "{perms}");
    }
    // A job with no block of its own gets the workflow's.
    let job = "  mac:\n    runs-on: macos-15\n    steps:\n      - run: true\n";
    let w = pr_workflow(job).replace("permissions: {}\n", "permissions:\n  id-token: write\n");
    assert_eq!(
        lint(&one("t.yml", &w)).1,
        ["t.yml/mac: holds id-token: write (OIDC)"]
    );
    // And with no block anywhere, the repository's default token.
    let w = pr_workflow(job).replace("permissions: {}\n", "");
    assert_eq!(
        lint(&one("t.yml", &w)).1,
        ["t.yml/mac: no permissions block, so the job gets the repository's default token"]
    );
    // Read access and an empty block pass.
    let w = pr_workflow(job);
    assert!(lint(&one("t.yml", &w)).1.is_empty());
}

#[test]
fn a_pull_request_macos_job_in_an_environment_or_naming_the_token_is_refused() {
    let w = pr_workflow(&mac_job("    environment: macos-signing\n"));
    assert_eq!(
        lint(&one("t.yml", &w)).1,
        ["t.yml/mac: runs in a deployment environment"]
    );
    let w = pr_workflow(&format!(
        "{}        env:\n          GH_TOKEN: ${{{{ github.token }}}}\n",
        mac_job("")
    ));
    assert_eq!(
        lint(&one("t.yml", &w)).1,
        ["t.yml/mac: the job names the job token"]
    );
}

#[test]
fn a_pull_request_macos_run_that_a_newer_push_does_not_cancel_is_refused() {
    let want = ["t.yml/mac: a newer push to the pull request does not cancel it"];
    // No concurrency at all.
    let w = pr_workflow(&mac_job("")).replace(
        "concurrency:\n  group: t-${{ github.event.pull_request.number || github.ref }}\n  cancel-in-progress: ${{ github.event_name == 'pull_request' }}\n",
        "",
    );
    assert_eq!(lint(&one("t.yml", &w)).1, want);
    // A group per pull request that queues instead of cancelling.
    let w = pr_workflow(&mac_job("")).replace(
        "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
        "cancel-in-progress: false",
    );
    assert_eq!(lint(&one("t.yml", &w)).1, want);
    // A group per run, which never meets the older run.
    let w = pr_workflow(&mac_job("")).replace(
        "github.event.pull_request.number || github.ref",
        "github.run_id",
    );
    assert_eq!(lint(&one("t.yml", &w)).1, want);
    // `true` cancels too.
    let w = pr_workflow(&mac_job("")).replace(
        "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
        "cancel-in-progress: true",
    );
    assert!(lint(&one("t.yml", &w)).1.is_empty());
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
fn jobs_off_pull_requests_off_macos_or_in_other_workflows_are_not_this_lint_s() {
    let secret = "    env:\n      KEY: ${{ secrets.KEY }}\n";
    // A macOS job with a secret in a workflow a pull request does not start.
    let w = pr_workflow(&mac_job(secret)).replace("  pull_request:\n", "  workflow_dispatch:\n");
    assert_eq!(lint(&one("t.yml", &w)), (vec![], vec![]));
    // A Linux job of a pull request workflow.
    let w = pr_workflow(&mac_job(secret).replace("macos-15", "ubuntu-24.04"));
    assert_eq!(lint(&one("t.yml", &w)), (vec![], vec![]));
    // A macOS job whose `if:` pins another event, in each spelling.
    for cond in [
        "${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' }}",
        "${{ github.event_name == 'push' }}",
        "${{ !cancelled() && github.event_name != 'pull_request' }}",
        "github.event_name == \"schedule\"",
    ] {
        let w = pr_workflow(&mac_job(&format!("    if: {cond}\n{secret}")));
        assert_eq!(lint(&one("t.yml", &w)), (vec![], vec![]), "{cond}");
    }
    // A comment that names a secret is not a secret.
    let w = pr_workflow(&mac_job("    # reads secrets.KEY? no.\n"));
    assert_eq!(
        lint(&one("t.yml", &w)),
        (vec!["t.yml/mac".to_owned()], vec![])
    );
}

#[test]
fn an_if_that_does_not_pin_another_event_keeps_the_job_on_pull_requests() {
    let secret = "    env:\n      KEY: ${{ secrets.KEY }}\n";
    for cond in [
        "${{ github.event_name == 'pull_request' }}",
        "${{ github.event_name == 'pull_request_target' }}",
        "${{ github.event_name == 'push' || github.event_name == 'pull_request' }}",
        "${{ !cancelled() && (github.event_name != 'pull_request' || needs.changes.outputs.macos == 'true') }}",
        "${{ always() }}",
    ] {
        let w = pr_workflow(&mac_job(&format!("    if: {cond}\n{secret}")));
        assert_eq!(
            lint(&one("t.yml", &w)).1,
            ["t.yml/mac: the job names a secret"],
            "{cond}"
        );
    }
}

#[test]
fn every_spelling_of_the_trigger_and_of_the_runner_is_seen() {
    let refused = ["t.yml/mac: the job names a secret"];
    let secret = "    env:\n      KEY: ${{ secrets.KEY }}\n";
    for on in [
        "on: pull_request\n",
        "on: [push, pull_request]\n",
        "on:\n  push:\n    branches: [main]\n  pull_request_target:\n",
        "on:\n  pull_request:\n    paths:\n      - crates/**\n",
    ] {
        let w = pr_workflow(&mac_job(secret)).replace("on:\n  pull_request:\n", on);
        assert_eq!(lint(&one("t.yml", &w)).1, refused, "{on}");
    }
    // A matrix runner, and the image name in any case.
    let job = format!(
        "  mac:\n    runs-on: ${{{{ matrix.runner }}}}\n    permissions:\n      contents: read\n    strategy:\n      matrix:\n        include:\n          - runner: ubuntu-24.04\n          - runner: macOS-15\n{secret}    steps:\n      - run: true\n"
    );
    assert_eq!(lint(&one("t.yml", &pr_workflow(&job))).1, refused);
}

/// The script of the step of `macos-result`, with its indentation removed.
fn result_script() -> String {
    let text = read("ci.yml");
    let start = text.find("\n  macos-result:\n").expect("macos-result job");
    let job = &text[start..];
    let mut lines = job.lines().skip_while(|l| l.trim() != "run: |");
    let header = lines.next().expect("run block");
    let pad = " ".repeat(indent(header) + 2);
    let body: Vec<&str> = lines
        .take_while(|l| l.is_empty() || l.starts_with(&pad))
        .map(|l| l.get(pad.len()..).unwrap_or(""))
        .collect();
    assert!(body.len() > 5, "{body:?}");
    body.join("\n") + "\n"
}

/// Runs `macos-result`'s script with the results of the jobs it needs and returns whether it
/// passed.
fn result(changes: &str, wanted: &str, macos: &str, pkg: &str) -> bool {
    Command::new("bash")
        .arg("-c")
        .arg(result_script())
        .env("CHANGES", changes)
        .env("WANTED", wanted)
        .env("MACOS", macos)
        .env("PKG", pkg)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("run bash")
        .success()
}

#[test]
fn the_result_job_passes_only_when_the_macos_jobs_did_what_the_filter_said() {
    // Passed: the filter said yes and both jobs succeeded, or it said no and both were skipped.
    assert!(result("success", "true", "success", "success"));
    assert!(result("success", "false", "skipped", "skipped"));
    // A macOS job that failed, was cancelled or timed out (a failure to Actions).
    for bad in ["failure", "cancelled"] {
        assert!(!result("success", "true", bad, "success"), "{bad}");
        assert!(!result("success", "true", "success", bad), "{bad}");
    }
    // A macOS job skipped although the filter said yes.
    assert!(!result("success", "true", "skipped", "success"));
    assert!(!result("success", "true", "success", "skipped"));
    // Jobs that ran although the filter said no.
    assert!(!result("success", "false", "success", "skipped"));
    // A filter that failed or was cancelled fails it, whatever the macOS jobs did.
    for bad in ["failure", "cancelled", "skipped"] {
        assert!(!result(bad, "", "success", "success"), "{bad}");
    }
}

#[test]
fn the_result_job_runs_on_every_pull_request_after_the_macos_jobs_and_holds_nothing() {
    let w = Workflow::parse("ci.yml", &read("ci.yml"));
    let job = &w
        .jobs
        .iter()
        .find(|(n, _)| n == "macos-result")
        .expect("macos-result")
        .1;
    assert_eq!(
        block(job, 4, "needs").expect("needs"),
        ["[changes, macos, macos-pkg]"]
    );
    assert_eq!(
        block(job, 4, "if").expect("if"),
        ["${{ !cancelled() && github.event_name == 'pull_request' }}"]
    );
    assert_eq!(block(job, 4, "permissions").expect("permissions"), ["{}"]);
    assert_eq!(
        block(job, 4, "runs-on").expect("runs-on"),
        ["ubuntu-latest"]
    );
    assert!(!job.iter().any(|l| l.contains("actions/checkout")));
    // The two macOS jobs run on the filter's yes; the package job only on a pull request.
    let macos_pkg = &w
        .jobs
        .iter()
        .find(|(n, _)| n == "macos-pkg")
        .expect("macos-pkg")
        .1;
    assert_eq!(
        block(macos_pkg, 4, "if").expect("if"),
        [
            "${{ !cancelled() && github.event_name == 'pull_request' && (needs.changes.result != 'success' || needs.changes.outputs.macos == 'true') }}"
        ]
    );
    assert_eq!(
        block(macos_pkg, 4, "uses").expect("uses"),
        ["./.github/workflows/js-agent-macos-pkg.yml"]
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
    let release = read("release.yml");
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
    // The same verbs and checks as the leg.
    let step = &ci[ci
        .find("      - name: Build, stage and package the release's macOS binaries\n")
        .expect("step")..];
    let step = &step[..step.find("      - uses: actions/cache/save").expect("end")];
    for needle in [
        "cargo build --locked --release --target \"$TARGET\" -p \"$package\" --bin \"$bin\"",
        "cargo xtask release stage --bin \"$bin\" --target \"$TARGET\"",
        "cargo xtask release hostkeeper-package --binary \"target/$TARGET/release/js-agent\" --target \"$TARGET\"",
        "otool -l",
        "--version",
    ] {
        assert!(step.contains(needle), "{needle}");
        assert!(release.contains(needle), "release.yml: {needle}");
    }
}
