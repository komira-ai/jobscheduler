//! The negative and edge cases of the lint, over workflows written in the test.

use crate::lint::lint;

/// One workflow written here.
fn one(file: &str, text: &str) -> Vec<(String, String)> {
    vec![(file.to_owned(), text.to_owned())]
}

/// A pull request workflow with `jobs` appended.
fn pr_workflow(jobs: &str) -> String {
    format!("name: t\non:\n  pull_request:\npermissions: {{}}\njobs:\n{jobs}")
}

/// A job named `j` that runs on `runner`, with `extra` lines in its block (four spaces).
fn job(runner: &str, extra: &str) -> String {
    format!("  j:\n    runs-on: {runner}\n{extra}    steps:\n      - run: cargo test\n")
}

fn problems(w: &str) -> Vec<String> {
    lint(&one("t.yml", w)).1
}

#[test]
fn a_pull_request_job_on_macos_is_refused() {
    assert_eq!(
        problems(&pr_workflow(&job("macos-15", ""))),
        ["t.yml/j: runs on macos-15, not Linux x86_64 (ubuntu-*)"]
    );
}

#[test]
fn a_pull_request_job_on_arm64_windows_or_a_custom_runner_is_refused() {
    for runner in [
        "ubuntu-24.04-arm",
        "ubuntu-22.04-arm",
        "windows-latest",
        "macos-latest",
        "macOS-15",
        "self-hosted",
        "ubuntu-latest-4-cores",
        "ubuntu-24.04-large",
        "ubuntu",
        "ubuntu-",
        "ubuntu-2404",
        "ubuntu-24.4",
        "ubuntu-xx.04",
        "my-runner",
    ] {
        assert_eq!(
            problems(&pr_workflow(&job(runner, ""))),
            [format!(
                "t.yml/j: runs on {runner}, not Linux x86_64 (ubuntu-*)"
            )],
            "{runner}"
        );
    }
}

#[test]
fn a_pull_request_job_on_a_hosted_ubuntu_x86_64_runner_passes() {
    for runner in [
        "ubuntu-latest",
        "ubuntu-24.04",
        "ubuntu-22.04",
        "'ubuntu-24.04'",
    ] {
        let (jobs, problems) = lint(&one("t.yml", &pr_workflow(&job(runner, ""))));
        assert_eq!(jobs, ["t.yml/j"], "{runner}");
        assert!(problems.is_empty(), "{runner}");
    }
}

#[test]
fn every_spelling_of_the_trigger_is_seen() {
    for on in [
        "on: pull_request\n",
        "on: [push, pull_request]\n",
        "on:\n  push:\n    branches: [main]\n  pull_request_target:\n",
        "on:\n  pull_request:\n    paths:\n      - crates/**\n",
    ] {
        let w = pr_workflow(&job("macos-15", "")).replace("on:\n  pull_request:\n", on);
        assert_eq!(
            problems(&w),
            ["t.yml/j: runs on macos-15, not Linux x86_64 (ubuntu-*)"],
            "{on}"
        );
    }
}

#[test]
fn a_runner_in_a_list_a_mapping_a_matrix_or_an_unreadable_expression_is_seen() {
    let mac = ["t.yml/j: runs on macos-15, not Linux x86_64 (ubuntu-*)"];
    for runs_on in [
        "    runs-on:\n      - macos-15\n",
        "    runs-on:\n      - ubuntu-24.04\n      - macos-15\n",
        "    runs-on: [ubuntu-24.04, macos-15]\n",
        "    runs-on:\n      labels: macos-15\n",
        "    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix:\n        os: [ubuntu-24.04, macos-15]\n",
        "    runs-on: ${{ matrix.runner }}\n    strategy:\n      matrix:\n        include:\n          - runner: ubuntu-24.04\n          - runner: macOS-15\n",
    ] {
        let w = pr_workflow(&format!("  j:\n{runs_on}    steps:\n      - run: true\n"));
        let got = problems(&w);
        if runs_on.contains("macOS-15") {
            assert_eq!(
                got,
                ["t.yml/j: runs on macOS-15, not Linux x86_64 (ubuntu-*)"],
                "{runs_on}"
            );
        } else {
            assert_eq!(got, mac, "{runs_on}");
        }
    }
    for expr in [
        "${{ inputs.runner }}",
        "${{ vars.RUNNER }}",
        "${{ needs.pick.outputs.runner }}",
        "${{ github.event_name == 'push' && 'macos-15' || 'ubuntu-24.04' }}",
        "${{ matrix.missing }}",
    ] {
        assert_eq!(
            problems(&pr_workflow(&job(expr, ""))),
            ["t.yml/j: runs on a runner the lint cannot read"],
            "{expr}"
        );
    }
    // A matrix of Linux x86_64 runners only is fine, in a flow list or a block list.
    for matrix in [
        "        os: [ubuntu-24.04, ubuntu-22.04]\n",
        "        include:\n          - os: ubuntu-24.04\n          - os: ubuntu-latest\n",
    ] {
        let w = pr_workflow(&format!(
            "  j:\n    runs-on: ${{{{ matrix.os }}}}\n    strategy:\n      matrix:\n{matrix}    steps:\n      - run: true\n"
        ));
        assert!(problems(&w).is_empty(), "{matrix}");
    }
    // The arm64 leg of a matrix is refused.
    let w = pr_workflow(
        "  j:\n    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix:\n        os: [ubuntu-24.04, ubuntu-24.04-arm]\n    steps:\n      - run: true\n",
    );
    assert_eq!(
        problems(&w),
        ["t.yml/j: runs on ubuntu-24.04-arm, not Linux x86_64 (ubuntu-*)"]
    );
}

#[test]
fn jobs_off_pull_requests_are_not_this_lint_s() {
    // A macOS job in a workflow a pull request does not start.
    for on in ["workflow_dispatch", "push", "workflow_call", "schedule"] {
        let w =
            pr_workflow(&job("macos-15", "")).replace("  pull_request:\n", &format!("  {on}:\n"));
        assert_eq!(lint(&one("t.yml", &w)), (vec![], vec![]), "{on}");
    }
    // A macOS job whose `if:` pins another event, in each spelling.
    for cond in [
        "${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' }}",
        "${{ github.event_name == 'push' }}",
        "${{ !cancelled() && github.event_name != 'pull_request' }}",
        "github.event_name == \"schedule\"",
    ] {
        let w = pr_workflow(&job("macos-15", &format!("    if: {cond}\n")));
        assert_eq!(lint(&one("t.yml", &w)), (vec![], vec![]), "{cond}");
    }
}

#[test]
fn an_if_that_does_not_pin_another_event_keeps_the_job_on_pull_requests() {
    for cond in [
        "${{ github.event_name == 'pull_request' }}",
        "${{ github.event_name == 'pull_request_target' }}",
        "${{ github.event_name == 'push' || github.event_name == 'pull_request' }}",
        "${{ !cancelled() && (github.event_name != 'pull_request' || needs.changes.outputs.macos == 'true') }}",
        "${{ always() }}",
    ] {
        let w = pr_workflow(&job("macos-15", &format!("    if: {cond}\n")));
        assert_eq!(
            problems(&w),
            ["t.yml/j: runs on macos-15, not Linux x86_64 (ubuntu-*)"],
            "{cond}"
        );
    }
}

#[test]
fn a_called_workflow_s_macos_job_is_refused_unless_the_call_pins_another_event() {
    let called = "name: c\non:\n  workflow_call:\njobs:\n  mac:\n    runs-on: macos-15\n    steps:\n      - run: true\n  lin:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n  later:\n    if: ${{ github.event_name == 'push' }}\n    runs-on: ubuntu-24.04-arm\n    steps:\n      - run: true\n";
    let call = |extra: &str| {
        vec![
            ("called.yml".to_owned(), called.to_owned()),
            (
                "t.yml".to_owned(),
                pr_workflow(&format!(
                    "  call:\n{extra}    uses: ./.github/workflows/called.yml\n"
                )),
            ),
        ]
    };
    let (jobs, problems) = lint(&call(""));
    assert_eq!(
        jobs,
        [
            "t.yml/call -> called.yml/lin",
            "t.yml/call -> called.yml/mac"
        ]
    );
    assert_eq!(
        problems,
        ["t.yml/call -> called.yml/mac: runs on macos-15, not Linux x86_64 (ubuntu-*)"]
    );
    // The call pins a push: none of the called jobs is a pull request job.
    assert_eq!(
        lint(&call("    if: ${{ github.event_name == 'push' }}\n")),
        (vec![], vec![])
    );
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
fn a_called_workflow_that_calls_another_is_refused() {
    let caller = pr_workflow("  call:\n    uses: ./.github/workflows/called.yml\n");
    let called = "name: c\non:\n  workflow_call:\njobs:\n  again:\n    uses: ./.github/workflows/inner.yml\n".to_owned();
    let workflows = vec![
        ("called.yml".to_owned(), called),
        ("t.yml".to_owned(), caller),
    ];
    assert_eq!(
        lint(&workflows).1,
        ["t.yml/call -> called.yml/again: calls a workflow again, which the lint does not follow"]
    );
}
