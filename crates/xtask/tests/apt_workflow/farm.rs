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
//! farm-connect's pin and inputs, and the upload of a stale table's patch; and through
//! `super::farm_steps` the steps that reach the build farm (each buck2 command through komira's
//! buck2-farm action, under its watchdog) and the time limit of every step. The steps that install
//! the pinned tools around `cargo xtask fetch` are `tests/pinned_tool_cache`'s:
//! - its `on` is exactly `pull_request` with no filter, `push` to `branches: [main]` and
//!   `workflow_call` with the one input `target` (a required string: release.yml's `blackbox`
//!   calls it with the target it builds in place of `//:ci`), and its top-level `permissions` are
//!   `{}`;
//! - its jobs are exactly [`JOB`], on [`RUNNER`], under [`JOB_IF`], with `permissions` exactly
//!   `contents: read` and `id-token: write`;
//! - farm-connect is used by exactly one step, pinned by a 40-digit commit, whose `with:` is
//!   exactly [`CONNECT_INPUTS`]: the tailnet identity (`TS_CLIENT_ID`, `TS_AUDIENCE`) and the
//!   farm's address, instance, worker property set and TLS switch, each from the organisation's
//!   variable of its name, and nothing else (no secret, no token, no input a pull request could
//!   set);
//! - the steps of `super::farm_steps::step_problems` and `super::farm_steps::timeout_problems`;
//! - exactly one step uploads the artifact [`PATCH_ARTIFACT`], after the table's check (which
//!   writes it to [`patch_file`] when the table is stale), under exactly [`UPLOAD_IF`] (a step
//!   failed: the patch exists only then), with `actions/upload-artifact` and a `with:` of exactly
//!   [`upload_inputs`]: that artifact name and the check's patch file, so the command the summary
//!   prints downloads what was uploaded.
//!
//! [`token_problems`] holds every other workflow to the same scope: in a workflow that starts on
//! `pull_request`, `pull_request_target` or `merge_group` (a merge group of the merge queue runs a
//! queued pull request's code, docs/ci.md), no job and no top level holds `id-token: write`
//! unless the job's `if:` pins a push to main or a manual run (its top-level `&&` conjuncts, or a
//! parenthesised `||` of such, are read: `github.event_name == 'push' || github.event_name ==
//! 'pull_request'` pins nothing), as the jobs of `release.yml` that sign and publish do. The jobs
//! of a workflow such a workflow calls are held to it too, unless the call or the job pins one.
//! In `artifacts.yml`, the signer, no job but `attest` and a call of another workflow file holds
//! the token, and in `macos-farm.yml` a job that holds it runs no repository code.

use std::collections::BTreeMap;

pub use xtask::farm::table::{PATCH_ARTIFACT, PATCH_FILE};

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
/// The action that uploads the patch, at any commit.
pub const UPLOAD: &str = "actions/upload-artifact@";
/// The exact `if:` of the upload of the patch.
pub const UPLOAD_IF: &str = "${{ failure() }}";

/// The patch file the table's check writes, below the runner's temporary directory, as its step's
/// shell names it and as the upload's `with:` does.
pub fn patch_file(shell: bool) -> String {
    let temp = if shell {
        "$RUNNER_TEMP"
    } else {
        "${{ runner.temp }}"
    };
    format!("{temp}/{PATCH_ARTIFACT}/{PATCH_FILE}")
}

/// The `with:` of the upload of the patch, exactly.
pub fn upload_inputs() -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        ("name", PATCH_ARTIFACT.to_owned()),
        ("path", patch_file(false)),
        ("if-no-files-found", "ignore".to_owned()),
        ("retention-days", "30".to_owned()),
    ])
}

/// What a [`FILE`] whose `on` is anything else is refused with.
pub const ON_PROBLEM: &str = "farm.yml: on must be pull_request with no filter, push to branches [main] and workflow_call with the one required string input target, alone";
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

/// Whether `on` is exactly `pull_request` with no filter, `push` with `branches: [main]` and
/// `workflow_call` with the one input `target`, a required string, and nothing else but its
/// description.
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
    let called = entry(on, "workflow_call").is_some_and(|call| match entries(call) {
        [(key, inputs)] => key.as_str() == Some("inputs") && one_target(inputs),
        _ => false,
    });
    names(Some(on)) == ["pull_request", "push", "workflow_call"] && unfiltered && to_main && called
}

/// Whether `inputs` is the one input `target`: a required string, with a description.
fn one_target(inputs: &Node) -> bool {
    match entries(inputs) {
        [(key, target)] => {
            key.as_str() == Some("target")
                && scalar_map(target).is_some_and(|m| {
                    m.keys().copied().collect::<Vec<_>>() == ["description", "required", "type"]
                        && m.get("type") == Some(&"string")
                        && m.get("required") == Some(&"true")
                })
        }
        _ => false,
    }
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
    problems.extend(super::farm_steps::step_problems(job));
    problems.extend(super::farm_steps::timeout_problems(job));
    let table = all
        .iter()
        .position(|s| entry(s, "run").and_then(Node::as_str) == Some(super::farm_steps::TABLE_RUN));
    problems.extend(upload_problems(all, table));
    problems
}

/// Whether `step` uploads the artifact [`PATCH_ARTIFACT`].
fn uploads_patch(step: &Node) -> bool {
    entry(step, "uses")
        .and_then(Node::as_str)
        .is_some_and(|u| u.starts_with(UPLOAD))
        && entry(step, "with")
            .and_then(|with| entry(with, "name"))
            .and_then(Node::as_str)
            == Some(PATCH_ARTIFACT)
}

/// What is wrong with the upload of the patch among the steps `all`, the table's check at
/// `table`.
fn upload_problems(all: &[Node], table: Option<usize>) -> Vec<String> {
    let uploads: Vec<usize> = (0..all.len()).filter(|&i| uploads_patch(&all[i])).collect();
    let [at] = uploads[..] else {
        return vec![format!(
            "job {JOB}: exactly one step must upload the artifact {PATCH_ARTIFACT} with {UPLOAD}<commit>"
        )];
    };
    let step = &all[at];
    let mut problems = Vec::new();
    if table.is_some_and(|t| t > at) {
        problems.push(format!(
            "job {JOB}: the upload of {PATCH_ARTIFACT} does not come after the table's check"
        ));
    }
    if entry(step, "if").and_then(Node::as_str) != Some(UPLOAD_IF) {
        problems.push(format!(
            "job {JOB}: the upload of {PATCH_ARTIFACT} must run under exactly `if: {UPLOAD_IF}`"
        ));
    }
    let with = entry(step, "with").and_then(scalar_map);
    let wanted = upload_inputs();
    let wanted: BTreeMap<&str, &str> = wanted.iter().map(|(k, v)| (*k, v.as_str())).collect();
    if with.as_ref() != Some(&wanted) {
        problems.push(format!(
            "job {JOB}: the upload of {PATCH_ARTIFACT}'s `with:` must be exactly {wanted:?}"
        ));
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

/// The workflow whose signing the farm hosts trust, the one file whose jobs may not hold
/// `id-token: write` but for [`SIGNER_JOB`] and the calls of other workflow files.
pub const SIGNER_FILE: &str = "artifacts.yml";
/// The one job of [`SIGNER_FILE`] that holds the token to sign.
pub const SIGNER_JOB: &str = "attest";
/// The workflow that builds on the build farm's Macs, whose jobs hold the token to join the
/// farm's network and so run no code compiled from the repository.
pub const FARM_BUILD_FILE: &str = "macos-farm.yml";
/// The prefix of the `uses:` of a call of a workflow of this repository.
const LOCAL_CALL: &str = "./.github/workflows/";
/// What the `if:` of a job that holds the token must pin: a push together with [`MAIN_REF`], or a
/// manual run.
const PUSH: &str = "github.event_name == 'push'";
/// The ref a push must be to, in the same `&&` conjunction as [`PUSH`].
const MAIN_REF: &str = "github.ref == 'refs/heads/main'";
const DISPATCH: &str = "github.event_name == 'workflow_dispatch'";

/// The parts of `text` between the occurrences of `operator` outside parentheses and single
/// quotes, trimmed.
fn split_top<'a>(text: &'a str, operator: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut from) = (0i32, false, 0);
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let c = rest.chars().next().unwrap_or_default();
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            _ if !quoted && depth == 0 && rest.starts_with(operator) => {
                parts.push(text[from..at].trim());
                at += operator.len();
                from = at;
                continue;
            }
            _ => {}
        }
        at += c.len_utf8();
    }
    parts.push(text[from..].trim());
    parts
}

/// `text` without the one pair of parentheses that encloses all of it, if it has one.
fn unparenthesised(text: &str) -> &str {
    let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) else {
        return text;
    };
    // `(a) && (b)` starts and ends with a parenthesis but is not one group: the first group must
    // close at the end.
    let mut depth = 0i32;
    let mut quoted = false;
    for c in inner.chars() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return text;
        }
    }
    inner.trim()
}

/// The `&&` conjuncts of `text` (one branch, no top-level `||`), with each parenthesised conjunct
/// that has no `||` of its own replaced by its conjuncts; a parenthesised `||` stays whole.
fn conjuncts(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for conjunct in split_top(text, "&&") {
        let inner = unparenthesised(conjunct);
        if inner != conjunct && split_top(inner, "||").len() == 1 {
            found.extend(conjuncts(inner));
        } else {
            found.push(conjunct);
        }
    }
    found
}

/// Whether the condition `expression` (the text of an `if:`, with or without `${{ }}`) holds only
/// on a push to main or a manual run: every `||` branch of it does, and a branch does when its
/// top-level `&&` conjuncts include [`DISPATCH`], or both [`PUSH`] and [`MAIN_REF`], or a
/// parenthesised condition that does.
fn pins_push_or_dispatch(expression: &str) -> bool {
    let text = expression.trim();
    let text = text
        .strip_prefix("${{")
        .and_then(|t| t.strip_suffix("}}"))
        .unwrap_or(text)
        .trim();
    let text = unparenthesised(text);
    let branches = split_top(text, "||");
    if branches.len() > 1 {
        return branches.into_iter().all(pins_push_or_dispatch);
    }
    let all = conjuncts(text);
    (all.contains(&PUSH) && all.contains(&MAIN_REF))
        || all.iter().any(|conjunct| {
            *conjunct == DISPATCH
                || (unparenthesised(conjunct) != *conjunct
                    && pins_push_or_dispatch(unparenthesised(conjunct)))
        })
}

/// Whether the job `job` pins a push or a manual run in its `if:`.
fn job_pins(job: &Node) -> bool {
    entry(job, "if")
        .and_then(Node::as_str)
        .is_some_and(pins_push_or_dispatch)
}

/// The file of this repository that the job `job` calls, if it calls one.
fn local_call(job: &Node) -> Option<&str> {
    entry(job, "uses")
        .and_then(Node::as_str)
        .and_then(|u| u.strip_prefix(LOCAL_CALL))
}

/// The name of the job `name`, empty when it is not text.
fn text_of_name(name: &Node) -> &str {
    name.as_str().unwrap_or_default()
}

/// What is wrong with the jobs that the call `job` (reached as `label`, from a workflow a pull
/// request starts, `file`) runs: a job of the called workflow that holds the token where neither
/// it nor the chain of calls above it pins a push or a manual run, a call of a workflow that is
/// not in `docs`, and a chain that comes back to a workflow it is inside. `pinned` says whether
/// the chain above already pins one; `inside` is the files the chain is inside.
fn chain_problems(
    file: &str,
    label: &str,
    job: &Node,
    pinned: bool,
    docs: &BTreeMap<&str, Node>,
    inside: &mut Vec<String>,
) -> Vec<String> {
    let Some(called) = local_call(job) else {
        return Vec::new();
    };
    let Some(doc) = docs.get(called) else {
        return vec![format!(
            "{file}: job {label} calls {LOCAL_CALL}{called}, a workflow this lint cannot read"
        )];
    };
    if inside.iter().any(|f| f == called) {
        return vec![format!(
            "{file}: job {label} calls {called}, which the chain is already inside"
        )];
    }
    inside.push(called.to_owned());
    let mut problems = Vec::new();
    for (name, inner) in entry(doc, "jobs").map_or(&[][..], entries) {
        let label = format!("{label} -> {called}/{}", text_of_name(name));
        let pinned = pinned || job_pins(inner);
        if holds_token(entry(inner, "permissions")) && !pinned {
            problems.push(format!(
                "{file}: job {label} holds `id-token: write` on a pull request (neither its `if:` nor the calls above it pin a push to main or a manual run)"
            ));
        }
        problems.extend(chain_problems(file, &label, inner, pinned, docs, inside));
    }
    inside.pop();
    problems
}

/// What is wrong with the grants of `id-token: write` in [`SIGNER_FILE`], the document `doc`: its
/// top level holds none, and no job does but [`SIGNER_JOB`] (which signs) and a call of another
/// workflow file, whose jobs' tokens name that file as their workflow, not this one.
fn signer_problems(file: &str, doc: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if holds_token(entry(doc, "permissions")) {
        problems.push(format!(
            "{file}: holds `id-token: write` at its top level, for every job"
        ));
    }
    for (name, job) in entry(doc, "jobs").map_or(&[][..], entries) {
        let name = text_of_name(name);
        let calls_another = entry(job, "uses")
            .and_then(Node::as_str)
            .is_some_and(|u| u != format!("{LOCAL_CALL}{SIGNER_FILE}"));
        if name != SIGNER_JOB && !calls_another && holds_token(entry(job, "permissions")) {
            problems.push(format!(
                "{file}: job {name} holds `id-token: write`, which only {SIGNER_JOB} and a call of another workflow file may"
            ));
        }
    }
    problems
}

/// Whether `run` (the text of a step's `run:`) invokes `cargo`: a word of it, or the last
/// component of a path, is `cargo`.
fn runs_cargo(run: &str) -> bool {
    run.split(|c: char| c.is_whitespace() || ";&|()\"'`".contains(c))
        .any(|w| w.rsplit('/').next() == Some("cargo"))
}

/// What is wrong with the jobs of [`FARM_BUILD_FILE`], the document `doc`, that hold the token:
/// each pins a push or a manual run itself, and runs none of this repository's code: no local
/// action and no `cargo` at all in a `run:` (the verb runs `cargo fetch` itself), only the binary a
/// job without the token built.
fn farm_build_problems(file: &str, doc: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, job) in entry(doc, "jobs").map_or(&[][..], entries) {
        let name = text_of_name(name);
        if !holds_token(entry(job, "permissions")) {
            continue;
        }
        if !job_pins(job) {
            problems.push(format!(
                "{file}: job {name} holds `id-token: write` and its `if:` does not pin a push to main or a manual run"
            ));
        }
        for step in steps(job) {
            if let Some(action) = entry(step, "uses")
                .and_then(Node::as_str)
                .filter(|u| u.starts_with("./"))
            {
                problems.push(format!(
                    "{file}: job {name} holds `id-token: write` and uses the repository's own action {action}"
                ));
            }
            if entry(step, "run")
                .and_then(Node::as_str)
                .is_some_and(runs_cargo)
            {
                problems.push(format!(
                    "{file}: job {name} holds `id-token: write` and runs `cargo`"
                ));
            }
        }
    }
    problems
}

/// What is wrong with the grants of `id-token: write` in `workflows` (file name, text), other than
/// those of [`FILE`]:
/// - a workflow that starts on a pull request or a merge group holds none at its top level, and a
///   job of it holds one only when its `if:` pins a push to main or a manual run (a real check of
///   the top-level `&&` conjuncts, not a search of the text), as the jobs of `release.yml` that
///   sign and publish do;
/// - the same holds for the jobs of a workflow such a workflow calls (`uses:
///   ./.github/workflows/<file>`, followed to any depth): a called job that holds the token is
///   refused unless it or a call above it pins a push or a manual run;
/// - in [`SIGNER_FILE`], whether or not a pull request starts it, no job but [`SIGNER_JOB`] holds
///   the token except a call of another workflow file (see [`signer_problems`]);
/// - in [`FARM_BUILD_FILE`] a job that holds it runs no repository code (see
///   [`farm_build_problems`]).
pub fn token_problems(workflows: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut docs = BTreeMap::new();
    for (file, text) in workflows {
        match yaml::load(text) {
            Ok(doc) => {
                docs.insert(file.as_str(), doc);
            }
            Err(why) if file != FILE => problems.push(format!("{file}: {why}")),
            Err(_) => {}
        }
    }
    for (file, doc) in docs.iter().filter(|(file, _)| **file != FILE) {
        if *file == SIGNER_FILE {
            problems.extend(signer_problems(file, doc));
        }
        if *file == FARM_BUILD_FILE {
            problems.extend(farm_build_problems(file, doc));
        }
        let on = names(entry(doc, "on"));
        if !on
            .iter()
            .any(|e| e == "pull_request" || e == "pull_request_target" || e == "merge_group")
        {
            continue;
        }
        if holds_token(entry(doc, "permissions")) {
            problems.push(format!(
                "{file}: holds `id-token: write` at its top level, for every job a pull request starts"
            ));
        }
        for (name, job) in entry(doc, "jobs").map_or(&[][..], entries) {
            let name = text_of_name(name);
            let pinned = job_pins(job);
            if holds_token(entry(job, "permissions")) && !pinned {
                problems.push(format!(
                    "{file}: job {name} holds `id-token: write` on a pull request (its `if:` does not pin a push to main or a manual run)"
                ));
            }
            let mut inside = vec![(*file).to_owned()];
            problems.extend(chain_problems(file, name, job, pinned, &docs, &mut inside));
        }
    }
    problems
}
