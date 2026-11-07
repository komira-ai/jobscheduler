//! The negative and edge cases of the lint, over workflows written in the test.

use crate::lint::lint;

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

#[test]
fn a_write_permission_spelled_with_quotes_a_comment_or_a_mapping_is_refused() {
    for (perms, want) in [
        (
            "    permissions:\n      contents: write # to push\n",
            "t.yml/mac: holds contents: write",
        ),
        (
            "    permissions:\n      id-token: \"write\"\n",
            "t.yml/mac: holds id-token: write (OIDC)",
        ),
        (
            "    permissions: 'write-all' # all\n",
            "t.yml/mac: holds write-all",
        ),
        (
            "    permissions: { contents: write }\n",
            "t.yml/mac: permissions written as { contents: write }, a mapping the lint does not read",
        ),
    ] {
        let job = format!("  mac:\n    runs-on: macos-15\n{perms}    steps:\n      - run: true\n");
        assert_eq!(lint(&one("t.yml", &pr_workflow(&job))).1, [want], "{perms}");
    }
}

#[test]
fn a_secret_or_the_token_in_any_spelling_is_refused() {
    for line in [
        "KEY: ${{ secrets['SIGNING_KEY'] }}",
        "ALL: ${{ toJSON(secrets) }}",
        "ALL: ${{ secrets }}",
    ] {
        let w = pr_workflow(&mac_job(&format!("    env:\n      {line}\n")));
        assert_eq!(
            lint(&one("t.yml", &w)).1,
            ["t.yml/mac: the job names a secret"],
            "{line}"
        );
    }
    for line in [
        "T: ${{ github['token'] }}",
        "T: ${{ github[\"token\"] }}",
        "ALL: ${{ toJSON(github) }}",
        "T: ${{ github . token }}",
    ] {
        let w = pr_workflow(&mac_job(&format!("    env:\n      {line}\n")));
        assert_eq!(
            lint(&one("t.yml", &w)).1,
            ["t.yml/mac: the job names the job token"],
            "{line}"
        );
    }
    // A longer name that only holds the word is not a secret.
    let w = pr_workflow(&mac_job("    env:\n      NO_SECRETS_HERE: x\n"));
    assert!(lint(&one("t.yml", &w)).1.is_empty());
}

#[test]
fn a_runner_in_a_list_a_matrix_key_or_an_unreadable_expression_is_seen() {
    let secret = "    env:\n      KEY: ${{ secrets.KEY }}\n";
    let refused = ["t.yml/mac: the job names a secret"];
    for runs_on in [
        "    runs-on:\n      - macos-15\n",
        "    runs-on:\n      labels: macos-15\n",
        "    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix:\n        os: [ubuntu-24.04, macos-15]\n",
    ] {
        let job = format!(
            "  mac:\n{runs_on}    permissions:\n      contents: read\n{secret}    steps:\n      - run: true\n"
        );
        assert_eq!(
            lint(&one("t.yml", &pr_workflow(&job))).1,
            refused,
            "{runs_on}"
        );
    }
    for expr in [
        "${{ inputs.runner }}",
        "${{ vars.RUNNER }}",
        "${{ needs.pick.outputs.runner }}",
    ] {
        let job = format!("  mac:\n    runs-on: {expr}\n    steps:\n      - run: true\n");
        assert_eq!(
            lint(&one("t.yml", &pr_workflow(&job))).1,
            ["t.yml/mac: runs on a runner the lint cannot read"],
            "{expr}"
        );
    }
    // A matrix of Linux runners only is Linux.
    let job = "  lin:\n    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix:\n        os: [ubuntu-24.04, ubuntu-24.04-arm]\n    steps:\n      - run: echo ${{ secrets.K }}\n";
    assert_eq!(lint(&one("t.yml", &pr_workflow(job))), (vec![], vec![]));
}

#[test]
fn a_called_workflow_that_calls_another_is_refused() {
    let caller = pr_workflow("  call:\n    uses: ./.github/workflows/called.yml\n");
    let called = "name: c\non:\n  workflow_call:\njobs:\n  again:\n    uses: ./.github/workflows/inner.yml\n    secrets: inherit\n".to_owned();
    let workflows = vec![
        ("called.yml".to_owned(), called),
        ("t.yml".to_owned(), caller),
    ];
    assert_eq!(
        lint(&workflows).1,
        ["t.yml/call -> called.yml/again: calls a workflow again, which the lint does not follow"]
    );
}

#[test]
fn a_concurrency_group_that_differs_per_run_is_refused() {
    // The fallback after `||` (main's run) is not the pull request's group.
    let w = pr_workflow(&mac_job("")).replace(
        "github.event.pull_request.number || github.ref",
        "github.event.pull_request.number || github.run_id",
    );
    assert!(lint(&one("t.yml", &w)).1.is_empty());
    for per_run in [
        "github.run_id",
        "github.run_number",
        "github.run_attempt",
        "github.sha",
    ] {
        let w = pr_workflow(&mac_job("")).replace(
            "github.event.pull_request.number || github.ref",
            &format!("github.event.pull_request.number }}}}-${{{{ {per_run}"),
        );
        assert_eq!(
            lint(&one("t.yml", &w)).1,
            ["t.yml/mac: a newer push to the pull request does not cancel it"],
            "{per_run}"
        );
    }
}
