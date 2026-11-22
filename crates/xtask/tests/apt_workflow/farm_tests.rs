//! The rules of `farm.yml` ([`super::farm`]), the farm lane: the one job that holds
//! `id-token: write` on a pull request, and only under its exact `if:`; farm-connect pinned by
//! commit with exactly the variables it is given; the triggers, the runner, the rights, and the
//! build through `cargo xtask farm-build` after farm-connect; and no other workflow's job holding
//! the token on a pull request. Each rule is shown to refuse its defect by putting it into the
//! committed workflow and reading the exact problems the lint reports.

use super::farm::{
    BUILD_RUN, CONNECT_INPUTS, FARM_CONNECT, FILE, JOB_IF, ON_PROBLEM, PATCH_ARTIFACT, RUNNER,
    TOP_PERMISSIONS_PROBLEM, UPLOAD_IF, patch_file, problems, token_problems, upload_inputs,
};
use super::support::{text_of, workflows};
use super::yaml;

/// The problems of the farm.yml text `text`; a text that is not YAML is its own problem.
fn problems_of(text: &str) -> Vec<String> {
    match yaml::load(text) {
        Ok(doc) => problems(&doc),
        Err(e) => vec![e],
    }
}

/// The committed farm.yml with its one occurrence of `from` replaced by `to`.
fn with(from: &str, to: &str) -> String {
    let text = text_of(FILE);
    assert_eq!(text.matches(from).count(), 1, "{from:?} once in farm.yml");
    text.replace(from, to)
}

/// The problems of the committed farm.yml with `from` replaced by `to`.
fn problems_with(from: &str, to: &str) -> Vec<String> {
    problems_of(&with(from, to))
}

/// The `uses:` value of farm-connect in the committed file.
fn pinned() -> String {
    let text = text_of(FILE);
    let start = text.find(FARM_CONNECT).expect("farm-connect is used");
    text[start..].lines().next().expect("its line").to_owned()
}

/// The problem of the permissions of the job `build` that are not exactly its two.
const PERMISSIONS: &str =
    "job build: permissions must be exactly contents: read and id-token: write";

/// The problem of a job that holds the token under another `if:` than its exact one.
fn needs_the_if() -> String {
    format!("job build: `id-token: write` needs the exact `if: {JOB_IF}`")
}

/// The repository's workflows with the one occurrence of `from` in `file` replaced by `to`.
fn changed(file: &str, from: &str, to: &str) -> Vec<(String, String)> {
    let mut all = workflows();
    let found = all
        .iter_mut()
        .find(|(name, _)| name == file)
        .unwrap_or_else(|| panic!("{file}"));
    assert_eq!(found.1.matches(from).count(), 1, "{from:?} once in {file}");
    found.1 = found.1.replace(from, to);
    all
}

/// A workflow that starts on `on` (a line of YAML) with one job holding `permissions` under `job_if`.
fn workflow(on: &str, permissions: &str, job_if: &str) -> (String, String) {
    (
        "new.yml".to_owned(),
        format!(
            "name: new\n{on}\njobs:\n  token:\n    runs-on: ubuntu-24.04\n{job_if}    permissions: {permissions}\n    steps:\n      - run: true\n"
        ),
    )
}

#[test]
fn the_committed_farm_yml_passes_every_rule() {
    // Catches: a farm.yml that the rules below would refuse, or rules that refuse the file they
    // were written for.
    assert_eq!(problems_of(&text_of(FILE)), Vec::<String>::new());
}

#[test]
fn the_token_is_held_only_under_the_jobs_exact_if() {
    // Catches: the job's `if:` loosened (a fork's pull request let in, Dependabot's branches let
    // in, the condition dropped) while it still holds `id-token: write`.
    assert_eq!(
        problems_with(&format!("    if: {JOB_IF}\n"), ""),
        [needs_the_if()],
        "no if:"
    );
    for condition in [
        "${{ github.event_name == 'push' || github.event_name == 'pull_request' }}",
        "${{ github.event_name == 'push' || github.event.pull_request.head.repo.full_name == github.repository }}",
        "${{ always() }}",
    ] {
        assert_eq!(
            problems_with(JOB_IF, condition),
            [needs_the_if()],
            "{condition}"
        );
    }
}

#[test]
fn a_renamed_job_and_a_second_job_are_refused() {
    // Catches: the job renamed (the rules would follow no job), and a job added beside `build`,
    // which the other rules would let through.
    assert_eq!(
        problems_of(&with("\njobs:\n  build:\n", "\njobs:\n  lane:\n")),
        [
            "farm.yml: no job build".to_owned(),
            "job lane is not a job of farm.yml".to_owned(),
        ]
    );
    assert_eq!(
        problems_with(
            "\njobs:\n",
            "\njobs:\n  other:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n"
        ),
        ["job other is not a job of farm.yml"]
    );
}

#[test]
fn no_other_scope_is_held_by_the_job_or_the_file() {
    // Catches: a second scope beside the token, the token's companion widened, every scope granted
    // at once, and a scope given to every job at the top of the file.
    assert_eq!(
        problems_with("      contents: read\n", "      contents: write\n"),
        [PERMISSIONS]
    );
    assert_eq!(
        problems_with(
            "      contents: read\n",
            "      contents: read\n      packages: read\n"
        ),
        [PERMISSIONS]
    );
    assert_eq!(
        problems_with(
            "    permissions:\n      contents: read\n      # For farm-connect's tailnet join alone: the OIDC token the farm's network trusts.\n      id-token: write\n",
            "    permissions: write-all\n",
        ),
        [PERMISSIONS]
    );
    assert_eq!(
        problems_with("\npermissions: {}\n", "\npermissions:\n  contents: read\n"),
        [TOP_PERMISSIONS_PROBLEM]
    );
    assert_eq!(
        problems_with("\npermissions: {}\n", "\n"),
        [TOP_PERMISSIONS_PROBLEM]
    );
}

#[test]
fn farm_connect_is_pinned_by_a_full_lowercase_commit() {
    // Catches: farm-connect at a branch, a short commit or a commit in capitals.
    let pin = pinned();
    let (_, sha) = pin.split_once('@').expect("a ref");
    let says =
        format!("job build: {FARM_CONNECT} must be pinned to a 40-digit lowercase commit SHA");
    let capitals = sha.to_uppercase();
    for at in ["main", &sha[..7], capitals.as_str()] {
        assert_eq!(
            problems_with(&pin, &format!("{FARM_CONNECT}@{at}")),
            std::slice::from_ref(&says),
            "{at}"
        );
    }
}

#[test]
fn exactly_one_step_uses_farm_connect() {
    // Catches: the join replaced by another action (whose name only starts like farm-connect's),
    // or a second join with other inputs beside the first.
    let pin = pinned();
    let lookalike = pin.replace(FARM_CONNECT, &format!("{FARM_CONNECT}-v2"));
    assert_eq!(
        problems_with(&pin, &lookalike),
        [
            format!("job build: exactly one step must use {FARM_CONNECT}"),
            "job build: the build does not come after the farm-connect step".to_owned(),
        ]
    );
    let twice = with(
        "      - name: Connect to the build farm\n",
        &format!(
            "      - uses: {pin}\n        with:\n          farm-address: elsewhere\n      - name: Connect to the build farm\n"
        ),
    );
    assert_eq!(
        problems_of(&twice),
        [format!(
            "job build: exactly one step must use {FARM_CONNECT}"
        )]
    );
}

#[test]
fn farm_connect_takes_exactly_its_variables() {
    // Catches: an input added (a secret, the token), one taken from pull-request data, one
    // missing, or one that is not a plain value.
    let inputs =
        format!("job build: the farm-connect step's `with:` must be exactly {CONNECT_INPUTS:?}");
    let tls = "          tls: ${{ vars.FARM_TLS || 'false' }}\n";
    for changed in [
        format!("{tls}          token: ${{{{ secrets.GITHUB_TOKEN }}}}\n"),
        "          tls: ${{ github.event.pull_request.title }}\n".to_owned(),
        String::new(),
        "          tls: [true]\n".to_owned(),
    ] {
        assert_eq!(
            problems_with(tls, &changed),
            std::slice::from_ref(&inputs),
            "{changed:?}"
        );
    }
}

#[test]
fn the_job_runs_farm_build_of_ci_after_farm_connect() {
    // Catches: a build of another target, a plain `buck2 build` that checks no `local: 0`, or the
    // build moved before the join (it would find no farm).
    let no_build = format!("job build: no step runs `{BUILD_RUN}`");
    for other in [
        BUILD_RUN.replace("//:ci", "//:lint"),
        "buck2 build //:ci".to_owned(),
    ] {
        assert_eq!(
            problems_with(BUILD_RUN, &other),
            std::slice::from_ref(&no_build),
            "{other}"
        );
    }
    let build = format!(
        "      - name: buck2 build //:ci on the build farm, local 0\n        run: {BUILD_RUN}\n"
    );
    let moved = with(&build, "").replace(
        "      - name: Connect to the build farm\n",
        &format!("{build}      - name: Connect to the build farm\n"),
    );
    assert_eq!(
        problems_of(&moved),
        ["job build: the build does not come after the farm-connect step"]
    );
}

/// The upload step of the committed file.
const UPLOAD_STEP: &str = "      - name: Upload the regenerated build table as a patch
        if: ${{ failure() }}
        uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: buildrules-sync
          path: ${{ runner.temp }}/buildrules-sync/buildrules-sync.patch
          if-no-files-found: ignore
          retention-days: 30
";

#[test]
fn the_build_writes_the_patch_the_upload_takes() {
    // Catches: the build step writing the patch to one file and the upload taking another, so a
    // stale table's summary names an artifact that holds nothing.
    assert!(BUILD_RUN.contains(&format!("--patch \"{}\"", patch_file(true))));
    assert_eq!(
        upload_inputs().get("path").map(String::as_str),
        Some(patch_file(false).as_str())
    );
    assert_eq!(
        patch_file(false),
        "${{ runner.temp }}/buildrules-sync/buildrules-sync.patch"
    );
    assert_eq!(
        patch_file(true),
        "$RUNNER_TEMP/buildrules-sync/buildrules-sync.patch"
    );
}

#[test]
fn the_patch_is_uploaded_once_after_a_failed_build_under_its_name() {
    // Catches: no upload of the patch, two of them, one under another artifact name (the summary's
    // command would download nothing), one before the build (there is no patch yet), one that
    // runs on a passing build only or always, and one of another file or kept another time.
    let none = format!(
        "job build: exactly one step must upload the artifact {PATCH_ARTIFACT} with \
         actions/upload-artifact@<commit>"
    );
    assert_eq!(problems_with(UPLOAD_STEP, ""), std::slice::from_ref(&none));
    assert_eq!(
        problems_with(UPLOAD_STEP, &UPLOAD_STEP.repeat(2)),
        std::slice::from_ref(&none)
    );
    assert_eq!(
        problems_with("name: buildrules-sync\n", "name: patch\n"),
        std::slice::from_ref(&none)
    );
    assert_eq!(
        problems_with(
            "uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
            "uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9"
        ),
        std::slice::from_ref(&none)
    );
    let moved = with(UPLOAD_STEP, "").replace(
        "      - name: Connect to the build farm\n",
        &format!("{UPLOAD_STEP}      - name: Connect to the build farm\n"),
    );
    assert_eq!(
        problems_of(&moved),
        [format!(
            "job build: the upload of {PATCH_ARTIFACT} does not come after the build"
        )]
    );
    let wrong_if = format!(
        "job build: the upload of {PATCH_ARTIFACT} must run under exactly `if: {UPLOAD_IF}`"
    );
    for to in [
        "        if: ${{ always() }}\n",
        "        if: ${{ success() }}\n",
        "",
    ] {
        assert_eq!(
            problems_with("        if: ${{ failure() }}\n", to),
            std::slice::from_ref(&wrong_if),
            "{to:?}"
        );
    }
    let wanted = upload_inputs();
    let wanted: std::collections::BTreeMap<&str, &str> =
        wanted.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let wrong_with =
        format!("job build: the upload of {PATCH_ARTIFACT}'s `with:` must be exactly {wanted:?}");
    for (from, to) in [
        (
            "path: ${{ runner.temp }}/buildrules-sync/buildrules-sync.patch",
            "path: ${{ runner.temp }}/buildrules-sync/",
        ),
        ("if-no-files-found: ignore", "if-no-files-found: error"),
        ("retention-days: 30", "retention-days: 90"),
        ("          retention-days: 30\n", ""),
    ] {
        assert_eq!(
            problems_with(from, to),
            std::slice::from_ref(&wrong_with),
            "{to:?}"
        );
    }
}

#[test]
fn the_triggers_are_every_pull_request_and_a_push_to_main() {
    // Catches: a filtered pull request, a push of another branch or of more branches, a push by
    // tags, another trigger beside them, `branches` as a scalar, and the list form of `on`.
    let on = "on:\n  pull_request:\n  push:\n    branches: [main]\n";
    for other in [
        "on:\n  pull_request:\n    paths: [crates/**]\n  push:\n    branches: [main]\n",
        "on:\n  pull_request:\n  push:\n    branches: [dev]\n",
        "on:\n  pull_request:\n  push:\n    branches: [main, dev]\n",
        "on:\n  pull_request:\n  push:\n    branches: main\n",
        "on:\n  pull_request:\n  push:\n    tags: ['v*']\n",
        "on:\n  pull_request:\n  push:\n    branches: [main]\n    paths: [crates/**]\n",
        "on:\n  pull_request:\n  push:\n    branches: [main]\n  workflow_dispatch:\n",
        "on: [pull_request, push]\n",
        "on: push\n",
    ] {
        assert_eq!(problems_with(on, other), [ON_PROBLEM], "{other}");
    }
}

#[test]
fn the_job_runs_on_its_one_runner() {
    // Catches: the job moved to another runner label, even another Linux x86_64 one.
    assert_eq!(
        problems_with(
            &format!("    runs-on: {RUNNER}\n"),
            "    runs-on: ubuntu-latest\n"
        ),
        [format!("job build: runs-on must be {RUNNER}")]
    );
}

#[test]
fn no_other_workflow_holds_the_token_on_a_pull_request_but_under_a_push() {
    // Catches: a token grant the farm lane's exception would be copied into, in a workflow that a
    // pull request starts: the committed ones hold none outside a push.
    assert_eq!(token_problems(&workflows()), Vec::<String>::new());
    let released = "if: ${{ !cancelled() && github.event_name == 'push' && needs.plan.outputs.release == 'true' }}";
    assert_eq!(
        token_problems(&changed(
            "release.yml",
            released,
            "if: ${{ !cancelled() && needs.plan.outputs.release == 'true' }}"
        )),
        [
            "release.yml: job artifacts holds `id-token: write` on a pull request (its `if:` does not pin a push)"
        ]
    );
    assert_eq!(
        token_problems(&changed(
            "release.yml",
            "\npermissions: {}\n",
            "\npermissions:\n  id-token: write\n"
        )),
        [
            "release.yml: holds `id-token: write` at its top level, for every job a pull request starts"
        ]
    );
}

#[test]
fn a_new_workflow_that_a_pull_request_starts_may_not_hold_the_token_without_a_push() {
    // Catches: the grant spelled as `write-all`, or in a workflow on `pull_request_target` or on a
    // merge group, or in the list form of `on`; and that a workflow no pull request starts, or a
    // job that pins a push, is left alone.
    let refused = "new.yml: job token holds `id-token: write` on a pull request (its `if:` does not pin a push)";
    let push_only = "    if: ${{ github.event_name == 'push' }}\n";
    for on in [
        "on: pull_request",
        "on: [push, pull_request_target]",
        "on:\n  pull_request:\n    paths: [a]",
        "on:\n  merge_group:\n    types: [checks_requested]",
    ] {
        for grant in ["{ id-token: write }", "write-all"] {
            assert_eq!(
                token_problems(&[workflow(on, grant, "")]),
                [refused],
                "{on} {grant}"
            );
            assert_eq!(
                token_problems(&[workflow(on, grant, push_only)]),
                Vec::<String>::new(),
                "{on} {grant} under a push"
            );
        }
        assert_eq!(
            token_problems(&[workflow(on, "{ contents: read }", "")]),
            Vec::<String>::new(),
            "{on} without the token"
        );
    }
    for on in ["on: push", "on: workflow_dispatch", "on: workflow_call"] {
        assert_eq!(
            token_problems(&[workflow(on, "{ id-token: write }", "")]),
            Vec::<String>::new(),
            "{on}"
        );
    }
}

#[test]
fn a_workflow_that_is_not_yaml_is_a_problem_and_farm_yml_is_not_judged_here() {
    // Catches: an unreadable workflow skipped as if it held no token, and the farm lane's own file
    // refused by the rule that holds the others.
    let bad = ("bad.yml".to_owned(), "a: [".to_owned());
    let found = token_problems(&[bad]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("bad.yml: "), "{found:?}");
    let (_, farm) = workflow("on: pull_request", "{ id-token: write }", "");
    assert_eq!(
        token_problems(&[(FILE.to_owned(), farm)]),
        Vec::<String>::new()
    );
}
