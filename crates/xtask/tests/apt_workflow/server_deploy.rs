//! `js-server-deploy.yml`: the run that deploys a published release of js-lease-server as the
//! service `js-server` of a control plane through the deploy service's reusable workflow, after
//! each release run that published one and by hand, once `cargo xtask release server-deploy` has
//! refused a release whose image or bundle is not in order.
//!
//! [`server_deploy_problems`] refuses the file when:
//! - anything but `workflow_run` and `workflow_dispatch` starts it; the `workflow_run` is not the
//!   completion of `release` on `main`; or the dispatch's inputs are not the one required string
//!   `version`;
//! - its concurrency is not [`GROUP`] with `cancel-in-progress: false`, or the top level grants
//!   any permission;
//! - its jobs are not `check` and `deploy`; `check`'s `if:` is not [`CHECK_IF`] (the repository
//!   variable is the gate, at the job level, only `main` runs it, and a release run is acted on
//!   only when it succeeded, on `main` of this repository) or `deploy`'s is not [`DEPLOY_IF`]
//!   (the same gate, and `check` said `deploy=true`);
//! - `check` needs a job, holds other than `actions: read` and `contents: read` (so no
//!   `id-token` and no write), does not run [`CHECK`] as its step `release` with `VERSION` and
//!   `RUN_ID` read from the input and from the run that started it, or does not hand on that
//!   step's five outputs;
//! - `deploy` needs anything but `check`, is not the deploy service's reusable workflow
//!   ([`DEPLOY_WORKFLOW`]) pinned to [`DEPLOY_COMMIT`], holds other than
//!   `contents: read` and `id-token: write`, sets an `environment` of its own job (the called
//!   workflow's own job enters it), or its inputs are not exactly those [`with_wanted`] gives,
//!   no more.
//!
//! [`caller_problems`] refuses any other workflow that calls this one.

use super::support::{at, entries, entry, job, names, permissions, steps_of};
use super::yaml::{self, Node};

/// The workflow's file.
pub const FILE: &str = "js-server-deploy.yml";

/// The `if:` of `check`: a run of `main` while the repository variable is `true`, by hand, or
/// after a run of `release.yml` that succeeded on `main` of this repository.
pub const CHECK_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.JS_SERVER_DEPLOYS == 'true' && (github.event_name == 'workflow_dispatch' || (github.event_name == 'workflow_run' && github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository)) }}";

/// The concurrency group: one for the runs by hand, one for the runs after a release.
pub const GROUP: &str =
    "js-server-deploy-${{ github.event_name == 'workflow_dispatch' && 'dispatch' || 'release' }}";

/// The `if:` of `deploy`: the gate again, and `check` chose a release.
pub const DEPLOY_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.JS_SERVER_DEPLOYS == 'true' && needs.check.outputs.deploy == 'true' }}";

/// The command of the step `release` of `check`.
pub const CHECK: &str = "cargo xtask release server-deploy --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --after-run \"$RUN_ID\" --dir \"$RUNNER_TEMP/server-deploy\" --output \"$GITHUB_OUTPUT\"";

/// The outputs `check` hands on, each its step `release`'s.
pub const OUTPUTS: [&str; 5] = [
    "deploy",
    "version",
    "image-digest",
    "image-url",
    "bundle-url",
];

/// The deploy service's reusable workflow.
pub const DEPLOY_WORKFLOW: &str = "komira-ai/docker-swarm-updater/.github/workflows/deploy.yml";

/// The revision of [`DEPLOY_WORKFLOW`] the workflow pins. Moving it moves the `job_workflow_ref`
/// the receiving side allows, so it is a reviewed change here.
pub const DEPLOY_COMMIT: &str = "c1fcc47610e2886d4d9e561be3c0c76f0452d1f6";

/// What each input of the deploy service's workflow must be, and no other input is given.
pub fn with_wanted() -> [(&'static str, &'static str); 12] {
    [
        ("service", "js-server"),
        ("version", "${{ needs.check.outputs.version }}"),
        ("image-digest", "${{ needs.check.outputs.image-digest }}"),
        ("image-url", "${{ needs.check.outputs.image-url }}"),
        ("bundle-url", "${{ needs.check.outputs.bundle-url }}"),
        ("endpoint", "${{ vars.CP_DEPLOY_ENDPOINT }}"),
        ("audience", "${{ vars.CP_DEPLOY_AUDIENCE }}"),
        ("environment", "controlplane"),
        ("wait-minutes", "30"),
        (
            "tailscale-oauth-client-id",
            "${{ vars.CP_DEPLOY_TAILNET_CLIENT_ID }}",
        ),
        (
            "tailscale-audience",
            "${{ vars.CP_DEPLOY_TAILNET_AUDIENCE }}",
        ),
        ("tailscale-tags", "${{ vars.CP_DEPLOY_TAILNET_TAGS }}"),
    ]
}

/// One line per way the triggers of the workflow (its `on`) are not the completion of `release`
/// on `main` and a dispatch with the one input `version`.
fn trigger_problems(on: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let triggers = names(on);
    if triggers != ["workflow_dispatch", "workflow_run"] {
        problems.push(format!(
            "triggers are {triggers:?}, not workflow_dispatch and workflow_run"
        ));
    }
    let run = on.and_then(|o| entry(o, "workflow_run"));
    for (key, want) in [
        ("workflows", "release"),
        ("types", "completed"),
        ("branches", "main"),
    ] {
        let got = names(run.and_then(|r| entry(r, key)));
        if got != [want] {
            problems.push(format!("workflow_run.{key} is {got:?}, not [{want:?}]"));
        }
    }
    let inputs = on
        .and_then(|o| entry(o, "workflow_dispatch"))
        .and_then(|w| entry(w, "inputs"));
    let listed = names(inputs);
    if listed != ["version"] {
        problems.push(format!("inputs are {listed:?}, not version alone"));
    }
    let version = inputs.and_then(|i| entry(i, "version"));
    for (key, want) in [("type", "string"), ("required", "true")] {
        let got = version.and_then(|v| at(v, &[key]));
        if got != Some(want) {
            problems.push(format!("input version: {key} is {got:?}, not {want}"));
        }
    }
    problems
}

/// One line per way `js-server-deploy.yml` (`text`) is not the run described above.
pub fn server_deploy_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut problems = trigger_problems(entry(&doc, "on"));
    let concurrency = entry(&doc, "concurrency");
    for (key, want) in [("group", GROUP), ("cancel-in-progress", "false")] {
        let got = concurrency.and_then(|c| at(c, &[key]));
        if got != Some(want) {
            problems.push(format!("concurrency: {key} is {got:?}, not {want}"));
        }
    }
    if entry(&doc, "permissions").is_none_or(|p| !p.is_empty_map()) {
        problems.push("the top level does not grant `permissions: {}`".to_owned());
    }
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let mut found: Vec<&str> = jobs.iter().filter_map(|(k, _)| k.as_str()).collect();
    found.sort_unstable();
    if found != ["check", "deploy"] {
        problems.push(format!("jobs are {found:?}, not check and deploy"));
    }
    for (name, want) in [("check", CHECK_IF), ("deploy", DEPLOY_IF)] {
        let condition = job(jobs, name).and_then(|j| at(j, &["if"]));
        if condition != Some(want) {
            problems.push(format!("{name}: `if` is {condition:?}, not {want}"));
        }
    }
    problems.extend(check_problems(job(jobs, "check")));
    problems.extend(deploy_problems(job(jobs, "deploy")));
    problems
}

/// One line per way the job `check` is not the read-only check that hands on the release.
fn check_problems(check: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let needs = names(check.and_then(|j| entry(j, "needs")));
    if !needs.is_empty() {
        problems.push(format!("check: needs {needs:?}; it needs no job"));
    }
    let held = permissions(check);
    if held != ["actions: read", "contents: read"] {
        problems.push(format!(
            "check: permissions are {held:?}, not actions: read and contents: read"
        ));
    }
    let all = steps_of(check);
    let release = all.iter().find(|s| at(s, &["id"]) == Some("release"));
    if release.and_then(|s| at(s, &["run"])).map(str::trim_end) != Some(CHECK) {
        problems.push(format!("check: the step `release` does not run `{CHECK}`"));
    }
    for (var, want) in [
        ("VERSION", "${{ inputs.version }}"),
        ("RUN_ID", "${{ github.event.workflow_run.id }}"),
        ("GH_TOKEN", "${{ github.token }}"),
    ] {
        if release.and_then(|s| at(s, &["env", var])) != Some(want) {
            problems.push(format!(
                "check: the step `release` does not read {var} from {want}"
            ));
        }
    }
    for output in OUTPUTS {
        let got = check.and_then(|j| at(j, &["outputs", output]));
        let want = format!("${{{{ steps.release.outputs.{output} }}}}");
        if got != Some(want.as_str()) {
            problems.push(format!("check: output {output} is {got:?}, not {want}"));
        }
    }
    let given = check
        .and_then(|j| entry(j, "outputs"))
        .map_or(0, |o| entries(o).len());
    if given != OUTPUTS.len() {
        problems.push(format!(
            "check: it hands on {given} outputs, not the {} of the step `release`",
            OUTPUTS.len()
        ));
    }
    problems
}

/// One line per way the job `deploy` is not the deploy service's workflow, pinned, called with
/// the release `check` handed on.
fn deploy_problems(job: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let needs = names(job.and_then(|j| entry(j, "needs")));
    if needs != ["check"] {
        problems.push(format!("deploy: needs are {needs:?}, not [\"check\"]"));
    }
    let held = permissions(job);
    if held != ["contents: read", "id-token: write"] {
        problems.push(format!(
            "deploy: permissions are {held:?}, not contents: read and id-token: write"
        ));
    }
    let uses = job.and_then(|j| entry(j, "uses")).and_then(Node::as_str);
    let want = format!("{DEPLOY_WORKFLOW}@{DEPLOY_COMMIT}");
    if uses != Some(want.as_str()) {
        problems.push(format!("deploy: `uses` is not {want}"));
    }
    if job.and_then(|j| entry(j, "environment")).is_some() {
        problems.push(
            "deploy: the job sets an `environment`; only the called workflow's job enters it"
                .to_owned(),
        );
    }
    let wanted = with_wanted();
    for (key, want) in wanted {
        let got = job.and_then(|j| at(j, &["with", key]));
        if got != Some(want) {
            problems.push(format!("deploy: with.{key} is {got:?}, not {want:?}"));
        }
    }
    let given = job.and_then(|j| entry(j, "with")).map_or(&[][..], entries);
    for (key, _) in given {
        let key = key.as_str().unwrap_or_default();
        if !wanted.iter().any(|(k, _)| *k == key) {
            problems.push(format!("deploy: with.{key} is not an input it gives"));
        }
    }
    problems
}

/// One line per workflow of `files` (name, text), other than [`FILE`], with a job that calls
/// [`FILE`]: none does; it starts on the completion of `release.yml` by its own trigger.
pub fn caller_problems(files: &[(String, String)]) -> Vec<String> {
    files
        .iter()
        .filter(|(name, _)| name != FILE)
        .filter(|(_, text)| {
            yaml::load(text).is_ok_and(|doc| {
                entry(&doc, "jobs")
                    .map_or(&[][..], entries)
                    .iter()
                    .any(|(_, j)| {
                        entry(j, "uses")
                            .and_then(Node::as_str)
                            .is_some_and(|u| u.contains(FILE))
                    })
            })
        })
        .map(|(name, _)| format!("{name}: calls {FILE}, which no other workflow calls"))
        .collect()
}
