//! The rules for the control-plane deploy of `release.yml`: the jobs `cp-deploy-digest` and
//! `cp-deploy`, which deploy the release's linux-x86_64 image of js-lease-server to the Docker
//! Swarm service `js-server` through docker-swarm-updater. [`cp_deploy_problems`] reads the
//! parsed workflow and names every way the jobs differ from what is written here.
//!
//! - **When.** Both run under [`CP_IF`] alone: a push to main, the plan's release, and the
//!   repository variables `CP_DEPLOY_ENDPOINT` and `CP_DEPLOY_TAILSCALE_TAGS` not empty, so a
//!   repository that sets no endpoint (a fork, or before the deploy front is set up) or no tailnet
//!   tags skips both, and the call never asks the Tailscale join for a node without tags (which
//!   it refuses). `cp-deploy-digest` needs `plan`,
//!   `publish` and `deploy-tool`; `cp-deploy` needs `plan` and `cp-deploy-digest`.
//! - **`cp-deploy-digest`** holds no permission (`permissions: {}`), names no environment and
//!   runs on `ubuntu-24.04`. Its actions are exactly [`DOWNLOADS`] (the `xtask` that
//!   `deploy-tool` built, and the release's files and bundles as `publish` uploaded them), its
//!   runs exactly [`DIGEST_RUNS`], the verb's step has the id `image` and is given the plan's
//!   version alone, and the job's outputs are that step's three, by name. No step names a
//!   secret, a variable or the run's token: it reads artifacts of this run and nothing else.
//! - **`cp-deploy`** calls exactly [`CALLED`] (docker-swarm-updater's deploy workflow at the full
//!   commit the updater's allow-list names) with exactly the inputs [`WITH`] (the three Tailscale
//!   inputs among them: client id, audience and tags), holds
//!   `id-token: write` alone and has no key but [`CALL_KEYS`]: no secret, no matrix, no `env:`.
//! - **Only these two.** No other job names a `CP_DEPLOY_*` variable, runs
//!   `xtask release image-digest` or calls docker-swarm-updater.

use super::support::{at, entries, entry, job, mentions, names, permissions, steps, text};
use super::yaml::Node;

/// The job that reads the image's digest and the release's addresses.
pub const DIGEST: &str = "cp-deploy-digest";
/// The job that calls docker-swarm-updater's deploy workflow.
pub const DEPLOY: &str = "cp-deploy";

/// The `if:` of both jobs.
pub const CP_IF: &str = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' && vars.CP_DEPLOY_ENDPOINT != '' && vars.CP_DEPLOY_TAILSCALE_TAGS != '' }}";

/// The workflow `cp-deploy` calls: docker-swarm-updater's deploy workflow at v0.1.3, the commit
/// its allow-list pins `job_workflow_ref` to.
pub const CALLED: &str = "komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@c1fcc47610e2886d4d9e561be3c0c76f0452d1f6";

/// The inputs `cp-deploy` passes, all of them, sorted by name. The signer inputs are left at the
/// called workflow's defaults: this repository, `.github/workflows/artifacts.yml`,
/// `refs/heads/main`.
pub const WITH: [(&str, &str); 12] = [
    ("audience", "${{ vars.CP_DEPLOY_AUDIENCE }}"),
    (
        "bundle-url",
        "${{ needs.cp-deploy-digest.outputs.bundle-url }}",
    ),
    ("endpoint", "${{ vars.CP_DEPLOY_ENDPOINT }}"),
    ("environment", "controlplane"),
    (
        "image-digest",
        "${{ needs.cp-deploy-digest.outputs.digest }}",
    ),
    (
        "image-url",
        "${{ needs.cp-deploy-digest.outputs.image-url }}",
    ),
    ("service", "js-server"),
    (
        "tailscale-audience",
        "${{ vars.CP_DEPLOY_TAILSCALE_AUDIENCE }}",
    ),
    (
        "tailscale-oauth-client-id",
        "${{ vars.CP_DEPLOY_TAILSCALE_CLIENT_ID }}",
    ),
    ("tailscale-tags", "${{ vars.CP_DEPLOY_TAILSCALE_TAGS }}"),
    ("version", "${{ needs.plan.outputs.version }}"),
    ("wait-minutes", "60"),
];

/// The keys of `cp-deploy`, sorted: it has no runner, steps, secrets, matrix or `env:` of its own.
const CALL_KEYS: [&str; 5] = ["if", "needs", "permissions", "uses", "with"];

/// The action every step of `cp-deploy-digest` that uses one uses.
const DOWNLOAD: &str = "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c";

/// `(artifact, path)` of each download of `cp-deploy-digest`, in order.
pub const DOWNLOADS: [(&str, &str); 3] = [
    ("deploy-tool", "tool"),
    ("release-assets", "dist"),
    ("bundles", "dist"),
];

/// The verb's command line.
pub const DIGEST_RUN: &str = "tool/xtask release image-digest --dist dist --repo \"$GITHUB_REPOSITORY\" --version \"$VERSION\" --output \"$GITHUB_OUTPUT\"";

/// Every `run:` of `cp-deploy-digest`, in order.
const DIGEST_RUNS: [&str; 2] = ["chmod +x tool/xtask", DIGEST_RUN];

/// The outputs of `cp-deploy-digest`, sorted.
const OUTPUTS: [(&str, &str); 3] = [
    ("bundle-url", "${{ steps.image.outputs.bundle-url }}"),
    ("digest", "${{ steps.image.outputs.digest }}"),
    ("image-url", "${{ steps.image.outputs.image-url }}"),
];

/// The `key: value` pairs of the mapping at `key` of `node`, sorted.
fn pairs(node: &Node, key: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = entry(node, key)
        .map_or(&[][..], entries)
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().unwrap_or_default().to_owned(),
                v.as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    found.sort();
    found
}

/// `want` as owned pairs.
fn owned(want: &[(&str, &str)]) -> Vec<(String, String)> {
    want.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// One line per way the control-plane deploy of the `release.yml` document `doc` is not as the
/// module says.
pub fn cp_deploy_problems(doc: &Node) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let mut problems = Vec::new();
    match job(jobs, DIGEST) {
        Some(found) => problems.extend(digest_problems(found)),
        None => problems.push(format!("{DIGEST}: there is no such job")),
    }
    match job(jobs, DEPLOY) {
        Some(found) => problems.extend(deploy_problems(found)),
        None => problems.push(format!("{DEPLOY}: there is no such job")),
    }
    for (name, found) in jobs {
        let name = name.as_str().unwrap_or_default();
        if name == DIGEST || name == DEPLOY {
            continue;
        }
        if mentions(found, "CP_DEPLOY_") {
            problems.push(format!(
                "{name}: names a CP_DEPLOY_ variable; only {DIGEST} and {DEPLOY} may"
            ));
        }
        if mentions(found, "xtask release image-digest") {
            problems.push(format!(
                "{name}: runs `xtask release image-digest`; only {DIGEST} may"
            ));
        }
        if mentions(found, "docker-swarm-updater") {
            problems.push(format!(
                "{name}: names docker-swarm-updater; only {DEPLOY} calls it"
            ));
        }
    }
    problems
}

/// The `needs` and the `if:` of one of the two jobs.
fn gate_problems(name: &str, job: &Node, needs: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    if names(entry(job, "needs")) != needs {
        problems.push(format!("{name}: needs are not {needs:?}"));
    }
    if entry(job, "if").and_then(Node::as_str) != Some(CP_IF) {
        problems.push(format!("{name}: `if` is not {CP_IF}"));
    }
    problems
}

fn digest_problems(job: &Node) -> Vec<String> {
    let mut problems = gate_problems(DIGEST, job, &["deploy-tool", "plan", "publish"]);
    if !entry(job, "permissions").is_some_and(Node::is_empty_map) {
        problems.push(format!("{DIGEST}: permissions are not {{}}"));
    }
    if entry(job, "environment").is_some() {
        problems.push(format!("{DIGEST}: names an environment"));
    }
    if at(job, &["runs-on"]) != Some("ubuntu-24.04") {
        problems.push(format!("{DIGEST}: runs-on is not ubuntu-24.04"));
    }
    let all = steps(job);
    let used: Vec<(&str, &str, &str)> = all
        .iter()
        .filter(|s| entry(s, "uses").is_some())
        .map(|s| {
            (
                text(s, "uses"),
                at(s, &["with", "name"]).unwrap_or_default(),
                at(s, &["with", "path"]).unwrap_or_default(),
            )
        })
        .collect();
    let want: Vec<(&str, &str, &str)> = DOWNLOADS.iter().map(|(n, p)| (DOWNLOAD, *n, *p)).collect();
    if used != want {
        problems.push(format!(
            "{DIGEST}: its actions are {used:?}, not exactly the downloads {DOWNLOADS:?}"
        ));
    }
    let runs: Vec<&str> = all
        .iter()
        .filter(|s| entry(s, "run").is_some())
        .map(|s| text(s, "run").trim_end())
        .collect();
    if runs != DIGEST_RUNS {
        problems.push(format!(
            "{DIGEST}: runs {runs:?}, not exactly {DIGEST_RUNS:?}"
        ));
    }
    match all.iter().find(|s| text(s, "run").trim_end() == DIGEST_RUN) {
        Some(step) => {
            if text(step, "id") != "image" {
                problems.push(format!("{DIGEST}: the verb's step has not the id `image`"));
            }
            if pairs(step, "env") != owned(&[("VERSION", "${{ needs.plan.outputs.version }}")]) {
                problems.push(format!(
                    "{DIGEST}: the verb's step is not given VERSION=${{{{ needs.plan.outputs.version }}}} alone"
                ));
            }
        }
        None => problems.push(format!("{DIGEST}: no step runs `{DIGEST_RUN}`")),
    }
    if entry(job, "env").is_some() {
        problems.push(format!("{DIGEST}: has a job `env:`"));
    }
    for word in ["secrets.", "vars.", "github.token", "GH_TOKEN"] {
        if all.iter().any(|s| mentions(s, word)) {
            problems.push(format!(
                "{DIGEST}: a step names {word}; it reads this run's artifacts alone"
            ));
        }
    }
    if pairs(job, "outputs") != owned(&OUTPUTS) {
        problems.push(format!(
            "{DIGEST}: outputs are {:?}, not exactly {OUTPUTS:?}",
            pairs(job, "outputs")
        ));
    }
    problems
}

fn deploy_problems(job: &Node) -> Vec<String> {
    let mut problems = gate_problems(DEPLOY, job, &[DIGEST, "plan"]);
    if permissions(Some(job)) != ["id-token: write"] {
        problems.push(format!(
            "{DEPLOY}: permissions are {:?}, not id-token: write alone",
            permissions(Some(job))
        ));
    }
    let called = text(job, "uses");
    if called != CALLED {
        problems.push(format!("{DEPLOY}: calls {called:?}, not {CALLED}"));
    }
    if pairs(job, "with") != owned(&WITH) {
        problems.push(format!(
            "{DEPLOY}: passes {:?}, not exactly {WITH:?}",
            pairs(job, "with")
        ));
    }
    let mut keys: Vec<&str> = entries(job)
        .iter()
        .map(|(k, _)| k.as_str().unwrap_or_default())
        .collect();
    keys.sort_unstable();
    if keys != CALL_KEYS {
        problems.push(format!(
            "{DEPLOY}: has the keys {keys:?}, not exactly {CALL_KEYS:?}"
        ));
    }
    problems
}
