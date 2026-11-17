//! `hostkeeper-desired.yml`: the run that sets the desired version of the hostkeeper service
//! `js-agent` on the hostkeeper coordinator for a published release, by its targets, started by
//! release.yml's job `desired` for the release its run published, or by hand, once `cargo xtask
//! release desired` has refused a release whose .debs the stable index does not list.
//!
//! [`desired_problems`] refuses the file when:
//! - anything but `workflow_dispatch` starts it (a `workflow_run` trigger is refused: the run is
//!   started by release.yml's `desired`, in release order), or the dispatch's inputs are not the
//!   one required string `version`;
//! - its concurrency is not [`GROUP`] with `cancel-in-progress: false` (one run at a time; a running
//!   run is never cancelled, and a waiting one is replaced by a newer one);
//! - its jobs are not `check`, `set-desired` and `set-desired-mac`; `check`'s `if:` is not
//!   [`DESIRED_IF`] (the repository variable is the gate, at the job level, only `main` runs it,
//!   and only a dispatch does),
//!   `set-desired`'s is not [`SET_IF`] (the same gate, and `check` said `set=true`) or
//!   `set-desired-mac`'s is not [`SET_MAC_IF`] (that, and `check` said `set-mac=true`);
//! - `check` needs a job, holds other than `actions: read` and `contents: read`, does not run
//!   [`CHECK`] as its step `desired` with `VERSION` read from the input, `GH_TOKEN` from the
//!   token (no `RUN_ID`: no run of another workflow is waited for) and `PKG_URL` the package repository's address ([`PKG_URL`]), or does
//!   not hand on that step's four outputs;
//! - `set-desired` needs anything but `check` (and `set-desired-mac` anything but `check` and
//!   `set-desired`, so the Macs are set after the Linux nodes), is not hostkeeper's reusable
//!   workflow
//!   ([`SET_DESIRED`]) pinned to [`CLIENT_COMMIT`] with the comment `# v`[`CLIENT_RELEASE`],
//!   holds other than `contents: read` and `id-token: write`, or its inputs are not exactly those
//!   [`with_wanted`] gives, no more: the service, the host group ([`HOST_GROUP`] for
//!   `set-desired`, [`HOST_GROUP_MAC`] for `set-desired-mac`), what `check` handed on (the same
//!   version and targets to both), the hostkeeper release the pin names, the coordinator's two
//!   repository variables, the audience, `wait: false`, and the tailnet's inputs from their
//!   repository variables.
//!
//! [`coordinator_problems`] refuses a workflow that reads a coordinator setting other than
//! hostkeeper's two variables ([`COORDINATOR_VARS`]) and the tailnet's name: the version is set
//! through hostkeeper's coordinator and nothing else (release.yml's gamma step reads
//! `JS_COORDINATOR_URL` and `JS_COORDINATOR_CA`, the kbf coordinator's, and is the one workflow
//! allowed to). [`caller_problems`] refuses any other workflow that calls this one with `uses:`,
//! and any `gh workflow run` of it outside release.yml's job `desired` (the one job that starts it,
//! with `actions: write`).

use super::support::{at, entries, entry, job, names, permissions, steps_of};
use super::yaml::{self, Node};

/// The workflow's file.
pub const FILE: &str = "hostkeeper-desired.yml";

/// The `if:` of `check`: a dispatch from `main` while the repository variable is `true`.
pub const DESIRED_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.JS_AGENT_SETS_DESIRED == 'true' && github.event_name == 'workflow_dispatch' }}";

/// The concurrency group: one for every run.
pub const GROUP: &str = "hostkeeper-desired";

/// The `if:` of `set-desired`: the gate again, and `check` chose a release.
pub const SET_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.JS_AGENT_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' }}";

/// The `if:` of `set-desired-mac`: the gate again, `check` chose a release, and that release has a
/// signed macOS package in the package repository (`set-mac`).
pub const SET_MAC_IF: &str = "${{ github.ref == 'refs/heads/main' && vars.JS_AGENT_SETS_DESIRED == 'true' && needs.check.outputs.set == 'true' && needs.check.outputs.set-mac == 'true' }}";

/// The command of the step `desired` of `check`.
pub const CHECK: &str = "cargo xtask release desired --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --pkg-url \"$PKG_URL\" --dir \"$RUNNER_TEMP/desired\" --output \"$GITHUB_OUTPUT\"";

/// The base of the package repository's public address of the macOS packages
/// (`<base>/<version>/SHA256SUMS` and the package), read by `--pkg-url`. A release before the
/// package repository had one has no such directory, and `check` hands on `set-mac=false`.
pub const PKG_URL: &str = "https://packages.komira.ai/pkg/jobscheduler";

/// The outputs `check` hands on, each its step `desired`'s.
pub const OUTPUTS: [&str; 4] = ["set", "set-mac", "package-version", "targets"];

/// hostkeeper's reusable workflow that sets the desired version (the CI client).
pub const SET_DESIRED: &str = "komira-ai/hostkeeper/.github/workflows/set-desired.yml";

/// The revision of the CI client the workflow pins: the commit of hostkeeper's release
/// [`CLIENT_RELEASE`], whose hkctl makes the call. Moving it moves the `job_workflow_ref` of the
/// coordinator's allow-list entry, so it is a reviewed change here.
pub const CLIENT_COMMIT: &str = "30141591c292d8881ce5bc16c9d0b3ba7fcfdca1";

/// The hostkeeper release of [`CLIENT_COMMIT`], whose hkctl makes the call.
pub const CLIENT_RELEASE: &str = "0.1.317";

/// The host group the version is set in: the six Linux build nodes. Naming it makes the call one
/// deployment to that group, not one to each group that lists js-agent.
pub const HOST_GROUP: &str = "all-in-one";

/// The host group of the Macs, which `set-desired-mac` sets after the Linux nodes.
pub const HOST_GROUP_MAC: &str = "mac-build";

/// The audience of the job's OIDC token, which the coordinator's allow-list entry names.
pub const AUDIENCE: &str = "jobscheduler-hostkeeper-desired";

/// The repository variables of the coordinator: its address and its CA.
pub const COORDINATOR_VARS: [&str; 2] = ["HOSTKEEPER_COORDINATOR_URL", "HOSTKEEPER_COORDINATOR_CA"];

/// The repository variable of the coordinator's name on the tailnet, which the join pings.
pub const TAILNET_COORDINATOR: &str = "HOSTKEEPER_TAILNET_COORDINATOR";

/// The coordinator variables release.yml's gamma step reads: the kbf update coordinator's.
pub const GAMMA_VARS: [&str; 2] = ["JS_COORDINATOR_URL", "JS_COORDINATOR_CA"];

/// One line per way the triggers of the workflow (its `on`) are not a dispatch with the one input
/// `version` and nothing else.
fn trigger_problems(on: Option<&Node>) -> Vec<String> {
    let mut problems = Vec::new();
    let triggers = names(on);
    if triggers != ["workflow_dispatch"] {
        problems.push(format!(
            "triggers are {triggers:?}, not workflow_dispatch alone"
        ));
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
    if found != ["check", "set-desired", "set-desired-mac"] {
        problems.push(format!(
            "jobs are {found:?}, not check, set-desired and set-desired-mac"
        ));
    }
    for (name, want) in [
        ("check", DESIRED_IF),
        ("set-desired", SET_IF),
        ("set-desired-mac", SET_MAC_IF),
    ] {
        let condition = job(jobs, name).and_then(|j| at(j, &["if"]));
        if condition != Some(want) {
            problems.push(format!("{name}: `if` is {condition:?}, not {want}"));
        }
    }
    problems.extend(check_problems(job(jobs, "check")));
    problems.extend(set_desired_problems(
        text,
        "set-desired",
        HOST_GROUP,
        &["check"],
        job(jobs, "set-desired"),
    ));
    problems.extend(set_desired_problems(
        text,
        "set-desired-mac",
        HOST_GROUP_MAC,
        &["check", "set-desired"],
        job(jobs, "set-desired-mac"),
    ));
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
        ("GH_TOKEN", "${{ github.token }}"),
        ("PKG_URL", PKG_URL),
    ] {
        if desired.and_then(|s| at(s, &["env", var])) != Some(want) {
            problems.push(format!(
                "check: the step `desired` does not read {var} from {want}"
            ));
        }
    }
    // The step reads no run id: it waits for no run of another workflow.
    if desired.is_some_and(|s| at(s, &["env", "RUN_ID"]).is_some()) {
        problems.push("check: the step `desired` reads RUN_ID, which it has no use for".to_owned());
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

/// What each input of hostkeeper's workflow must be for the host group `host_group`, and no other
/// input is given.
pub fn with_wanted(host_group: &str) -> [(&str, &str); 15] {
    [
        ("service", "js-agent"),
        ("host_group", host_group),
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

/// One line per way the job `name` (`set-desired` or `set-desired-mac`) is not hostkeeper's
/// workflow, called for the host group `host_group` with the release `check` handed on, after
/// the jobs `after`.
fn set_desired_problems(
    text: &str,
    name: &str,
    host_group: &str,
    after: &[&str],
    job: Option<&Node>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let needs = names(job.and_then(|j| entry(j, "needs")));
    if needs != after {
        problems.push(format!("{name}: needs are {needs:?}, not {after:?}"));
    }
    let held = permissions(job);
    if held != ["contents: read", "id-token: write"] {
        problems.push(format!(
            "{name}: permissions are {held:?}, not contents: read and id-token: write"
        ));
    }
    let uses = job.and_then(|j| entry(j, "uses"));
    let want = format!("{SET_DESIRED}@{CLIENT_COMMIT}");
    if uses.and_then(Node::as_str) != Some(want.as_str()) {
        problems.push(format!("{name}: `uses` is not {want}"));
    }
    // The version comment of the pin, on the line of `uses:`.
    let release = uses
        .and_then(|u| text.lines().nth(u.line() - 1))
        .and_then(|line| line.split_once(" # v"))
        .map(|(_, version)| version.trim_end());
    if release != Some(CLIENT_RELEASE) {
        problems.push(format!(
            "{name}: the pin's comment names {release:?}, not v{CLIENT_RELEASE}"
        ));
    }
    let wanted = with_wanted(host_group);
    for (key, want) in wanted {
        let got = job.and_then(|j| at(j, &["with", key]));
        if got != Some(want) {
            problems.push(format!("{name}: with.{key} is {got:?}, not {want:?}"));
        }
    }
    // No other input: not `package_digest`, which the client refuses beside the targets, and not
    // `max_polls`, which only a wait reads.
    let given = job.and_then(|j| entry(j, "with")).map_or(&[][..], entries);
    for (key, _) in given {
        let key = key.as_str().unwrap_or_default();
        if !wanted.iter().any(|(k, _)| *k == key) {
            problems.push(format!("{name}: with.{key} is not an input it gives"));
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

/// The one job of release.yml that starts [`FILE`], with `gh workflow run`.
pub const STARTER: (&str, &str) = ("release.yml", "desired");

/// One line per workflow of `files` (name, text), other than [`FILE`], with a job that calls
/// [`FILE`] (`uses:`): none does; it starts by `workflow_dispatch`. And one line per place that
/// starts it with `gh workflow run` outside the job [`STARTER`] (release.yml's `desired`), a
/// comment excepted.
pub fn caller_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems: Vec<String> = files
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
        .collect();
    let starts = format!("gh workflow run {FILE}");
    for (name, text) in files {
        let mut in_job = "";
        let mut in_jobs = false;
        for (n, line) in text.lines().enumerate() {
            if line == "jobs:" {
                in_jobs = true;
            } else if let Some(job) = line
                .strip_prefix("  ")
                .and_then(|l| l.strip_suffix(':'))
                .filter(|l| in_jobs && !l.starts_with([' ', '#']))
            {
                in_job = job;
            }
            if line.contains(&starts)
                && !line.trim_start().starts_with('#')
                && (name.as_str(), in_job) != STARTER
            {
                problems.push(format!(
                    "{name}: line {}: starts {FILE} outside the job `desired` of release.yml",
                    n + 1
                ));
            }
        }
    }
    problems
}
