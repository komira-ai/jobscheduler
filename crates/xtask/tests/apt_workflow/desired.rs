//! `hostkeeper-desired.yml`: the run that sets the desired version of the hostkeeper service
//! `js-agent` on the hostkeeper coordinator for a published release, by its targets, after each
//! release run that published one and by hand, once `cargo xtask release desired` has refused a
//! release whose .debs the stable index does not list.
//!
//! [`desired_problems`] refuses the file when:
//! - anything but `workflow_run` and `workflow_dispatch` starts it; the `workflow_run` is not the
//!   completion of `release` on `main`; or the dispatch's inputs are not the one required string
//!   `version`;
//! - its concurrency is not [`GROUP`] with `cancel-in-progress: false` (a run by hand is never
//!   replaced by the completion of a release run while it waits, nor any run cancelled);
//! - its jobs are not `check` and `set-desired`; `check`'s `if:` is not [`DESIRED_IF`] (the
//!   repository variable is the gate, at the job level, only `main` runs it, and a release run is
//!   acted on only when it succeeded, on `main` of this repository) or `set-desired`'s is not
//!   [`SET_IF`] (the same gate, and `check` said `set=true`);
//! - `check` needs a job, holds other than `actions: read` and `contents: read`, does not run
//!   [`CHECK`] as its step `desired` with `VERSION` and `RUN_ID` read from the input and from the
//!   run that started it, or does not hand on that step's three outputs;
//! - `set-desired` needs anything but `check`, is not hostkeeper's reusable workflow
//!   ([`SET_DESIRED`]) pinned to [`CLIENT_COMMIT`] with the comment `# v`[`CLIENT_RELEASE`],
//!   holds other than `contents: read` and `id-token: write`, or its inputs are not exactly those
//!   [`with_wanted`] gives, no more: the service, what `check` handed on, the hostkeeper release
//!   the pin names, the coordinator's two repository variables, the audience, `wait: false`, and
//!   the tailnet's inputs from their repository variables. There is no `host_group` input (the
//!   reusable workflow has none yet: the coordinator's placement of the service picks the hosts).
//!
//! [`coordinator_problems`] refuses a workflow that reads a coordinator setting other than
//! hostkeeper's two variables ([`COORDINATOR_VARS`]) and the tailnet's name: the version is set
//! through hostkeeper's coordinator and nothing else (release.yml's gamma step reads
//! `JS_COORDINATOR_URL` and `JS_COORDINATOR_CA`, the kbf coordinator's, and is the one workflow
//! allowed to). [`caller_problems`] refuses any other workflow that calls this one.

use super::support::{at, entries, entry, job, names, permissions, steps_of};
use super::yaml::{self, Node};

/// The workflow's file.
pub const FILE: &str = "hostkeeper-desired.yml";

/// The `if:` of `check`: a run of `main` while the repository variable is `true`, by hand, or
/// after a run of `release.yml` that succeeded on `main` of this repository.
pub const DESIRED_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.JS_AGENT_SETS_DESIRED == 'true' && (github.event_name == 'workflow_dispatch' || (github.event_name == 'workflow_run' && github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository)) }}";

/// The concurrency group: one for the runs by hand, one for the runs after a release.
pub const GROUP: &str =
    "hostkeeper-desired-${{ github.event_name == 'workflow_dispatch' && 'dispatch' || 'release' }}";

/// The `if:` of `set-desired`: the gate again, and `check` chose a release.
pub const SET_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.JS_AGENT_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' }}";

/// The command of the step `desired` of `check`.
pub const CHECK: &str = "cargo xtask release desired --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --after-run \"$RUN_ID\" --dir \"$RUNNER_TEMP/desired\" --output \"$GITHUB_OUTPUT\"";

/// The outputs `check` hands on, each its step `desired`'s.
pub const OUTPUTS: [&str; 3] = ["set", "package-version", "targets"];

/// hostkeeper's reusable workflow that sets the desired version (the CI client).
pub const SET_DESIRED: &str = "komira-ai/hostkeeper/.github/workflows/set-desired.yml";

/// The revision of the CI client the workflow pins: the commit of hostkeeper's release
/// [`CLIENT_RELEASE`], whose hkctl makes the call. Moving it moves the `job_workflow_ref` of the
/// coordinator's allow-list entry, so it is a reviewed change here.
pub const CLIENT_COMMIT: &str = "318bcc93c9bcd009d40597c45227278742acc622";

/// The hostkeeper release of [`CLIENT_COMMIT`], whose hkctl makes the call.
pub const CLIENT_RELEASE: &str = "0.1.242";

/// The audience of the job's OIDC token, which the coordinator's allow-list entry names.
pub const AUDIENCE: &str = "jobscheduler-hostkeeper-desired";

/// The repository variables of the coordinator: its address and its CA.
pub const COORDINATOR_VARS: [&str; 2] = ["HOSTKEEPER_COORDINATOR_URL", "HOSTKEEPER_COORDINATOR_CA"];

/// The repository variable of the coordinator's name on the tailnet, which the join pings.
pub const TAILNET_COORDINATOR: &str = "HOSTKEEPER_TAILNET_COORDINATOR";

/// The coordinator variables release.yml's gamma step reads: the kbf update coordinator's.
pub const GAMMA_VARS: [&str; 2] = ["JS_COORDINATOR_URL", "JS_COORDINATOR_CA"];

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

/// One line per way `hostkeeper-desired.yml` (`text`) is not the run described above.
pub fn desired_problems(text: &str) -> Vec<String> {
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
    if found != ["check", "set-desired"] {
        problems.push(format!("jobs are {found:?}, not check and set-desired"));
    }
    for (name, want) in [("check", DESIRED_IF), ("set-desired", SET_IF)] {
        let condition = job(jobs, name).and_then(|j| at(j, &["if"]));
        if condition != Some(want) {
            problems.push(format!("{name}: `if` is {condition:?}, not {want}"));
        }
    }
    problems.extend(check_problems(job(jobs, "check")));
    problems.extend(set_desired_problems(text, job(jobs, "set-desired")));
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
    let desired = all.iter().find(|s| at(s, &["id"]) == Some("desired"));
    if desired.and_then(|s| at(s, &["run"])).map(str::trim_end) != Some(CHECK) {
        problems.push(format!("check: the step `desired` does not run `{CHECK}`"));
    }
    for (var, want) in [
        ("VERSION", "${{ inputs.version }}"),
        ("RUN_ID", "${{ github.event.workflow_run.id }}"),
        ("GH_TOKEN", "${{ github.token }}"),
    ] {
        if desired.and_then(|s| at(s, &["env", var])) != Some(want) {
            problems.push(format!(
                "check: the step `desired` does not read {var} from {want}"
            ));
        }
    }
    for output in OUTPUTS {
        let got = check.and_then(|j| at(j, &["outputs", output]));
        let want = format!("${{{{ steps.desired.outputs.{output} }}}}");
        if got != Some(want.as_str()) {
            problems.push(format!("check: output {output} is {got:?}, not {want}"));
        }
    }
    let given = check
        .and_then(|j| entry(j, "outputs"))
        .map_or(0, |o| entries(o).len());
    if given != OUTPUTS.len() {
        problems.push(format!(
            "check: it hands on {given} outputs, not the {} of the step `desired`",
            OUTPUTS.len()
        ));
    }
    problems
}

/// What each input of hostkeeper's workflow must be, and no other input is given.
pub fn with_wanted() -> [(&'static str, &'static str); 14] {
    [
        ("service", "js-agent"),
        ("version", "${{ needs.check.outputs.package-version }}"),
        ("targets", "${{ needs.check.outputs.targets }}"),
        ("hostkeeper_version", CLIENT_RELEASE),
        ("hostkeeper_commit", CLIENT_COMMIT),
        ("coordinator_url", "${{ vars.HOSTKEEPER_COORDINATOR_URL }}"),
        ("coordinator_ca", "${{ vars.HOSTKEEPER_COORDINATOR_CA }}"),
        ("audience", AUDIENCE),
        ("wait", "false"),
        (
            "join_tailnet",
            "${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID != '' }}",
        ),
        (
            "tailnet_client_id",
            "${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID }}",
        ),
        (
            "tailnet_audience",
            "${{ vars.HOSTKEEPER_TAILNET_AUDIENCE }}",
        ),
        (
            "tailnet_coordinator",
            "${{ vars.HOSTKEEPER_TAILNET_COORDINATOR }}",
        ),
        ("tailnet_tag", "${{ vars.HOSTKEEPER_TAILNET_TAG }}"),
    ]
}

/// One line per way the job `set-desired` is not hostkeeper's workflow, called with the release
/// `check` handed on.
fn set_desired_problems(text: &str, job: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let needs = names(job.and_then(|j| entry(j, "needs")));
    if needs != ["check"] {
        problems.push(format!("set-desired: needs are {needs:?}, not [\"check\"]"));
    }
    let held = permissions(job);
    if held != ["contents: read", "id-token: write"] {
        problems.push(format!(
            "set-desired: permissions are {held:?}, not contents: read and id-token: write"
        ));
    }
    let uses = job.and_then(|j| entry(j, "uses"));
    let want = format!("{SET_DESIRED}@{CLIENT_COMMIT}");
    if uses.and_then(Node::as_str) != Some(want.as_str()) {
        problems.push(format!("set-desired: `uses` is not {want}"));
    }
    // The version comment of the pin, on the line of `uses:`.
    let release = uses
        .and_then(|u| text.lines().nth(u.line() - 1))
        .and_then(|line| line.split_once(" # v"))
        .map(|(_, version)| version.trim_end());
    if release != Some(CLIENT_RELEASE) {
        problems.push(format!(
            "set-desired: the pin's comment names {release:?}, not v{CLIENT_RELEASE}"
        ));
    }
    let wanted = with_wanted();
    for (key, want) in wanted {
        let got = job.and_then(|j| at(j, &["with", key]));
        if got != Some(want) {
            problems.push(format!("set-desired: with.{key} is {got:?}, not {want:?}"));
        }
    }
    // No other input: not `host_group`, which the reusable workflow does not take yet, not
    // `package_digest`, which the client refuses beside the targets, and not `max_polls`, which
    // only a wait reads.
    let given = job.and_then(|j| entry(j, "with")).map_or(&[][..], entries);
    for (key, _) in given {
        let key = key.as_str().unwrap_or_default();
        if !wanted.iter().any(|(k, _)| *k == key) {
            problems.push(format!("set-desired: with.{key} is not an input it gives"));
        }
    }
    problems
}

/// One line per place in the workflow `file` (`text`) that reads a coordinator variable other
/// than [`COORDINATOR_VARS`], [`TAILNET_COORDINATOR`] and, in release.yml, [`GAMMA_VARS`].
pub fn coordinator_problems(file: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for read in line.split("vars.").skip(1) {
            let name: String = read
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let gamma = file == "release.yml" && GAMMA_VARS.contains(&name.as_str());
            if name.contains("COORDINATOR")
                && !COORDINATOR_VARS.contains(&name.as_str())
                && name != TAILNET_COORDINATOR
                && !gamma
            {
                problems.push(format!(
                    "{file}: line {}: reads vars.{name}, not one of hostkeeper's {COORDINATOR_VARS:?}",
                    n + 1
                ));
            }
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
