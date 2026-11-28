//! The gate before the farm: release.yml's job `blackbox`, which the two jobs that tell the farm
//! about a release need, read from the file as text. [`blackbox_problems`] names every way the
//! job differs from this, [`gate_problems`] every way the jobs around it do, and
//! [`suite_problems`] every way the root BUCK file's target does:
//!
//! - `blackbox` needs `plan`, `publish` and the black-box tests that are release jobs today
//!   ([`TESTS`]), and nothing else; its `if:` is [`BLACKBOX_IF`] alone: not cancelled, nothing up
//!   the chain failed, a push to main on the plan's release, and each job it needs succeeded, by
//!   name, so it runs only when every job it needs passed (a test that failed or was skipped holds
//!   it back). The results are named, not left to GitHub's implied `success()`, which is false
//!   when any job up the chain was skipped: the apt jobs' retry slots under `check-apt` are
//!   skipped on every release with no apt conflict (`skip_rule` runs the scenarios).
//! - It calls farm.yml ([`FARM`]) with the one input `target` = [`TARGET`], the Buck2 target of the
//!   black-box tests, which farm.yml's last step builds on the build farm in place of `//:ci`
//!   (`tests/apt_workflow`'s `farm_steps` holds that step). [`TARGET`] is named nowhere else in
//!   release.yml but its comments. It grants `contents: read` and `id-token: write` (farm.yml's
//!   job joins the build farm's network with the token) and passes no secret.
//! - `desired` and `cp-deploy-digest` ([`GATED`]) need `blackbox`; `desired`, whose `if:` has
//!   `!cancelled()`, also requires `needs.blackbox.result == 'success'`, and so does
//!   `cp-deploy-digest`'s, which starts `!cancelled() && !failure()` and has no `always()`.
//! - `release-red` and `release-green` need `blackbox`, so its result is reported, and
//!   `release-green` runs only when it passed: a run whose black-box tests were skipped deployed
//!   nothing, and closes no RELEASE-RED issue.
//! - The root BUCK file defines [`TARGET`] as a `test_suite` (each black-box test target joins its
//!   `test_deps`).

use std::path::Path;

use super::calls::calls;
use super::fast_lane::{code, ifs, needs, permissions, refused};
use super::{job, jobs, workflow};

/// The gate's job.
const GATE: &str = "blackbox";

/// The black-box tests the gate needs as release jobs: the install of the release's .debs from the
/// suite `testing`, and the first install of its macOS package.
const TESTS: [&str; 2] = ["check-apt", "beta-macos-first-install"];

/// The jobs that tell the farm about the release: the agent's desired version, and the image
/// digest the control-plane deploy of js-server is given.
const GATED: [&str; 2] = ["desired", "cp-deploy-digest"];

/// The `if:` of `blackbox`.
const BLACKBOX_IF: &str = "    if: ${{ !cancelled() && !failure() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.result == 'success' && needs.plan.outputs.release == 'true' && needs.publish.result == 'success' && needs.check-apt.result == 'success' && needs.beta-macos-first-install.result == 'success' }}";

/// The workflow `blackbox` calls: the build farm lane, which builds the target it is given.
const FARM: &str = "./.github/workflows/farm.yml";

/// The Buck2 target of the black-box tests.
const TARGET: &str = "//:blackbox";

/// What is wrong with the job `blackbox` of `text`.
fn blackbox_problems(text: &str) -> Vec<String> {
    let all = jobs(text);
    let Some((_, lines)) = all.iter().find(|(n, _)| n == GATE) else {
        return vec![format!("there is no job {GATE}")];
    };
    let mut problems = Vec::new();
    let mut want: Vec<String> = ["plan", "publish"]
        .into_iter()
        .chain(TESTS)
        .map(str::to_owned)
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
    if permissions(lines)
        != Some(vec![
            "contents: read".to_owned(),
            "id-token: write".to_owned(),
        ])
    {
        problems.push(format!(
            "{GATE} does not grant contents: read and id-token: write alone"
        ));
    }
    if calls(lines) != Some(FARM) {
        problems.push(format!("{GATE} does not call {FARM}"));
    }
    let body = code(lines);
    let with: Vec<&str> = body
        .lines()
        .skip_while(|l| *l != "    with:")
        .skip(1)
        .take_while(|l| l.starts_with("      "))
        .collect();
    let target = format!("      target: {TARGET}");
    if with != [target.as_str()] {
        problems.push(format!("{GATE} with {with:?}, not target: {TARGET} alone"));
    }
    if body.contains("secrets") {
        problems.push(format!("{GATE} passes a secret"));
    }
    let named = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && l.contains(TARGET))
        .count();
    if named != 1 {
        problems.push(format!(
            "{TARGET} is named {named} times outside comments, not once"
        ));
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
    let green = ifs(job(&all, "release-green"));
    if green.len() != 1 || !green[0].contains("needs.blackbox.result == 'success'") {
        problems.push("release-green closes issues of a run whose gate did not pass".to_owned());
    }
    let digest = ifs(job(&all, "cp-deploy-digest"));
    if digest.len() != 1
        || !digest[0].starts_with("    if: ${{ !cancelled() && !failure() && ")
        || digest[0].contains("always()")
        || !digest[0].contains(" && needs.blackbox.result == 'success'")
    {
        problems.push("cp-deploy-digest runs without blackbox's success".to_owned());
    }
    problems
}

/// The root BUCK file's text.
fn root_buck() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../BUCK");
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read BUCK: {e}"))
}

/// What is wrong with the root BUCK file (`text`): [`TARGET`] is not a `test_suite` there.
fn suite_problems(text: &str) -> Vec<String> {
    let name = TARGET.trim_start_matches("//:");
    if text.contains(&format!("\ntest_suite(\n    name = \"{name}\",\n")) {
        Vec::new()
    } else {
        vec![format!("BUCK defines no test_suite {TARGET}")]
    }
}

/// Asserts the shape of `blackbox`: it needs `publish` and the black-box test jobs, runs only when
/// all of them passed (each named by its result, after `!cancelled() && !failure()`), and calls
/// farm.yml with the one target named here alone; each change below is refused.
#[test]
fn blackbox_builds_the_black_box_target_on_the_farm_after_every_test_job_passed() {
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
        // Left to the implied `success()`, which a skipped retry slot up the chain makes false.
        ("if: ${{ !cancelled() && !failure() && ", "if: ${{ "),
        (
            "if: ${{ !cancelled() && !failure() && ",
            "if: ${{ !cancelled() && ",
        ),
        (
            "if: ${{ !cancelled() && !failure() && ",
            "if: ${{ always() && ",
        ),
        (
            " && needs.check-apt.result == 'success'",
            " && needs.check-apt.result != 'failure'",
        ),
        (" && needs.check-apt.result == 'success'", ""),
        (" && needs.publish.result == 'success'", ""),
        (" && needs.plan.result == 'success'", ""),
        (" && needs.beta-macos-first-install.result == 'success'", ""),
        (
            "github.event_name == 'push' && github.ref == 'refs/heads/main' && ",
            "",
        ),
        ("      id-token: write\n", ""),
        (
            "      id-token: write\n",
            "      id-token: write\n      actions: write\n",
        ),
        (
            "    uses: ./.github/workflows/farm.yml\n",
            "    uses: ./.github/workflows/beta.yml\n",
        ),
        ("      target: //:blackbox\n", "      target: //:ci\n"),
        (
            "      target: //:blackbox\n",
            "      target: //:blackbox\n      other: x\n",
        ),
        (
            "    uses: ./.github/workflows/farm.yml\n",
            "    uses: ./.github/workflows/farm.yml\n    secrets: inherit\n",
        ),
    ] {
        refused(blackbox_problems, &text, GATE, from, to);
    }
    assert_eq!(
        blackbox_problems(&text.replace("\n  blackbox:\n", "\n  gate:\n")),
        ["there is no job blackbox"]
    );
}

/// Asserts that the target is named in one place: a second mention outside the comments (another
/// job that would build or name the suite) is refused.
#[test]
fn the_black_box_target_is_named_in_one_place() {
    let text = workflow();
    let twice = text.replacen(
        "      target: //:blackbox\n",
        "      target: //:blackbox\n\n  other:\n    runs-on: ubuntu-24.04\n    env:\n      SUITE: //:blackbox\n    steps:\n      - run: echo \"$SUITE\"\n",
        1,
    );
    assert_ne!(twice, text);
    assert_eq!(
        blackbox_problems(&twice),
        ["//:blackbox is named 2 times outside comments, not once"]
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
            "if: ${{ !cancelled() && !failure() && ",
            "if: ${{ ",
        ),
        (
            "cp-deploy-digest",
            "if: ${{ !cancelled() && !failure() && ",
            "if: ${{ always() && !cancelled() && !failure() && ",
        ),
        (
            "cp-deploy-digest",
            " && needs.blackbox.result == 'success'",
            "",
        ),
        ("release-red", "promote-macos, blackbox,", "promote-macos,"),
        (
            "release-green",
            "promote-macos, blackbox,",
            "promote-macos,",
        ),
        (
            "release-green",
            " && needs.blackbox.result == 'success' }}",
            " }}",
        ),
    ] {
        refused(gate_problems, &text, name, from, to);
    }
}

/// Asserts that the root BUCK file defines the target `blackbox` builds as a `test_suite`, and
/// that a BUCK file without it is refused.
#[test]
fn the_black_box_target_is_a_test_suite_of_the_root_buck_file() {
    let buck = root_buck();
    assert_eq!(suite_problems(&buck), Vec::<String>::new());
    let renamed = buck.replace("    name = \"blackbox\",\n", "    name = \"black-box\",\n");
    assert_ne!(renamed, buck);
    assert_eq!(
        suite_problems(&renamed),
        ["BUCK defines no test_suite //:blackbox"]
    );
}
