//! The rules of `farm.yml` ([`super::farm`]), the farm lane: the one job that holds
//! `id-token: write` on a pull request, and only under its exact `if:`; farm-connect pinned by
//! commit with exactly the variables it is given; the triggers, the runner, the rights, and the
//! upload of a stale table's patch (the steps that reach the farm and the time limits are
//! `farm_steps_tests`); and no other workflow's job holding
//! the token on a pull request but under a push to main or a manual run, a check of the `if:`'s
//! conjuncts that follows the calls of other workflows, the signer artifacts.yml's one holder
//! besides its calls, and macos-farm.yml's token jobs that run no repository code. Each rule is
//! shown to refuse its defect by putting it into the committed workflow and reading the exact
//! problems the lint reports.

use super::farm::{
    CONNECT_INPUTS, FARM_CONNECT, FILE, JOB_IF, ON_PROBLEM, PATCH_ARTIFACT, RUNNER,
    TOP_PERMISSIONS_PROBLEM, UPLOAD_IF, patch_file, problems, token_problems, upload_inputs,
};
use super::farm_steps::TABLE_RUN;
use super::support::{in_job, text_of, workflows};
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
    // Catches: farm-connect at a branch, a short commit or a commit in capitals (each also another
    // commit than the one buck2-farm and its tool are of, farm_steps' rule).
    let pin = pinned();
    let (_, sha) = pin.split_once('@').expect("a ref");
    let says =
        format!("job build: {FARM_CONNECT} must be pinned to a 40-digit lowercase commit SHA");
    let other = format!(
        "job build: step 15 uses {FARM_CONNECT} at another commit than KOMIRA_COMMIT ({sha})"
    );
    let capitals = sha.to_uppercase();
    for at in ["main", &sha[..7], capitals.as_str()] {
        assert_eq!(
            problems_with(&pin, &format!("{FARM_CONNECT}@{at}")),
            [says.clone(), other.clone()],
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
        [format!(
            "job build: exactly one step must use {FARM_CONNECT}"
        )]
    );
    let twice = with(
        "      - name: Connect to the build farm\n",
        &format!(
            "      - uses: {pin}\n        timeout-minutes: 30\n        with:\n          farm-address: elsewhere\n      - name: Connect to the build farm\n"
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

/// The upload step of the committed file.
const UPLOAD_STEP: &str = "      - name: Upload the regenerated build table as a patch
        if: ${{ failure() }}
        timeout-minutes: 10
        uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1
        with:
          name: buildrules-sync
          path: ${{ runner.temp }}/buildrules-sync/buildrules-sync.patch
          if-no-files-found: ignore
          retention-days: 30
";

#[test]
fn the_table_check_writes_the_patch_the_upload_takes() {
    // Catches: the table's check writing the patch to one file and the upload taking another, so a
    // stale table's summary names an artifact that holds nothing.
    assert!(TABLE_RUN.contains(&format!("--patch \"{}\"", patch_file(true))));
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
    // command would download nothing), one before the table's check (there is no patch yet), one
    // that runs on a passing build only or always, and one of another file or kept another time.
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
            "job build: the upload of {PATCH_ARTIFACT} does not come after the table's check"
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
    // tags, another trigger beside them, `branches` as a scalar, and the list form of `on`; and a
    // call without its input, or with the input renamed, optional, of another type, defaulted, or
    // beside another.
    let call = "  workflow_call:\n    inputs:\n      target:\n        description: The one target the last step builds in place of //:ci.\n        type: string\n        required: true\n";
    let on = format!("on:\n  pull_request:\n  push:\n    branches: [main]\n{call}");
    for other in [
        format!(
            "on:\n  pull_request:\n    paths: [crates/**]\n  push:\n    branches: [main]\n{call}"
        ),
        format!("on:\n  pull_request:\n  push:\n    branches: [dev]\n{call}"),
        format!("on:\n  pull_request:\n  push:\n    branches: [main, dev]\n{call}"),
        format!("on:\n  pull_request:\n  push:\n    branches: main\n{call}"),
        format!("on:\n  pull_request:\n  push:\n    tags: ['v*']\n{call}"),
        format!(
            "on:\n  pull_request:\n  push:\n    branches: [main]\n    paths: [crates/**]\n{call}"
        ),
        format!(
            "on:\n  pull_request:\n  push:\n    branches: [main]\n{call}  workflow_dispatch:\n"
        ),
        "on:\n  pull_request:\n  push:\n    branches: [main]\n".to_owned(),
        on.replace("      target:\n", "      targets:\n"),
        on.replace("        required: true\n", "        required: false\n"),
        on.replace("        type: string\n", "        type: boolean\n"),
        on.replace(
            "        required: true\n",
            "        required: true\n        default: //:ci\n",
        ),
        format!("{on}      other:\n        type: string\n        required: false\n"),
        "on: [pull_request, push, workflow_call]\n".to_owned(),
        "on: push\n".to_owned(),
    ] {
        assert_eq!(problems_with(&on, &other), [ON_PROBLEM], "{other}");
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

/// The problem of a job of a workflow a pull request starts that holds the token under another
/// `if:` than a push to main or a manual run.
fn on_a_pull_request(file: &str, job: &str) -> String {
    format!(
        "{file}: job {job} holds `id-token: write` on a pull request (its `if:` does not pin a push to main or a manual run)"
    )
}

/// The problem of a job below a call that holds the token where nothing above it pins an event.
fn below_a_call(file: &str, chain: &str) -> String {
    format!(
        "{file}: job {chain} holds `id-token: write` on a pull request (neither its `if:` nor the calls above it pin a push to main or a manual run)"
    )
}

/// The repository's workflows with `from` replaced by `to` inside the job `job` of `file`.
fn changed_job(file: &str, job: &str, from: &str, to: &str) -> Vec<(String, String)> {
    let mut all = workflows();
    let found = all
        .iter_mut()
        .find(|(name, _)| name == file)
        .unwrap_or_else(|| panic!("{file}"));
    found.1 = in_job(&found.1, job, from, to);
    all
}

/// A workflow `file` that starts on `on` (a line of YAML) with `jobs` (the YAML under `jobs:`).
fn file_of(file: &str, on: &str, jobs: &str) -> (String, String) {
    (
        file.to_owned(),
        format!("name: {file}\n{on}\njobs:\n{jobs}"),
    )
}

#[test]
fn no_other_workflow_holds_the_token_on_a_pull_request_but_under_a_push() {
    // Catches: a token grant the farm lane's exception would be copied into, in a workflow that a
    // pull request starts: the committed ones hold none outside a push or a manual run, and a
    // call that loses its push condition is named with the jobs below it that hold the token.
    assert_eq!(token_problems(&workflows()), Vec::<String>::new());
    let released = "if: ${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}";
    assert_eq!(
        token_problems(&changed(
            "release.yml",
            released,
            "if: ${{ !cancelled() && needs.plan.outputs.release == 'true' }}"
        )),
        [
            on_a_pull_request("release.yml", "artifacts"),
            below_a_call("release.yml", "artifacts -> artifacts.yml/macos-farm"),
        ]
    );
    let packaged = "  beta-macos-pkg:\n    needs: [plan, artifacts]\n    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}\n";
    assert_eq!(
        token_problems(&changed(
            "release.yml",
            packaged,
            "  beta-macos-pkg:\n    needs: [plan, artifacts]\n    if: ${{ needs.plan.outputs.release == 'true' }}\n"
        )),
        [
            on_a_pull_request("release.yml", "beta-macos-pkg"),
            below_a_call(
                "release.yml",
                "beta-macos-pkg -> js-agent-macos-pkg.yml/farm"
            ),
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
    // job that pins a push or a manual run, is left alone.
    let refused = on_a_pull_request("new.yml", "token");
    let push_only =
        "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n";
    let dispatch_only = "    if: ${{ github.event_name == 'workflow_dispatch' }}\n";
    for on in [
        "on: pull_request",
        "on: [push, pull_request_target]",
        "on:\n  pull_request:\n    paths: [a]",
        "on:\n  merge_group:\n    types: [checks_requested]",
    ] {
        for grant in ["{ id-token: write }", "write-all"] {
            assert_eq!(
                token_problems(&[workflow(on, grant, "")]),
                [refused.as_str()],
                "{on} {grant}"
            );
            for pinned in [push_only, dispatch_only] {
                assert_eq!(
                    token_problems(&[workflow(on, grant, pinned)]),
                    Vec::<String>::new(),
                    "{on} {grant} under {pinned}"
                );
            }
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
fn the_push_check_reads_the_top_level_conjuncts_of_the_if_and_not_its_text() {
    // Catches: a condition that names a push and also lets a pull request through (`||` with
    // another event, a negation, another event's name, a name inside a string, a ref alone, a
    // variable), which a search of the text for `== 'push'` accepts; and that every spelling of a
    // push to main or a manual run is accepted: the conjunct alone, among others, with the ref,
    // in a parenthesised `||` of the two events, nested, and without `${{ }}`.
    let refused = [
        "${{ github.event_name == 'push' || github.event_name == 'pull_request' }}",
        "${{ github.event_name == 'pull_request' || github.event_name == 'push' }}",
        "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' || github.ref == 'refs/heads/topic' }}",
        "${{ (github.event_name == 'push' || github.event_name == 'pull_request') && github.ref == 'refs/heads/main' }}",
        "${{ !(github.event_name == 'push') }}",
        "${{ github.event_name != 'workflow_dispatch' }}",
        "${{ github.event_name != 'pull_request' }}",
        "${{ github.event_name == 'pull_request' }}",
        "${{ github.event_name == 'push_' }}",
        "${{ github.event_name == 'a && github.event_name == ''push''' }}",
        "${{ github.ref == 'refs/heads/main' }}",
        "${{ github.event_name == 'push' }}",
        "${{ github.event_name == 'push' && github.ref == 'refs/heads/topic' }}",
        "${{ !cancelled() && github.event_name == 'push' && needs.plan.outputs.release == 'true' }}",
        "${{ needs.a.result == 'success' && (github.event_name == 'push' || github.event_name == 'workflow_dispatch') }}",
        "${{ (github.event_name == 'push') }}",
        "github.event_name == 'push'",
        "${{ vars.MACOS_ON_FARM != 'true' }}",
        "${{ contains(github.event_name, 'push') }}",
        "${{ always() }}",
        "${{ (github.event_name) == 'x' }}",
        "${{ }}",
        "''",
    ];
    for condition in refused {
        assert_eq!(
            token_problems(&[workflow(
                "on: pull_request",
                "{ id-token: write }",
                &format!("    if: {condition}\n")
            )]),
            [on_a_pull_request("new.yml", "token")],
            "{condition}"
        );
    }
    let accepted = [
        "${{ github.event_name == 'workflow_dispatch' }}",
        "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}",
        "${{ github.ref == 'refs/heads/main' && github.event_name == 'push' }}",
        "${{ !cancelled() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}",
        "${{ (github.event_name == 'push') && (github.ref == 'refs/heads/main') }}",
        "${{ (github.event_name == 'push' && github.ref == 'refs/heads/main') || github.event_name == 'workflow_dispatch' }}",
        "${{ inputs.what == 'release' && ((github.event_name == 'push' && github.ref == 'refs/heads/main') || github.event_name == 'workflow_dispatch') }}",
        "${{ needs.a.result == 'success' && (github.event_name == 'workflow_dispatch' || (github.event_name == 'push' && github.ref == 'refs/heads/main')) }}",
        "${{ ((github.event_name == 'workflow_dispatch')) }}",
        "github.event_name == 'push' && github.ref == 'refs/heads/main'",
    ];
    for condition in accepted {
        assert_eq!(
            token_problems(&[workflow(
                "on: pull_request",
                "{ id-token: write }",
                &format!("    if: {condition}\n")
            )]),
            Vec::<String>::new(),
            "{condition}"
        );
    }
    // The job with no `if:` at all, and a job that holds no token whatever its `if:`.
    assert_eq!(
        token_problems(&[workflow("on: pull_request", "{ id-token: write }", "")]),
        [on_a_pull_request("new.yml", "token")]
    );
    assert_eq!(
        token_problems(&[workflow(
            "on: pull_request",
            "{ contents: read }",
            "    if: ${{ github.event_name == 'pull_request' }}\n"
        )]),
        Vec::<String>::new()
    );
}

/// A caller `a.yml` started by a pull request whose call job `call` has `call_if` (a line of YAML
/// or empty) and no permissions, and the callee `b.yml` with `jobs`.
fn chain(call_if: &str, jobs: &str) -> Vec<(String, String)> {
    vec![
        file_of(
            "a.yml",
            "on: pull_request",
            &format!("  call:\n{call_if}    uses: ./.github/workflows/b.yml\n"),
        ),
        file_of("b.yml", "on: workflow_call", jobs),
    ]
}

/// The job `token` of a callee: runs on a runner, holds the token, under `job_if`.
fn token_job(job_if: &str) -> String {
    format!(
        "  token:\n    runs-on: ubuntu-24.04\n{job_if}    permissions: {{ id-token: write }}\n    steps:\n      - run: true\n"
    )
}

#[test]
fn the_jobs_a_pull_request_reaches_through_a_call_are_held_to_the_same_rule() {
    // Catches: a called workflow's job that holds the token under a call that does not pin an
    // event, which no check of the calling workflow alone sees (the call job here has no
    // permissions of its own); and that the call's `if:`, the job's own, a dispatch and a push
    // each let it be, as does a callee that holds no token.
    let push = "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n";
    let dispatch = "    if: ${{ github.event_name == 'workflow_dispatch' }}\n";
    assert_eq!(
        token_problems(&chain("", &token_job(""))),
        [below_a_call("a.yml", "call -> b.yml/token")]
    );
    assert_eq!(
        token_problems(&chain(
            "    if: ${{ vars.MACOS_ON_FARM == 'true' }}\n",
            &token_job("")
        )),
        [below_a_call("a.yml", "call -> b.yml/token")]
    );
    for pinned in [push, dispatch] {
        assert_eq!(
            token_problems(&chain(pinned, &token_job(""))),
            Vec::<String>::new(),
            "the call pins: {pinned}"
        );
        assert_eq!(
            token_problems(&chain("", &token_job(pinned))),
            Vec::<String>::new(),
            "the job pins: {pinned}"
        );
    }
    assert_eq!(
        token_problems(&chain(
            "",
            "  quiet:\n    runs-on: ubuntu-24.04\n    permissions: { contents: read }\n    steps:\n      - run: true\n"
        )),
        Vec::<String>::new()
    );
    // A call of a workflow of another repository is not followed.
    assert_eq!(
        token_problems(&[file_of(
            "a.yml",
            "on: pull_request",
            "  call:\n    uses: someone/else/.github/workflows/b.yml@0123456789abcdef0123456789abcdef01234567\n"
        )]),
        Vec::<String>::new()
    );
}

#[test]
fn a_call_below_a_call_is_followed_and_what_cannot_be_followed_is_named() {
    // Catches: a token job two calls down (named by the whole chain, with no pin above it, or a
    // pin at the middle that covers it), a call of a file that is not in the repository, and a
    // chain of calls that comes back to a file it is inside, which would never end.
    let middle = |call_if: &str, jobs: &str| {
        vec![
            file_of(
                "a.yml",
                "on: pull_request",
                "  call:\n    uses: ./.github/workflows/b.yml\n",
            ),
            file_of(
                "b.yml",
                "on: workflow_call",
                &format!("  again:\n{call_if}    uses: ./.github/workflows/c.yml\n"),
            ),
            file_of("c.yml", "on: workflow_call", jobs),
        ]
    };
    assert_eq!(
        token_problems(&middle("", &token_job(""))),
        [below_a_call("a.yml", "call -> b.yml/again -> c.yml/token")]
    );
    assert_eq!(
        token_problems(&middle(
            "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n",
            &token_job("")
        )),
        Vec::<String>::new()
    );
    let missing = vec![file_of(
        "a.yml",
        "on: pull_request",
        "  call:\n    uses: ./.github/workflows/gone.yml\n",
    )];
    assert_eq!(
        token_problems(&missing),
        ["a.yml: job call calls ./.github/workflows/gone.yml, a workflow this lint cannot read"]
    );
    let looped = vec![
        file_of(
            "a.yml",
            "on: pull_request",
            "  call:\n    uses: ./.github/workflows/b.yml\n",
        ),
        file_of(
            "b.yml",
            "on: workflow_call",
            "  back:\n    uses: ./.github/workflows/b.yml\n",
        ),
    ];
    assert_eq!(
        token_problems(&looped),
        ["a.yml: job call -> b.yml/back calls b.yml, which the chain is already inside"]
    );
}

#[test]
fn artifacts_yml_lets_only_attest_and_the_calls_of_other_files_hold_the_token() {
    // Catches: a job of the signer's file that holds the token and could mint one that names it as
    // the signer (a build job, `sums`, a token at the top), though the release's call, which
    // pins a push, is the only way the file runs; and that `attest`, and a call of another file
    // (local or of another repository) that holds the token for its own jobs, are left alone.
    assert_eq!(token_problems(&workflows()), Vec::<String>::new());
    let refused = |job: &str| {
        format!(
            "artifacts.yml: job {job} holds `id-token: write`, which only attest and a call of another workflow file may"
        )
    };
    for job in ["build", "sums", "verify"] {
        assert_eq!(
            token_problems(&changed_job(
                "artifacts.yml",
                job,
                "      contents: read\n",
                "      contents: read\n      id-token: write\n"
            )),
            [refused(job)],
            "{job}"
        );
    }
    assert_eq!(
        token_problems(&changed(
            "artifacts.yml",
            "\npermissions: {}\n",
            "\npermissions:\n  id-token: write\n"
        )),
        ["artifacts.yml: holds `id-token: write` at its top level, for every job"]
    );
    // Another call, of this repository or another, may hold it; a call of the file itself may not.
    let other = |uses: &str| {
        changed_job(
            "artifacts.yml",
            "macos-farm",
            "    uses: ./.github/workflows/macos-farm.yml\n",
            &format!("    uses: {uses}\n"),
        )
    };
    assert_eq!(
        token_problems(&other("./.github/workflows/beta.yml")),
        Vec::<String>::new()
    );
    assert_eq!(
        token_problems(&other(
            "someone/else/.github/workflows/b.yml@0123456789abcdef0123456789abcdef01234567"
        )),
        Vec::<String>::new()
    );
    assert_eq!(
        token_problems(&other("./.github/workflows/artifacts.yml")),
        [
            refused("macos-farm"),
            "release.yml: job artifacts -> artifacts.yml/macos-farm calls artifacts.yml, which the chain is already inside"
                .to_owned()
        ]
    );
}

#[test]
fn the_jobs_of_macos_farm_yml_that_hold_the_token_run_no_repository_code() {
    // Catches: a job that holds the token running any `cargo` (build, run, test, xtask, check,
    // clippy, install, fetch; behind a toolchain, a flag with or without a value, a path or a
    // pipeline), using an action of this repository, or under an `if:` that does not pin a push
    // to main or a manual run; and that the real file, whose jobs run `rustup` and the prebuilt
    // `tools/xtask`, and the job that holds no token, which builds xtask, are left alone.
    assert_eq!(token_problems(&workflows()), Vec::<String>::new());
    let stamp = r#"run: tools/xtask release stamp --to "$VERSION""#;
    let holds = "macos-farm.yml: job release holds `id-token: write` and runs `cargo`";
    for run in [
        r#"run: cargo xtask release stamp --to "$VERSION""#,
        "run: cargo +stable build --locked",
        "run: cargo --locked build",
        "run: set -e; cargo run -p xtask",
        "run: (cargo test) | tee log",
        r#"run: echo "$(cargo xtask x)""#,
        "run: cargo check",
        "run: cargo clippy --all-targets",
        "run: cargo install --path .",
        "run: cargo --manifest-path x build",
        "run: cargo -C dir build",
        "run: cargo fetch --locked",
        "run: cargo",
        "run: ~/.cargo/bin/cargo build",
        "run: |\n          set -e\n          cargo metadata",
    ] {
        assert_eq!(
            token_problems(&changed_job("macos-farm.yml", "release", stamp, run)),
            [holds],
            "{run}"
        );
    }
    for run in [
        "run: rustup toolchain install",
        "run: echo cargo-deny",
        r#"run: tools/xtask release stamp --to "$VERSION""#,
    ] {
        assert_eq!(
            token_problems(&changed_job("macos-farm.yml", "release", stamp, run)),
            Vec::<String>::new(),
            "{run}"
        );
    }
    let action = "      - run: rustup toolchain install\n";
    assert_eq!(
        token_problems(&changed_job(
            "macos-farm.yml",
            "pkg",
            action,
            &format!("{action}      - uses: ./.github/actions/js-agent-next-version\n")
        )),
        [
            "macos-farm.yml: job pkg holds `id-token: write` and uses the repository's own action ./.github/actions/js-agent-next-version"
        ]
    );
    assert_eq!(
        token_problems(&changed_job(
            "macos-farm.yml",
            "pkg-update",
            "    if: ${{ inputs.what == 'pkg' && ((github.event_name == 'push' && github.ref == 'refs/heads/main') || github.event_name == 'workflow_dispatch') }}\n",
            "    if: ${{ inputs.what == 'pkg' }}\n"
        )),
        [
            "macos-farm.yml: job pkg-update holds `id-token: write` and its `if:` does not pin a push to main or a manual run"
        ]
    );
    assert_eq!(
        token_problems(&changed_job(
            "macos-farm.yml",
            "xtask",
            "      contents: read\n",
            "      contents: read\n      id-token: write\n"
        )),
        [
            "macos-farm.yml: job xtask holds `id-token: write` and its `if:` does not pin a push to main or a manual run",
            "macos-farm.yml: job xtask holds `id-token: write` and runs `cargo`"
        ]
    );
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
    // An unreadable farm.yml is not judged either, and a call of it cannot be followed.
    assert_eq!(
        token_problems(&[(FILE.to_owned(), "a: [".to_owned())]),
        Vec::<String>::new()
    );
    let call = file_of(
        "a.yml",
        "on: pull_request",
        "  call:\n    uses: ./.github/workflows/farm.yml\n",
    );
    let found = token_problems(&[call, (FILE.to_owned(), "a: [".to_owned())]);
    assert_eq!(
        found,
        ["a.yml: job call calls ./.github/workflows/farm.yml, a workflow this lint cannot read"]
    );
}
