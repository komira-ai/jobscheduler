//! The rules of `farm.yml` ([`FILE`]), the farm lane: `buck2 build //:ci` on the build farm,
//! additive and advisory beside ci.yml, which stays the pull-request gate (docs/farm.md). Not to be
//! confused with the hosts of the fleet, which `release.yml` installs a release on.
//!
//! It is the one workflow that runs for a pull request with a job that holds `id-token: write`:
//! its job [`JOB`] holds it so that komira's farm-connect action ([`FARM_CONNECT`]) can join the
//! build farm's private network with the job's OIDC token (workload identity federation: no stored
//! secret). Only under its exact `if:` ([`JOB_IF`]: a push to main, or a pull request from a branch
//! of this repository that is not Dependabot's; a fork's pull request gets no token and is
//! skipped).
//!
//! [`problems`] holds the file to its shape: the triggers, the one job, its rights, its `if:`,
//! farm-connect's pin and inputs and the check of the build. It does not fix the other steps of
//! the job (the pinned-tool cache around `cargo xtask fetch` is `tests/pinned_tool_cache`); a
//! change to them is reviewed like any change to a workflow:
//! - its `on` is exactly `pull_request` with no filter and `push` to `branches: [main]`, and its
//!   top-level `permissions` are `{}`;
//! - its jobs are exactly [`JOB`], on [`RUNNER`], under [`JOB_IF`], with `permissions` exactly
//!   `contents: read` and `id-token: write`;
//! - farm-connect is used by exactly one step, pinned by a 40-digit commit, whose `with:` is
//!   exactly [`CONNECT_INPUTS`]: the tailnet identity (`TS_CLIENT_ID`, `TS_AUDIENCE`) and the
//!   farm's address, instance, worker property set and TLS switch, each from the organisation's
//!   variable of its name, and nothing else (no secret, no token, no input a pull request could
//!   set);
//! - a step after farm-connect's runs exactly [`BUILD_RUN`]: `cargo xtask farm-build` of `//:ci`,
//!   which refuses a build with no farm workers or whose summary line does not say `local: 0`
//!   (crates/xtask/src/farm.rs).
//!
//! [`token_problems`] holds every other workflow to the same scope: in a workflow that starts on
//! `pull_request`, `pull_request_target` or `merge_group` (a merge group of the merge queue runs a
//! queued pull request's code, docs/ci.md), no job and no top level holds `id-token: write`
//! unless the job's `if:` pins a push (`github.event_name == 'push'`), as the jobs of `release.yml`
//! that sign and publish do.

use std::collections::BTreeMap;

use super::support::{entries, entry, names, steps};
use super::yaml::{self, Node, Value};

/// The workflow.
pub const FILE: &str = "farm.yml";
/// Its one job, the one that holds `id-token: write` on a pull request.
pub const JOB: &str = "build";
/// The runner of [`JOB`].
pub const RUNNER: &str = "ubuntu-24.04";
/// The exact `if:` of [`JOB`].
pub const JOB_IF: &str = "${{ github.event_name == 'push' || (github.event.pull_request.head.repo.full_name == github.repository && !startsWith(github.head_ref, 'dependabot/')) }}";
/// komira's action that joins the build farm's network and writes its buckconfig.
pub const FARM_CONNECT: &str = "komira-ai/komira/.github/actions/farm-connect";
/// The `with:` of the farm-connect step, exactly.
pub const CONNECT_INPUTS: [(&str, &str); 6] = [
    ("ts-client-id", "${{ vars.TS_CLIENT_ID }}"),
    ("ts-audience", "${{ vars.TS_AUDIENCE }}"),
    ("farm-address", "${{ vars.FARM_ADDRESS }}"),
    ("farm-instance", "${{ vars.FARM_INSTANCE }}"),
    (
        "farm-properties",
        "${{ vars.FARM_LINUX_X86_64_PROPERTIES }}",
    ),
    ("tls", "${{ vars.FARM_TLS || 'false' }}"),
];
/// The build step's `run:`.
pub const BUILD_RUN: &str =
    "cargo xtask farm-build --buck2 \"$RUNNER_TEMP/bin/buck2\" --target //:ci";

/// What a [`FILE`] whose `on` is anything else is refused with.
pub const ON_PROBLEM: &str =
    "farm.yml: on must be pull_request with no filter and push to branches [main] alone";
/// What a [`FILE`] with any top-level permission is refused with.
pub const TOP_PERMISSIONS_PROBLEM: &str = "farm.yml: the top-level permissions must be `{}`";

/// The scalars of the mapping `node`, by key; `None` when a key or value is not a scalar.
fn scalar_map(node: &Node) -> Option<BTreeMap<&str, &str>> {
    match &node.value {
        Value::Map(entries) => entries
            .iter()
            .map(|(k, v)| Some((k.as_str()?, v.as_str()?)))
            .collect(),
        _ => None,
    }
}

/// Whether `on` is exactly `pull_request` with no filter and `push` with `branches: [main]`.
fn pull_request_and_push_to_main(on: Option<&Node>) -> bool {
    let Some(on) = on.filter(|on| matches!(on.value, Value::Map(_))) else {
        return false;
    };
    let unfiltered = entry(on, "pull_request").is_some_and(|v| {
        v.is_empty_map() || (matches!(v.value, Value::Scalar { .. }) && v.as_str().is_none())
    });
    let to_main = entry(on, "push").is_some_and(|push| match entries(push) {
        [(key, branches)] => key.as_str() == Some("branches") && only_main(branches),
        _ => false,
    });
    names(Some(on)) == ["pull_request", "push"] && unfiltered && to_main
}

/// Whether `branches` is the list `[main]`.
fn only_main(branches: &Node) -> bool {
    match &branches.value {
        Value::Seq(items) => matches!(&items[..], [only] if only.as_str() == Some("main")),
        _ => false,
    }
}

/// The `uses:` value of `step`, when it is farm-connect (at any ref, a lookalike name excluded).
fn connect_use(step: &Node) -> Option<&str> {
    entry(step, "uses").and_then(Node::as_str).filter(|u| {
        u.strip_prefix(FARM_CONNECT)
            .is_some_and(|r| r.starts_with('@'))
    })
}

/// Whether `uses` (farm-connect with a ref) is pinned by a 40-digit lowercase commit.
fn pinned_by_commit(uses: &str) -> bool {
    uses.strip_prefix(FARM_CONNECT)
        .and_then(|rest| rest.strip_prefix('@'))
        .is_some_and(|sha| {
            sha.len() == 40 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// What is wrong with the shape of [`FILE`], the document `doc`.
pub fn problems(doc: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if !pull_request_and_push_to_main(entry(doc, "on")) {
        problems.push(ON_PROBLEM.to_owned());
    }
    if !entry(doc, "permissions").is_some_and(Node::is_empty_map) {
        problems.push(TOP_PERMISSIONS_PROBLEM.to_owned());
    }
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    if !jobs.iter().any(|(name, _)| name.as_str() == Some(JOB)) {
        problems.push(format!("{FILE}: no job {JOB}"));
    }
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        if name == JOB {
            problems.extend(job_problems(job));
        } else {
            problems.push(format!("job {name} is not a job of {FILE}"));
        }
    }
    problems
}

/// What is wrong with the job [`JOB`].
fn job_problems(job: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if entry(job, "runs-on").and_then(Node::as_str) != Some(RUNNER) {
        problems.push(format!("job {JOB}: runs-on must be {RUNNER}"));
    }
    if entry(job, "if").and_then(Node::as_str) != Some(JOB_IF) {
        problems.push(format!(
            "job {JOB}: `id-token: write` needs the exact `if: {JOB_IF}`"
        ));
    }
    let wanted = BTreeMap::from([("contents", "read"), ("id-token", "write")]);
    if entry(job, "permissions").and_then(scalar_map).as_ref() != Some(&wanted) {
        problems.push(format!(
            "job {JOB}: permissions must be exactly contents: read and id-token: write"
        ));
    }
    let all = steps(job);
    let connecting: Vec<&Node> = all.iter().filter(|s| connect_use(s).is_some()).collect();
    let inputs = BTreeMap::from(CONNECT_INPUTS);
    match &connecting[..] {
        [only] => {
            if !connect_use(only).is_some_and(pinned_by_commit) {
                problems.push(format!(
                    "job {JOB}: {FARM_CONNECT} must be pinned to a 40-digit lowercase commit SHA"
                ));
            }
            if entry(only, "with").and_then(scalar_map).as_ref() != Some(&inputs) {
                problems.push(format!(
                    "job {JOB}: the farm-connect step's `with:` must be exactly {CONNECT_INPUTS:?}"
                ));
            }
        }
        _ => problems.push(format!(
            "job {JOB}: exactly one step must use {FARM_CONNECT}"
        )),
    }
    let connect = all.iter().position(|s| connect_use(s).is_some());
    let build = all
        .iter()
        .position(|s| entry(s, "run").and_then(Node::as_str) == Some(BUILD_RUN));
    match (connect, build) {
        (_, None) => problems.push(format!("job {JOB}: no step runs `{BUILD_RUN}`")),
        (Some(c), Some(b)) if c < b => {}
        _ => problems.push(format!(
            "job {JOB}: the build does not come after the farm-connect step"
        )),
    }
    problems
}

/// Whether the `permissions` value `permissions` grants `id-token: write`.
fn holds_token(permissions: Option<&Node>) -> bool {
    permissions.is_some_and(|p| {
        p.as_str() == Some("write-all")
            || entry(p, "id-token").and_then(Node::as_str) == Some("write")
    })
}

/// What is wrong with the grants of `id-token: write` in `workflows` (file name, text), other than
/// those of [`FILE`]: a workflow that starts on a pull request or a merge group holds none at its
/// top level, and a job of it holds one only when its `if:` pins a push.
pub fn token_problems(workflows: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (file, text) in workflows.iter().filter(|(file, _)| file != FILE) {
        let doc = match yaml::load(text) {
            Ok(doc) => doc,
            Err(why) => {
                problems.push(format!("{file}: {why}"));
                continue;
            }
        };
        let on = names(entry(&doc, "on"));
        if !on
            .iter()
            .any(|e| e == "pull_request" || e == "pull_request_target" || e == "merge_group")
        {
            continue;
        }
        if holds_token(entry(&doc, "permissions")) {
            problems.push(format!(
                "{file}: holds `id-token: write` at its top level, for every job a pull request starts"
            ));
        }
        for (name, job) in entry(&doc, "jobs").map_or(&[][..], entries) {
            let pins_push = entry(job, "if")
                .and_then(Node::as_str)
                .is_some_and(|c| c.contains("github.event_name == 'push'"));
            if holds_token(entry(job, "permissions")) && !pins_push {
                problems.push(format!(
                    "{file}: job {} holds `id-token: write` on a pull request (its `if:` does not pin a push)",
                    name.as_str().unwrap_or_default()
                ));
            }
        }
    }
    problems
}
