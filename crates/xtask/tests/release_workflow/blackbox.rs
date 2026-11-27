//! The gate before the farm: release.yml's job `blackbox`, which the two jobs that tell the farm
//! about a release need, read from the file as text. [`blackbox_problems`] names every way the
//! job differs from this, and [`gate_problems`] every way the jobs around it do:
//!
//! - `blackbox` needs `plan`, `publish`, the black-box tests of today ([`TESTS`]) and every job
//!   named `blackbox-<test>`, and nothing else; it runs under [`BLACKBOX_IF`] alone: a push to
//!   main on the plan's release once `publish` passed, whatever the tests did (`!cancelled()`), so
//!   a test that failed, was skipped or was cancelled fails it rather than skip it.
//! - It holds no permission, runs on `ubuntu-24.04`, checks out and builds nothing, names no
//!   secret or variable, reads the results of what it needs from `NEEDS` ([`NEEDS_ENV`]) and runs
//!   exactly two steps: [`SHOW`] (each job's result, for the log) and [`PASS`] (fails unless every
//!   one is `success`).
//! - Every `blackbox-<test>` job runs for a push to main alone.
//! - `desired` and `cp-deploy-digest` ([`GATED`]) need `blackbox`; `desired`, whose `if:` has
//!   `!cancelled()`, also requires `needs.blackbox.result == 'success'`, and `cp-deploy-digest`'s
//!   `if:` has no status function, so GitHub's implied `success()` requires it.
//! - `release-red` and `release-green` need `blackbox`, so its result is reported.

use super::fast_lane::{code, ifs, needs, permissions, refused, runs_no_repository_code, steps};
use super::{job, jobs, workflow};

/// The gate's job.
const GATE: &str = "blackbox";

/// The black-box tests the gate needs besides the `blackbox-<test>` jobs: the install of the
/// release's .debs from the suite `testing`, and the first install of its macOS package.
const TESTS: [&str; 2] = ["check-apt", "beta-macos-first-install"];

/// The jobs that tell the farm about the release: the agent's desired version, and the image
/// digest the control-plane deploy of js-server is given.
const GATED: [&str; 2] = ["desired", "cp-deploy-digest"];

/// The `if:` of `blackbox`.
const BLACKBOX_IF: &str = "    if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' && needs.publish.result == 'success' }}";

/// The condition of a job that runs for a push to main alone.
const PUSH: &str = "github.event_name == 'push' && github.ref == 'refs/heads/main'";

/// The job's `env:` entry that hands its steps the results of the jobs it needs.
const NEEDS_ENV: &str = "      NEEDS: ${{ toJSON(needs) }}";

/// The step that prints each needed job's result.
const SHOW: &str =
    "        run: jq -r 'to_entries[] | \"\\(.key) \\(.value.result)\"' <<< \"$NEEDS\"";

/// The step that fails unless every needed job passed.
const PASS: &str = "        run: jq -e 'all(.[]; .result == \"success\")' <<< \"$NEEDS\"";

/// The names of the jobs of `text` named `blackbox-<test>`.
fn test_jobs(text: &str) -> Vec<String> {
    jobs(text)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("blackbox-"))
        .collect()
}

/// What is wrong with the job `blackbox` of `text`.
fn blackbox_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let Some((_, lines)) = all.iter().find(|(n, _)| n == GATE) else {
        return vec![format!("there is no job {GATE}")];
    };
    let mut problems = runs_no_repository_code(GATE, lines);
    let mut want: Vec<String> = ["plan", "publish"]
        .into_iter()
        .chain(TESTS)
        .map(str::to_owned)
        .chain(test_jobs(text))
        .collect();
    want.sort();
    let mut have = needs(lines);
    have.sort();
    if have != want {
        problems.push(format!("{GATE} needs {have:?}, not {want:?}"));
    }
    if ifs(lines) != [BLACKBOX_IF] {
        problems.push(format!("{GATE} if {:?}", ifs(lines)));
    }
    if permissions(lines) != Some(Vec::new()) {
        problems.push(format!("{GATE} holds a permission"));
    }
    if !lines.iter().any(|l| l == "    runs-on: ubuntu-24.04") {
        problems.push(format!("{GATE} does not run on ubuntu-24.04"));
    }
    let body = code(lines);
    for name in ["secrets.", "vars.", "github.token"] {
        if body.contains(name) {
            problems.push(format!("{GATE} names {name}"));
        }
    }
    if !lines.iter().any(|l| l == NEEDS_ENV) {
        problems.push(format!("{GATE} does not read NEEDS from toJSON(needs)"));
    }
    let runs: Vec<&String> = lines
        .iter()
        .filter(|l| l.trim_start().starts_with("run:"))
        .collect();
    if steps(lines) != 2 || runs != [SHOW, PASS] {
        problems.push(format!("{GATE} steps are not the two jq steps: {runs:?}"));
    }
    if body.contains("continue-on-error") {
        problems.push(format!(
            "{GATE} has a step that may fail without failing the job (continue-on-error)"
        ));
    }
    for name in test_jobs(text) {
        let condition = ifs(job(&all, &name));
        if condition.len() != 1 || !condition[0].contains(PUSH) {
            problems.push(format!("{name} runs for more than a push to main"));
        }
    }
    problems
}

/// What is wrong with the jobs around `blackbox`: the two that tell the farm, and the reports.
fn gate_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let mut problems = Vec::new();
    for name in GATED.into_iter().chain(["release-red", "release-green"]) {
        if !needs(job(&all, name)).iter().any(|n| n == GATE) {
            problems.push(format!("{name} does not need {GATE}"));
        }
    }
    let desired = ifs(job(&all, "desired"));
    if desired.len() != 1 || !desired[0].contains("needs.blackbox.result == 'success'") {
        problems.push("desired runs without blackbox's success".to_owned());
    }
    let digest = ifs(job(&all, "cp-deploy-digest"));
    if digest.len() != 1
        || ["always()", "!cancelled()", "failure()", "success()"]
            .iter()
            .any(|f| digest[0].contains(f))
    {
        problems.push("cp-deploy-digest has a status function".to_owned());
    }
    problems
}

/// Asserts the shape of `blackbox`: it needs `publish`, the black-box tests and every
/// `blackbox-<test>` job, runs once `publish` passed whatever the tests did, holds nothing, and
/// passes only when every job it needs passed; each change below is refused.
#[test]
fn blackbox_needs_every_black_box_test_and_passes_only_when_all_passed() {
    let text = workflow();
    for (from, to) in [
        (
            "needs: [plan, publish, check-apt, beta-macos-first-install]",
            "needs: [plan, publish, check-apt]",
        ),
        (
            "needs: [plan, publish, check-apt, beta-macos-first-install]",
            "needs: [plan, publish, beta-macos-first-install]",
        ),
        (
            "needs: [plan, publish, check-apt, beta-macos-first-install]",
            "needs: [plan, check-apt, beta-macos-first-install]",
        ),
        (
            "needs: [plan, publish, check-apt, beta-macos-first-install]",
            "needs: [plan, publish, check-apt, beta-macos-first-install, beta-macos]",
        ),
        ("if: ${{ !cancelled() && ", "if: ${{ "),
        ("if: ${{ !cancelled() && ", "if: ${{ always() && "),
        (" && needs.publish.result == 'success' }}", " }}"),
        (
            "github.event_name == 'push' && github.ref == 'refs/heads/main' && ",
            "",
        ),
        (
            "    permissions: {}\n",
            "    permissions:\n      contents: read\n",
        ),
        (
            "    runs-on: ubuntu-24.04\n",
            "    runs-on: ubuntu-latest\n",
        ),
        (
            "NEEDS: ${{ toJSON(needs) }}",
            "NEEDS: ${{ toJSON(needs.check-apt) }}",
        ),
        (
            "jq -e 'all(.[]; .result == \"success\")'",
            "jq 'all(.[]; .result == \"success\")'",
        ),
        (
            "jq -e 'all(.[]; .result == \"success\")'",
            "jq -e 'all(.[]; .result != \"failure\")'",
        ),
        (
            "jq -e 'all(.[]; .result == \"success\")'",
            "jq -e 'any(.[]; .result == \"success\")'",
        ),
        (
            "      - name: Pass only when every one passed\n",
            "      - name: Pass only when every one passed\n        continue-on-error: true\n",
        ),
        (
            "      - name: Show the result",
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n      - name: Show the result",
        ),
        (
            "    env:\n      NEEDS:",
            "    env:\n      TOKEN: ${{ secrets.ANY }}\n      NEEDS:",
        ),
    ] {
        refused(blackbox_problems, &text, GATE, from, to);
    }
    assert_eq!(
        blackbox_problems(&text.replace("\n  blackbox:\n", "\n  gate:\n")),
        ["there is no job blackbox"]
    );
}

/// The text with a job `name` added before `desired`, needing `plan` and `publish`, whose `if:` is
/// `condition`.
fn with_test_job(text: &str, name: &str, condition: &str) -> String {
    let added = format!(
        "  {name}:\n    needs: [plan, publish]\n    if: {condition}\n    runs-on: ubuntu-24.04\n    permissions: {{}}\n    steps:\n      - run: echo test\n\n  desired:\n"
    );
    text.replacen("\n  desired:\n", &format!("\n{added}"), 1)
}

/// Asserts that a black-box test added as a job `blackbox-<test>` must be needed by `blackbox`
/// (a test it does not need would gate nothing) and must run for a push to main alone, and that
/// one it needs is accepted.
#[test]
fn a_blackbox_test_job_is_needed_by_the_gate_and_runs_on_a_push_to_main() {
    let text = workflow();
    let push_if = format!(
        "${{{{ {PUSH} && needs.plan.outputs.release == 'true' && needs.publish.result == 'success' }}}}"
    );
    let added = with_test_job(&text, "blackbox-released-deb", &push_if);
    assert_eq!(
        blackbox_problems(&added),
        [
            "blackbox needs [\"beta-macos-first-install\", \"check-apt\", \"plan\", \"publish\"], not [\"beta-macos-first-install\", \"blackbox-released-deb\", \"check-apt\", \"plan\", \"publish\"]"
        ]
    );
    let needed = added.replacen(
        "needs: [plan, publish, check-apt, beta-macos-first-install]",
        "needs: [plan, publish, check-apt, beta-macos-first-install, blackbox-released-deb]",
        1,
    );
    assert_eq!(blackbox_problems(&needed), Vec::<String>::new());
    let pull_request = needed.replacen(&push_if, "${{ github.event_name == 'pull_request' }}", 1);
    assert_eq!(
        blackbox_problems(&pull_request),
        ["blackbox-released-deb runs for more than a push to main"]
    );
}

/// Asserts that the two jobs that tell the farm need `blackbox` and run only when it passed, and
/// that the reports need it; each change below is refused.
#[test]
fn the_farm_is_told_only_after_blackbox_passed() {
    let text = workflow();
    for (name, from, to) in [
        (
            "desired",
            "needs: [plan, turn, pkg-upload, blackbox,",
            "needs: [plan, turn, pkg-upload,",
        ),
        ("desired", " && needs.blackbox.result == 'success'", ""),
        (
            "desired",
            " && needs.blackbox.result == 'success'",
            " && needs.blackbox.result != 'cancelled'",
        ),
        (
            "cp-deploy-digest",
            "needs: [plan, publish, deploy-tool, blackbox]",
            "needs: [plan, publish, deploy-tool]",
        ),
        (
            "cp-deploy-digest",
            "if: ${{ github.event_name",
            "if: ${{ !cancelled() && github.event_name",
        ),
        (
            "cp-deploy-digest",
            "if: ${{ github.event_name",
            "if: ${{ always() && github.event_name",
        ),
        ("release-red", "promote-macos, blackbox,", "promote-macos,"),
        (
            "release-green",
            "promote-macos, blackbox,",
            "promote-macos,",
        ),
    ] {
        refused(gate_problems, &text, name, from, to);
    }
}
