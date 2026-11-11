//! `js-server-deploy.yml` ([`super::server_deploy`]): the file of the repository passes, each way
//! of breaking what it is held to is named by the lint on a broken copy of the workflow text, and
//! no workflow calls the run.

use super::server_deploy::{
    CHECK, CHECK_IF, DEPLOY_COMMIT, DEPLOY_IF, DEPLOY_WORKFLOW, FILE, GROUP, OUTPUTS,
    caller_problems, server_deploy_problems, with_wanted,
};
use super::support::{in_job, text_of, workflows};

#[test]
fn js_server_deploy_yml_checks_the_release_then_deploys_its_digest() {
    // Catches: any change to js-server-deploy.yml that breaks what it is held to: a trigger
    // added or dropped, the gate dropped from a job, the check skipped or not handed on, the
    // deploy service's workflow called with another service, release, endpoint or environment.
    assert_eq!(server_deploy_problems(&text_of(FILE)), Vec::<String>::new());
}

#[test]
fn the_variable_gate_is_the_job_level_if_of_both_jobs() {
    // Catches: a gate on a step (the job would start, and the token exist, with the deploy off),
    // a gate on another variable or on a value other than `true`, a job that runs from a branch
    // other than main, and a release run acted on although it failed, ran on another branch or
    // came from a fork.
    for condition in [CHECK_IF, DEPLOY_IF] {
        assert!(condition.contains("github.ref == 'refs/heads/main'"));
        assert!(condition.contains("vars.JS_SERVER_DEPLOYS == 'true'"));
    }
    assert!(DEPLOY_IF.contains("needs.check.outputs.deploy == 'true'"));
    let text = text_of(FILE);
    for (part, with) in [
        (
            "vars.JS_SERVER_DEPLOYS == 'true'",
            "vars.JS_SERVER_DEPLOYS != ''",
        ),
        ("github.ref == 'refs/heads/main' && ", ""),
        ("github.event.workflow_run.conclusion == 'success' && ", ""),
        ("github.event.workflow_run.head_branch == 'main' && ", ""),
        (
            " && github.event.workflow_run.head_repository.full_name == github.repository",
            "",
        ),
    ] {
        assert!(CHECK_IF.contains(part), "{part}");
        let spoiled = in_job(&text, "check", CHECK_IF, &CHECK_IF.replacen(part, with, 1));
        assert!(
            server_deploy_problems(&spoiled)
                .iter()
                .any(|p| p.starts_with("check: `if` is")),
            "check without {part:?}"
        );
    }
    for (part, with) in [
        (
            "vars.JS_SERVER_DEPLOYS == 'true'",
            "vars.JS_SERVER_DEPLOYS != ''",
        ),
        ("github.ref == 'refs/heads/main' && ", ""),
        (" && needs.check.outputs.deploy == 'true'", ""),
    ] {
        assert!(DEPLOY_IF.contains(part), "{part}");
        let spoiled = in_job(
            &text,
            "deploy",
            DEPLOY_IF,
            &DEPLOY_IF.replacen(part, with, 1),
        );
        assert!(
            server_deploy_problems(&spoiled)
                .iter()
                .any(|p| p.starts_with("deploy: `if` is")),
            "deploy without {part:?}"
        );
    }
    for (job, condition) in [("check", CHECK_IF), ("deploy", DEPLOY_IF)] {
        let spoiled = in_job(&text, job, &format!("    if: {condition}\n"), "");
        assert!(
            server_deploy_problems(&spoiled)
                .iter()
                .any(|p| p == &format!("{job}: `if` is None, not {condition}")),
            "{job} without an `if`"
        );
    }
}

/// `(job, text to replace once in it, its replacement, what a problem must say)`.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "check",
        "    runs-on: ubuntu-24.04\n",
        "    needs: [deploy]\n    runs-on: ubuntu-24.04\n",
        "check: needs [\"deploy\"]; it needs no job",
    ),
    (
        "check",
        "      actions: read\n",
        "      actions: write\n",
        "check: permissions are [\"actions: write\", \"contents: read\"], not actions: read and contents: read",
    ),
    (
        "check",
        "      actions: read\n",
        "",
        "check: permissions are [\"contents: read\"], not actions: read and contents: read",
    ),
    // The check runs the repository's code: it must hold no token the deploy service accepts.
    (
        "check",
        "      contents: read\n",
        "      contents: read\n      id-token: write\n",
        "check: permissions are [\"actions: read\", \"contents: read\", \"id-token: write\"]",
    ),
    (
        "check",
        "      contents: read\n",
        "      contents: write\n",
        "check: permissions are [\"actions: read\", \"contents: write\"]",
    ),
    (
        "check",
        "--dir \"$RUNNER_TEMP/server-deploy\"",
        "--dir dist",
        "check: the step `release` does not run",
    ),
    // A run after a release that ignores which run it was: every run would deploy a release.
    (
        "check",
        " --after-run \"$RUN_ID\"",
        "",
        "check: the step `release` does not run",
    ),
    (
        "check",
        "        id: release\n",
        "        id: index\n",
        "check: the step `release` does not run",
    ),
    (
        "check",
        "VERSION: ${{ inputs.version }}",
        "VERSION: 0.1.0",
        "check: the step `release` does not read VERSION from ${{ inputs.version }}",
    ),
    (
        "check",
        "RUN_ID: ${{ github.event.workflow_run.id }}",
        "RUN_ID: ${{ github.run_id }}",
        "check: the step `release` does not read RUN_ID from ${{ github.event.workflow_run.id }}",
    ),
    (
        "check",
        "GH_TOKEN: ${{ github.token }}",
        "GH_TOKEN: ''",
        "check: the step `release` does not read GH_TOKEN from ${{ github.token }}",
    ),
    (
        "check",
        "deploy: ${{ steps.release.outputs.deploy }}",
        "deploy: 'true'",
        "check: output deploy is",
    ),
    (
        "check",
        "version: ${{ steps.release.outputs.version }}",
        "version: ${{ inputs.version }}",
        "check: output version is",
    ),
    (
        "check",
        "image-digest: ${{ steps.release.outputs.image-digest }}",
        "image-digest: sha256:0",
        "check: output image-digest is",
    ),
    (
        "check",
        "image-url: ${{ steps.release.outputs.image-url }}",
        "image-url: https://example.com/x.tar",
        "check: output image-url is",
    ),
    (
        "check",
        "      bundle-url: ${{ steps.release.outputs.bundle-url }}\n",
        "",
        "check: output bundle-url is None",
    ),
    (
        "check",
        "      bundle-url: ${{ steps.release.outputs.bundle-url }}\n",
        "      bundle-url: ${{ steps.release.outputs.bundle-url }}\n      extra: ${{ steps.release.outputs.extra }}\n",
        "check: it hands on 6 outputs, not the 5 of the step `release`",
    ),
    (
        "deploy",
        "needs: [check]",
        "needs: [check, publish]",
        "deploy: needs are [\"check\", \"publish\"], not [\"check\"]",
    ),
    (
        "deploy",
        "    needs: [check]\n",
        "",
        "deploy: needs are [], not [\"check\"]",
    ),
    (
        "deploy",
        "      id-token: write\n",
        "      id-token: write\n      actions: read\n",
        "deploy: permissions are",
    ),
    (
        "deploy",
        "      contents: read\n",
        "",
        "deploy: permissions are [\"id-token: write\"], not contents: read and id-token: write",
    ),
    (
        "deploy",
        "komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@",
        "komira-ai/other/.github/workflows/deploy.yml@",
        "deploy: `uses` is not",
    ),
    // A revision that is not the one the receiving side allows.
    (
        "deploy",
        "deploy.yml@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6",
        "deploy.yml@32a9cc6f8242d0efbc3476391302062ff444dd47",
        "deploy: `uses` is not",
    ),
    // A branch or tag instead of a commit.
    (
        "deploy",
        "deploy.yml@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6",
        "deploy.yml@main",
        "deploy: `uses` is not",
    ),
    (
        "deploy",
        "    uses: komira-ai",
        "    environment: controlplane\n    uses: komira-ai",
        "deploy: the job sets an `environment`",
    ),
    (
        "deploy",
        "service: js-server",
        "service: js-agent",
        "deploy: with.service",
    ),
    (
        "deploy",
        "version: ${{ needs.check.outputs.version }}",
        "version: ${{ inputs.version }}",
        "deploy: with.version",
    ),
    // The digest dropped: the service would be given a release by name alone.
    (
        "deploy",
        "      image-digest: ${{ needs.check.outputs.image-digest }}\n",
        "",
        "deploy: with.image-digest is None",
    ),
    (
        "deploy",
        "image-url: ${{ needs.check.outputs.image-url }}",
        "image-url: https://example.com/x.tar",
        "deploy: with.image-url",
    ),
    (
        "deploy",
        "bundle-url: ${{ needs.check.outputs.bundle-url }}",
        "bundle-url: ''",
        "deploy: with.bundle-url",
    ),
    (
        "deploy",
        "endpoint: ${{ vars.CP_DEPLOY_ENDPOINT }}",
        "endpoint: https://deploy.example",
        "deploy: with.endpoint",
    ),
    (
        "deploy",
        "audience: ${{ vars.CP_DEPLOY_AUDIENCE }}",
        "audience: ''",
        "deploy: with.audience",
    ),
    (
        "deploy",
        "environment: controlplane",
        "environment: production",
        "deploy: with.environment is Some(\"production\"), not \"controlplane\"",
    ),
    (
        "deploy",
        "wait-minutes: 30",
        "wait-minutes: 0",
        "deploy: with.wait-minutes is Some(\"0\"), not \"30\"",
    ),
    // An input the receiving side's signer defaults make unnecessary and a caller must not move.
    (
        "deploy",
        "      wait-minutes: 30\n",
        "      wait-minutes: 30\n      signer-repository: someone/else\n",
        "deploy: with.signer-repository is not an input it gives",
    ),
    (
        "deploy",
        "tailscale-oauth-client-id: ${{ vars.CP_DEPLOY_TAILNET_CLIENT_ID }}",
        "tailscale-oauth-client-id: ''",
        "deploy: with.tailscale-oauth-client-id",
    ),
    (
        "deploy",
        "tailscale-audience: ${{ vars.CP_DEPLOY_TAILNET_AUDIENCE }}",
        "tailscale-audience: ''",
        "deploy: with.tailscale-audience",
    ),
    (
        "deploy",
        "      tailscale-tags: ${{ vars.CP_DEPLOY_TAILNET_TAGS }}\n",
        "",
        "deploy: with.tailscale-tags is None",
    ),
];

#[test]
fn each_defect_in_js_server_deploy_yml_is_named_by_the_lint() {
    // Catches: a rule of the lint that holds nothing, because the file passes whether or not the
    // thing it holds is there. Each row is the workflow with one change, which must be named.
    let text = text_of(FILE);
    for (job, replace, with, says) in DEFECTS {
        let problems = server_deploy_problems(&in_job(&text, job, replace, with));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{job}: {replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
}

#[test]
fn a_trigger_an_input_or_a_job_other_than_the_two_ways_in_is_named() {
    // Catches: a push or a schedule deploying, the completion of another workflow or of release on
    // another branch, the dispatch dropped, the two ways in sharing one concurrency group or a run
    // cancelled, a second input or none, an input that is not a required string, a job beside the
    // two, a top level that grants something, and a document that does not parse being read as
    // clean.
    let text = text_of(FILE);
    for (replace, with, says) in [
        (
            "on:\n  workflow_run:\n",
            "on:\n  push:\n  workflow_run:\n",
            "triggers are [\"push\", \"workflow_dispatch\", \"workflow_run\"], not workflow_dispatch and workflow_run",
        ),
        (
            "    workflows: [release]\n",
            "    workflows: [ci]\n",
            "workflow_run.workflows is [\"ci\"], not [\"release\"]",
        ),
        (
            "    workflows: [release]\n",
            "    workflows: [release, ci]\n",
            "workflow_run.workflows is [\"ci\", \"release\"], not [\"release\"]",
        ),
        (
            "    types: [completed]\n",
            "    types: [requested]\n",
            "workflow_run.types is [\"requested\"], not [\"completed\"]",
        ),
        (
            "    branches: [main]\n",
            "",
            "workflow_run.branches is [], not [\"main\"]",
        ),
        (
            "      version:\n        description",
            "      tag:\n        description",
            "inputs are [\"tag\"], not version alone",
        ),
        (
            "      version:\n        description",
            "      note:\n        type: string\n      version:\n        description",
            "inputs are [\"note\", \"version\"], not version alone",
        ),
        (
            "        type: string\n        required: true\n",
            "        type: number\n        required: true\n",
            "input version: type is Some(\"number\"), not string",
        ),
        (
            "        type: string\n        required: true\n",
            "        type: string\n        required: false\n",
            "input version: required is Some(\"false\"), not true",
        ),
        // One group for both ways in: a waiting deploy by hand is replaced by a release run.
        (
            "  group: js-server-deploy-${{ github.event_name == 'workflow_dispatch' && 'dispatch' || 'release' }}\n",
            "  group: js-server-deploy\n",
            "concurrency: group is Some(\"js-server-deploy\"), not",
        ),
        (
            "  cancel-in-progress: false\n",
            "  cancel-in-progress: true\n",
            "concurrency: cancel-in-progress is Some(\"true\"), not false",
        ),
        (
            "permissions: {}\n",
            "permissions:\n  contents: read\n",
            "the top level does not grant `permissions: {}`",
        ),
        (
            "jobs:\n  check:\n",
            "jobs:\n  publish:\n    runs-on: ubuntu-24.04\n  check:\n",
            "jobs are [\"check\", \"deploy\", \"publish\"], not check and deploy",
        ),
    ] {
        assert!(text.contains(replace), "{replace:?} is not in {FILE}");
        let problems = server_deploy_problems(&text.replacen(replace, with, 1));
        assert!(
            problems.iter().any(|p| p.contains(says)),
            "{replace:?} -> {with:?} must say {says:?}: {problems:?}"
        );
    }
    // The dispatch dropped (a deploy by hand is how a failed one is retried).
    let start = text.find("  workflow_dispatch:\n").expect("dispatch");
    let end = text.find("permissions: {}\n").expect("permissions");
    let without = format!("{}\n{}", &text[..start], &text[end..]);
    assert!(
        server_deploy_problems(&without)
            .iter()
            .any(|p| p == "triggers are [\"workflow_run\"], not workflow_dispatch and workflow_run"),
        "{:?}",
        server_deploy_problems(&without)
    );
    assert!(!server_deploy_problems("permissions: {\njobs: [\n").is_empty());
}

#[test]
fn the_check_runs_the_verb_and_the_call_pins_a_commit_of_the_deploy_service() {
    // Catches: the deploy starting without the check (it needs it), the check running another
    // command than the verb, a pin that is not a full commit, and a call that names another
    // service, environment or wait than js-server in controlplane for 30 minutes.
    let text = text_of(FILE);
    assert!(text.contains(&format!("        run: {CHECK}\n")));
    assert!(text.contains("    needs: [check]\n"));
    assert!(text.contains(&format!("    uses: {DEPLOY_WORKFLOW}@{DEPLOY_COMMIT}\n")));
    assert_eq!(DEPLOY_COMMIT.len(), 40);
    assert!(
        DEPLOY_COMMIT
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    );
    assert!(GROUP.starts_with("js-server-deploy-"));
    let wanted = with_wanted();
    assert!(wanted.contains(&("service", "js-server")));
    assert!(wanted.contains(&("environment", "controlplane")));
    assert!(wanted.contains(&("wait-minutes", "30")));
    assert_eq!(OUTPUTS.len(), 5);
    for output in OUTPUTS {
        assert!(text.contains(&format!(
            "{output}: ${{{{ steps.release.outputs.{output} }}}}"
        )));
    }
    // No input of the signer: the called workflow's defaults are this repository, artifacts.yml
    // and refs/heads/main, which is where the release's image is attested.
    assert!(!wanted.iter().any(|(k, _)| k.starts_with("signer-")));
}

#[test]
fn no_workflow_calls_the_run() {
    // Catches: release.yml or another workflow calling js-server-deploy.yml (it starts by its own
    // trigger once release.yml completes, and a second caller would deploy twice).
    let files = workflows();
    assert_eq!(caller_problems(&files), Vec::<String>::new());
    let release = files
        .iter()
        .find(|(name, _)| name == "release.yml")
        .expect("release.yml")
        .1
        .clone();
    let calling = vec![
        (FILE.to_owned(), text_of(FILE)),
        (
            "release.yml".to_owned(),
            format!("{release}  deploy:\n    uses: ./.github/workflows/{FILE}\n"),
        ),
    ];
    assert_eq!(
        caller_problems(&calling),
        vec!["release.yml: calls js-server-deploy.yml, which no other workflow calls".to_owned()]
    );
    // A workflow that is not YAML is not a caller (the YAML reader's own test names it).
    assert_eq!(
        caller_problems(&[("t.yml".to_owned(), "a: b: c\n".to_owned())]),
        Vec::<String>::new()
    );
}
