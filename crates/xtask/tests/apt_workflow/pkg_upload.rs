//! The job `pkg-upload` of `release.yml`, which puts the signed macOS package of js-agent in the
//! package repository (`pkg/jobscheduler/<version>/`) so that hostkeeper-desired.yml can order it
//! onto the Macs. [`pkg_upload_problems`] reads the parsed workflow and names every way the job
//! differs from what is written here.
//!
//! The job is an apt job in everything but the verb: it holds the role of the environment
//! `apt-publish` (assumed by one step, through OIDC) and `id-token: write`, no signing key and no
//! secret, and runs the `xtask` that `deploy-tool` built, so it compiles nothing.
//! - It needs `plan`, `deploy-tool` and `promote-macos` (so the three beta jobs and `publish`
//!   passed), runs only for a push to main on the plan's release, and holds `contents: read` and
//!   `id-token: write`.
//! - Its first steps download the tool and make it executable; the gate follows, with the two
//!   settings the upload needs (the role and the bucket), and every step after it runs only when
//!   the gate said `enabled=true`.
//! - The signed package is downloaded by a step that may fail (a release whose package was not
//!   signed has none): the steps that need it run only when that download succeeded, and one step
//!   says in a notice that nothing is uploaded when it did not.
//! - The role is assumed before the upload, which is the one command that writes
//!   (`channels pkg upload`, which no other job runs).

use super::apt::{GATE_IF, RELEASE_IF, ROLE};
use super::support::{at, entries, entry, job, mentions, names, permissions, steps};
use super::yaml::Node;

/// The job's name.
pub const PKG_JOB: &str = "pkg-upload";

/// The jobs it waits for: the plan, the tool, and the promotion of the macOS archives, which
/// itself waits for the three beta jobs and `publish`.
pub const NEEDS: [&str; 3] = ["deploy-tool", "plan", "promote-macos"];

/// The `if:` of the steps that need the signed package.
pub const SIGNED_IF: &str =
    "${{ steps.gate.outputs.enabled == 'true' && steps.signed.outcome == 'success' }}";

/// The `if:` of the notice that nothing is uploaded.
pub const UNSIGNED_IF: &str =
    "${{ steps.gate.outputs.enabled == 'true' && steps.signed.outcome != 'success' }}";

/// The command of the gate.
pub const GATE: &str = "tool/xtask channels gate --label pkg-upload --output \"$GITHUB_OUTPUT\" --setting \"APT_PUBLISH_ROLE_ARN=${APT_PUBLISH_ROLE_ARN:+set}\" --setting \"APT_BUCKET=${APT_BUCKET:+set}\"";

/// The command that assumes the role.
pub const ASSUME: &str = "tool/xtask channels assume --role-arn \"$ROLE_ARN\" --session-name \"jobscheduler-pkg-upload-$GITHUB_RUN_ID\" --region us-east-1 --env-file \"$GITHUB_ENV\"";

/// The command that uploads: the package, then its SHA256SUMS, to `pkg/jobscheduler/<version>/`.
pub const UPLOAD: &str = "tool/xtask channels pkg upload --dir pkg --version \"$VERSION\" --bucket \"$APT_BUCKET\" --work \"$RUNNER_TEMP/pkg-work\"";

/// The verb that only [`PKG_JOB`] may run.
pub const VERB: &str = "xtask channels pkg upload";

/// The artifact the signing job of js-agent-macos-pkg.yml uploads.
const SIGNED_ARTIFACT: &str = "js-agent-macos-pkg-signed";

/// One line per way the job `pkg-upload` of the `release.yml` document `doc` is not as the module
/// says.
pub fn pkg_upload_problems(doc: &Node) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let Some(found) = job(jobs, PKG_JOB) else {
        return vec![format!("{PKG_JOB}: there is no such job")];
    };
    let mut problems = Vec::new();
    if names(entry(found, "needs")) != NEEDS {
        problems.push(format!("{PKG_JOB}: needs are not {NEEDS:?}"));
    }
    if at(found, &["if"]) != Some(RELEASE_IF) {
        problems.push(format!("{PKG_JOB}: `if` is not {RELEASE_IF}"));
    }
    if at(found, &["environment"]) != Some("apt-publish") {
        problems.push(format!("{PKG_JOB}: environment is not apt-publish"));
    }
    if permissions(Some(found)) != ["contents: read", "id-token: write"] {
        problems.push(format!(
            "{PKG_JOB}: permissions are not contents: read and id-token: write"
        ));
    }
    if at(found, &["env", "VERSION"]) != Some("${{ needs.plan.outputs.version }}") {
        problems.push(format!(
            "{PKG_JOB}: VERSION is not the plan's version, ${{{{ needs.plan.outputs.version }}}}"
        ));
    }
    if mentions(found, "secrets.") || mentions(found, "APT_GPG_") {
        problems.push(format!("{PKG_JOB}: names a secret or the signing key"));
    }
    problems.extend(step_problems(steps(found)));
    problems
}

/// The position of the step with `id`.
fn by_id(all: &[Node], id: &str) -> Option<usize> {
    all.iter().position(|s| at(s, &["id"]) == Some(id))
}

/// The position of the step that runs `command`.
fn runs(all: &[Node], command: &str) -> Option<usize> {
    all.iter()
        .position(|s| at(s, &["run"]).is_some_and(|r| r.trim_end() == command))
}

fn step_problems(all: &[Node]) -> Vec<String> {
    let mut problems = Vec::new();
    for step in all {
        let uses = at(step, &["uses"]).unwrap_or_default();
        let run = at(step, &["run"]).unwrap_or_default();
        if uses.starts_with("actions/checkout@") || run.contains("cargo ") || run.contains("rustup")
        {
            problems.push(format!(
                "{PKG_JOB}: checks out or compiles; it runs the tool deploy-tool built"
            ));
        }
        if run.trim_end().contains('\n') {
            problems.push(format!("{PKG_JOB}: a step runs more than one command"));
        }
    }
    let download = all.iter().position(|s| {
        at(s, &["uses"]).is_some_and(|u| u.starts_with("actions/download-artifact@"))
            && at(s, &["with", "name"]) == Some("deploy-tool")
            && at(s, &["with", "path"]) == Some("tool")
    });
    let chmod = runs(all, "chmod +x tool/xtask");
    let gate = by_id(all, "gate");
    match (download, chmod, gate) {
        (Some(d), Some(c), Some(g)) if d < c && c < g => {}
        _ => problems.push(format!(
            "{PKG_JOB}: the tool is not downloaded and made executable before the gate"
        )),
    }
    let Some(gate) = gate else {
        problems.push(format!("{PKG_JOB}: there is no gate"));
        return problems;
    };
    if runs(all, GATE) != Some(gate) {
        problems.push(format!("{PKG_JOB}: the gate does not run `{GATE}`"));
    }
    for (var, want) in [
        ("APT_PUBLISH_ROLE_ARN", ROLE),
        ("APT_BUCKET", "${{ vars.APT_BUCKET }}"),
    ] {
        if at(&all[gate], &["env", var]) != Some(want) {
            problems.push(format!(
                "{PKG_JOB}: the gate does not read {var} from {want}"
            ));
        }
    }
    let signed = by_id(all, "signed");
    match signed.map(|i| &all[i]) {
        Some(step) => {
            let right = at(step, &["uses"])
                .is_some_and(|u| u.starts_with("actions/download-artifact@"))
                && at(step, &["with", "name"]) == Some(SIGNED_ARTIFACT)
                && at(step, &["with", "path"]) == Some("pkg")
                && at(step, &["if"]) == Some(GATE_IF);
            if !right {
                problems.push(format!(
                    "{PKG_JOB}: the step `signed` is not the download of {SIGNED_ARTIFACT} into pkg, when the gate is enabled"
                ));
            }
            if at(step, &["continue-on-error"]) != Some("true") {
                problems.push(format!(
                    "{PKG_JOB}: the download of the signed package may not fail the job (a release whose package was not signed has none)"
                ));
            }
        }
        None => problems.push(format!("{PKG_JOB}: the signed package is not downloaded")),
    }
    let assume = runs(all, ASSUME);
    let upload = runs(all, UPLOAD);
    match (signed, assume, upload) {
        (Some(s), Some(a), Some(u)) if gate < s && s < a && a < u => {}
        _ => problems.push(format!(
            "{PKG_JOB}: the steps are not the gate, the download, the role and the upload, in that order (`{UPLOAD}`)"
        )),
    }
    if let Some(a) = assume
        && at(&all[a], &["env", "ROLE_ARN"]) != Some(ROLE)
    {
        problems.push(format!("{PKG_JOB}: the role assumed is not {ROLE}"));
    }
    if let Some(u) = upload
        && at(&all[u], &["env", "APT_BUCKET"]) != Some("${{ vars.APT_BUCKET }}")
    {
        problems.push(format!("{PKG_JOB}: the bucket is not vars.APT_BUCKET"));
    }
    for (index, step) in all.iter().enumerate().skip(gate + 1) {
        let condition = at(step, &["if"]).unwrap_or_default();
        let wanted = if Some(index) == signed {
            continue;
        } else if runs_notice(step) {
            UNSIGNED_IF
        } else {
            SIGNED_IF
        };
        if condition != wanted {
            problems.push(format!(
                "{PKG_JOB}: the step at {} runs `if: {condition}`, not `{wanted}`",
                index + 1
            ));
        }
    }
    problems
}

/// Is `step` the notice that says nothing is uploaded?
fn runs_notice(step: &Node) -> bool {
    at(step, &["run"]).is_some_and(|r| r.starts_with("echo \"::notice::"))
}
