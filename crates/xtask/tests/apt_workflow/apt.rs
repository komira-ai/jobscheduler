//! The rules for the apt jobs of `release.yml` (`testing-apt-read`, `testing-apt-index` and
//! `testing-apt-write`, then the same three for `stable`, each in up to three attempts), which put
//! the release's two Debian packages in the apt repository the organisation's products share.
//! [`apt_problems`] reads the parsed workflow and names every way the jobs differ from what is
//! written here.
//!
//! The split is the point: no job holds both the signing key and the cloud role, so a compromise
//! of one step cannot sign and publish.
//! - The jobs that read and write (environment `apt-publish`) hold `id-token: write` and the
//!   role of the environment, assumed by one step; they name no signing key. No other job but
//!   `artifacts` (the call of artifacts.yml), `gamma` and `cp-deploy` (the call of
//!   docker-swarm-updater's deploy workflow) holds `id-token: write`.
//! - The index job (environment `apt-signing`) holds `contents: read` alone, assumes no role and
//!   names no variable of the role; the key is read by its gate and its signing step alone, and
//!   the signing step comes after the merge.
//! - No apt job compiles anything: none checks out the repository or runs cargo or rustup, each
//!   needs `deploy-tool` (the job that builds `xtask` with neither a token nor a secret),
//!   downloads its artifact into `tool` and makes it executable before the gate, and runs every
//!   verb as `tool/xtask channels ...`. Each step is one command.
//! - Each job has the `needs` and the `if:` of the table (each requires a push to main and the
//!   plan's release), its gate is the first step that is not setup and is given each setting as
//!   `NAME=${VAR:+set}` and each earlier job's answer as `--require`, and every step after the
//!   gate runs only when the gate said `enabled=true`.
//! - The jobs that use `dist` (every index job and the testing write job) check it first:
//!   `sha256sum --check --strict SHA256SUMS`, then the `.deb` of each architecture against its
//!   bundle (a step each) with the signer of artifacts.yml, main and the commit; all come after
//!   the two downloads and before the verb. The index job passes the key and the passphrase to
//!   `apt sign` as `--key-env` and `--passphrase-env`.
//! - The last attempt's write is the one that passes `--last` (it fails on a lost race instead of
//!   asking for a fourth attempt).
//! - The verbs of the apt channel are run by the jobs of their kind alone, and the promotion
//!   (`stable`) waits for `check-apt` and runs only when it succeeded.

use super::support::{at, entries, entry, job, mentions, names, permissions, steps, text};
use super::yaml::Node;

/// The `if:` of a job after jobs that may be skipped: not cancelled, no predecessor failed, the
/// base, and `$tail`. GitHub skips a job whose `needs` chain holds a skipped job unless the `if:`
/// has a status function, so every such job says so.
macro_rules! after_if {
    ($tail:literal) => {
        concat!(
            "${{ !cancelled() && !failure() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' && ",
            $tail,
            " }}"
        )
    };
}

/// The `if:` of the first attempt of the testing tree.
pub const RELEASE_IF: &str = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' }}";

/// The `if:` of the read job of attempt 2: the first write lost the race for the index.
pub const RETRY_2_IF: &str = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' && needs.testing-apt-write.outputs.conflict == 'true' }}";

/// The `if:` of the read job of attempt 3.
pub const RETRY_3_IF: &str = after_if!("needs.testing-apt-write-2.outputs.conflict == 'true'");

/// The `if:` of the promotion's jobs of the first attempt: `check-apt` succeeded.
pub const PROMOTE_IF: &str = after_if!("needs.check-apt.result == 'success'");

/// The `if:` of the read job of the second attempt of the promotion.
pub const STABLE_RETRY_2_IF: &str = after_if!("needs.stable-apt-write.outputs.conflict == 'true'");

/// The `if:` of the read job of the third attempt of the promotion.
pub const STABLE_RETRY_3_IF: &str =
    after_if!("needs.stable-apt-write-2.outputs.conflict == 'true'");

/// The `if:` of the index job of attempt 2 of the testing tree: its read job ran.
pub const TESTING_INDEX_2_IF: &str = after_if!("needs.testing-apt-read-2.result == 'success'");

/// The `if:` of the write job of attempt 2 of the testing tree: its read and index jobs ran.
pub const TESTING_WRITE_2_IF: &str = after_if!(
    "needs.testing-apt-read-2.result == 'success' && needs.testing-apt-index-2.result == 'success'"
);

/// The `if:` of the index job of attempt 3 of the testing tree: its read job ran.
pub const TESTING_INDEX_3_IF: &str = after_if!("needs.testing-apt-read-3.result == 'success'");

/// The `if:` of the write job of attempt 3 of the testing tree: its read and index jobs ran.
pub const TESTING_WRITE_3_IF: &str = after_if!(
    "needs.testing-apt-read-3.result == 'success' && needs.testing-apt-index-3.result == 'success'"
);

/// The `if:` of the index job of attempt 2 of the stable tree: its read job ran.
pub const STABLE_INDEX_2_IF: &str = after_if!("needs.stable-apt-read-2.result == 'success'");

/// The `if:` of the write job of attempt 2 of the stable tree: its read and index jobs ran.
pub const STABLE_WRITE_2_IF: &str = after_if!(
    "needs.stable-apt-read-2.result == 'success' && needs.stable-apt-index-2.result == 'success'"
);

/// The `if:` of the index job of attempt 3 of the stable tree: its read job ran.
pub const STABLE_INDEX_3_IF: &str = after_if!("needs.stable-apt-read-3.result == 'success'");

/// The `if:` of the write job of attempt 3 of the stable tree: its read and index jobs ran.
pub const STABLE_WRITE_3_IF: &str = after_if!(
    "needs.stable-apt-read-3.result == 'success' && needs.stable-apt-index-3.result == 'success'"
);

/// The job that builds the `xtask` every apt job runs, with neither a token nor a secret.
const TOOL_JOB: &str = "deploy-tool";

/// The `if:` of every step after the gate.
pub const GATE_IF: &str = "${{ steps.gate.outputs.enabled == 'true' }}";

/// The role the read and write jobs assume.
pub const ROLE: &str = "${{ vars.APT_PUBLISH_ROLE_ARN }}";

/// The settings of the read and write jobs, all in the environment `apt-publish`.
const PUBLISH_SETTINGS: [&str; 4] = [
    "APT_PUBLISH_ROLE_ARN",
    "APT_BUCKET",
    "APT_DISTRIBUTION_ID",
    "APT_HOSTNAME",
];

/// The settings of the index job, in the environment `apt-signing`.
const SIGNING_SETTINGS: [&str; 3] = [
    "APT_GPG_FINGERPRINT",
    "APT_GPG_PRIVATE_KEY",
    "APT_GPG_PASSPHRASE",
];

/// The kind of an apt job: it reads the index, merges and signs it, or writes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Reads the index and the ETag of each object from S3.
    Read,
    /// Merges (or promotes) the entries and signs the index.
    Index,
    /// Uploads the packages and writes the signed index.
    Write,
}

/// What one apt job is held to.
pub struct Job {
    /// The job's name in `release.yml`.
    pub name: &'static str,
    /// What the job does.
    pub kind: Kind,
    /// The jobs it needs, sorted.
    pub needs: &'static [&'static str],
    /// Its `if:`.
    pub condition: &'static str,
    /// The answers of earlier jobs the gate is given with `--require`.
    pub requires: &'static [&'static str],
    /// Does the write pass `--last`?
    pub last: bool,
    /// The suite its verbs are given with `--suite`, and its index is signed in.
    pub suite: &'static str,
}

/// The eighteen apt jobs, in the order of the file.
pub const JOBS: [Job; 18] = [
    Job {
        name: "testing-apt-read",
        kind: Kind::Read,
        needs: &["plan", "publish"],
        condition: RELEASE_IF,
        requires: &[],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-index",
        kind: Kind::Index,
        needs: &["plan", "testing-apt-read"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-write",
        kind: Kind::Write,
        needs: &["plan", "testing-apt-index", "testing-apt-read"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-read-2",
        kind: Kind::Read,
        needs: &["plan", "testing-apt-write"],
        condition: RETRY_2_IF,
        requires: &[],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-index-2",
        kind: Kind::Index,
        needs: &["plan", "testing-apt-read-2"],
        condition: TESTING_INDEX_2_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-write-2",
        kind: Kind::Write,
        needs: &["plan", "testing-apt-index-2", "testing-apt-read-2"],
        condition: TESTING_WRITE_2_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-read-3",
        kind: Kind::Read,
        needs: &["plan", "testing-apt-write-2"],
        condition: RETRY_3_IF,
        requires: &[],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-index-3",
        kind: Kind::Index,
        needs: &["plan", "testing-apt-read-3"],
        condition: TESTING_INDEX_3_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-write-3",
        kind: Kind::Write,
        needs: &["plan", "testing-apt-index-3", "testing-apt-read-3"],
        condition: TESTING_WRITE_3_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: true,
        suite: "testing",
    },
    Job {
        name: "stable-apt-read",
        kind: Kind::Read,
        needs: &["check-apt", "plan", "testing-apt-read"],
        condition: PROMOTE_IF,
        requires: &["TESTING_ENABLED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-index",
        kind: Kind::Index,
        needs: &["check-apt", "plan", "stable-apt-read", "testing-apt-index"],
        condition: PROMOTE_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-write",
        kind: Kind::Write,
        needs: &[
            "check-apt",
            "plan",
            "stable-apt-index",
            "stable-apt-read",
            "testing-apt-write",
        ],
        condition: PROMOTE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-read-2",
        kind: Kind::Read,
        needs: &["plan", "stable-apt-write"],
        condition: STABLE_RETRY_2_IF,
        requires: &[],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-index-2",
        kind: Kind::Index,
        needs: &["plan", "stable-apt-read-2"],
        condition: STABLE_INDEX_2_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-write-2",
        kind: Kind::Write,
        needs: &["plan", "stable-apt-index-2", "stable-apt-read-2"],
        condition: STABLE_WRITE_2_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-read-3",
        kind: Kind::Read,
        needs: &["plan", "stable-apt-write-2"],
        condition: STABLE_RETRY_3_IF,
        requires: &[],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-index-3",
        kind: Kind::Index,
        needs: &["plan", "stable-apt-read-3"],
        condition: STABLE_INDEX_3_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-write-3",
        kind: Kind::Write,
        needs: &["plan", "stable-apt-index-3", "stable-apt-read-3"],
        condition: STABLE_WRITE_3_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: true,
        suite: "stable",
    },
];

/// Is `name` an apt job that holds the cloud role (a read or a write)?
pub fn is_role_job(name: &str) -> bool {
    JOBS.iter().any(|j| j.name == name && j.kind != Kind::Index)
}

/// The jobs besides the apt jobs that may hold `id-token: write` in `release.yml`: the call of
/// artifacts.yml, whose jobs sign, the gamma step, and the call of docker-swarm-updater's deploy
/// workflow, which asks for a token for each request to the deploy front.
const TOKEN_HOLDERS: [&str; 3] = ["artifacts", "gamma", "cp-deploy"];

/// The verb each kind of job runs, and no other kind: the index job of the suite `testing` merges
/// the release's packages (`apt index`), the one of `stable` promotes the entries testing lists
/// (`apt promote`).
fn verb(kind: Kind, suite: &str) -> &'static str {
    match (kind, suite) {
        (Kind::Read, _) => "tool/xtask channels apt read ",
        (Kind::Index, "stable") => "tool/xtask channels apt promote ",
        (Kind::Index, _) => "tool/xtask channels apt index ",
        (Kind::Write, _) => "tool/xtask channels apt write ",
    }
}

/// The verbs (as `xtask channels ...`, whether run from the tool or from a build), with the kind and
/// the suite of the only jobs that may run each.
const VERBS: [(&str, Kind, &str); 4] = [
    ("xtask channels apt read ", Kind::Read, ""),
    ("xtask channels apt index ", Kind::Index, "testing"),
    ("xtask channels apt promote ", Kind::Index, "stable"),
    ("xtask channels apt write ", Kind::Write, ""),
];

/// The signing verb as the index jobs run it.
const SIGN: &str = "tool/xtask channels apt sign ";

/// The signing verb as any job may name it.
const SIGN_ANY: &str = "xtask channels apt sign ";

/// The step that makes the downloaded tool executable.
const CHMOD: &str = "chmod +x tool/xtask";

/// Does a step of `node` run `needle`?
fn runs(node: &Node, needle: &str) -> bool {
    steps(node).iter().any(|s| text(s, "run").contains(needle))
}

/// One line per way the apt jobs of the `release.yml` document `doc` are not as the module says.
pub fn apt_problems(doc: &Node) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let mut problems = Vec::new();
    for want in &JOBS {
        match job(jobs, want.name) {
            Some(found) => problems.extend(job_problems(want, found)),
            None => problems.push(format!("{}: there is no such job", want.name)),
        }
    }
    for (name, found) in jobs {
        let name = name.as_str().unwrap_or_default();
        let own = JOBS.iter().find(|j| j.name == name);
        if own.is_none() && name.contains("-apt-") {
            problems.push(format!(
                "{name}: an apt job this lint does not hold; the apt jobs are the eighteen it lists"
            ));
        }
        if own.map(|j| j.kind) != Some(Kind::Index) && mentions(found, "APT_GPG_") {
            problems.push(format!(
                "{name}: names APT_GPG_, the signing key, which only the apt index jobs may"
            ));
        }
        if !is_role_job(name) && mentions(found, "APT_PUBLISH_ROLE_ARN") {
            problems.push(format!(
                "{name}: names APT_PUBLISH_ROLE_ARN, which only the apt jobs that read and write may"
            ));
        }
        if !is_role_job(name) && runs(found, "xtask channels assume") {
            problems.push(format!(
                "{name}: runs `xtask channels assume...`, which only the apt jobs that read and write may"
            ));
        }
        if !is_role_job(name) && !TOKEN_HOLDERS.contains(&name) && holds_token(found) {
            problems.push(format!(
                "{name}: holds `id-token: write`; only artifacts, gamma, cp-deploy and the apt jobs that read and write may"
            ));
        }
        if own.is_none()
            && entry(found, "environment")
                .and_then(Node::as_str)
                .is_some_and(|e| e == "apt-publish" || e == "apt-signing")
        {
            problems.push(format!(
                "{name}: names the environment apt-publish or apt-signing, which only the apt jobs may"
            ));
        }
        for (word, other, suite) in VERBS {
            let may =
                own.is_some_and(|j| j.kind == other && (suite.is_empty() || j.suite == suite));
            if !may && runs(found, word) {
                let of = if suite.is_empty() {
                    String::new()
                } else {
                    format!(" of the suite {suite}")
                };
                problems.push(format!(
                    "{name}: runs `{}...`, which only the {other:?} jobs{of} of the apt repository may",
                    word.trim_end()
                ));
            }
        }
        if own.map(|j| j.kind) != Some(Kind::Index) && runs(found, SIGN_ANY) {
            problems.push(format!(
                "{name}: runs `{}...`, which only the apt index jobs may",
                SIGN_ANY.trim_end()
            ));
        }
    }
    problems
}

/// Does the job hold `id-token: write`?
fn holds_token(job: &Node) -> bool {
    permissions(Some(job))
        .iter()
        .any(|p| p == "id-token: write")
}

fn job_problems(want: &Job, job: &Node) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    let mut needs: Vec<&str> = want.needs.to_vec();
    needs.push(TOOL_JOB);
    needs.sort_unstable();
    if names(entry(job, "needs")) != needs {
        problems.push(format!("{name}: needs are not {needs:?}"));
    }
    if entry(job, "if").and_then(Node::as_str) != Some(want.condition) {
        problems.push(format!("{name}: `if` is not {}", want.condition));
    }
    let (environment, held): (&str, &[&str]) = match want.kind {
        Kind::Index => ("apt-signing", &["contents: read"]),
        _ => ("apt-publish", &["contents: read", "id-token: write"]),
    };
    if entry(job, "environment").and_then(Node::as_str) != Some(environment) {
        problems.push(format!("{name}: environment is not {environment}"));
    }
    if permissions(Some(job)) != held {
        problems.push(format!(
            "{name}: permissions are {:?}, not {held:?}",
            permissions(Some(job))
        ));
    }
    let all = steps(job);
    let Some(gate) = all
        .iter()
        .position(|s| entry(s, "id").and_then(Node::as_str) == Some("gate"))
    else {
        problems.push(format!("{name}: no step has the id `gate`"));
        return problems;
    };
    problems.extend(tool_problems(want, all, gate));
    problems.extend(suite_problems(want, all));
    problems.extend(gate_problems(want, &all[gate]));
    for step in &all[..gate] {
        for word in [
            "aws-actions/",
            "channels assume",
            "secrets.",
            "APT_GPG_",
            "apt-key",
        ] {
            if mentions(step, word) {
                problems.push(format!("{name}: a step before the gate names {word}"));
            }
        }
    }
    for step in &all[gate + 1..] {
        if entry(step, "if").and_then(Node::as_str) != Some(GATE_IF) {
            problems.push(format!(
                "{name}: the step `{}` after the gate does not have `if: {GATE_IF}`",
                text(step, "name").to_owned() + text(step, "uses")
            ));
        }
    }
    let after = &all[gate + 1..];
    if want.kind == Kind::Index || (want.kind == Kind::Write && want.suite == "testing") {
        problems.extend(dist_problems(want, after));
    }
    match want.kind {
        Kind::Index => problems.extend(index_problems(want, job, after)),
        _ => {
            problems.extend(role_problems(want, after));
            for step in all.iter().filter(|s| mentions(s, "secrets.")) {
                problems.push(format!(
                    "{name}: the step `{}` names a secret; this job holds the role, not a key",
                    text(step, "name")
                ));
            }
        }
    }
    problems
}

/// The job runs the `xtask` that `deploy-tool` built and compiles nothing: it holds a token or the
/// key, and a build would run the build scripts of the whole dependency closure in it. So no step
/// checks out the repository or runs cargo or rustup, the artifact `deploy-tool` is downloaded into
/// `tool` and made executable (one command) before the gate, which runs the tool, and each step is
/// one command.
fn tool_problems(want: &Job, all: &[Node], gate: usize) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    for step in all {
        let shown = match text(step, "name") {
            "" => text(step, "uses").to_owned() + text(step, "run"),
            named => named.to_owned(),
        };
        if text(step, "uses").starts_with("actions/checkout@") {
            problems.push(format!(
                "{name}: checks out the repository; this job runs the tool deploy-tool built and compiles nothing"
            ));
        }
        let run = text(step, "run");
        if run.contains("cargo ") || run.contains("rustup") {
            problems.push(format!(
                "{name}: the step `{shown}` runs cargo or rustup; this job runs the tool deploy-tool built and compiles nothing"
            ));
        }
        if run.trim_end().contains('\n') {
            problems.push(format!(
                "{name}: the step `{shown}` runs more than one command; each step is one"
            ));
        }
    }
    let download = all.iter().position(|s| {
        text(s, "uses").starts_with("actions/download-artifact@")
            && at(s, &["with", "name"]) == Some(TOOL_JOB)
            && at(s, &["with", "path"]) == Some("tool")
    });
    let chmod = all.iter().position(|s| text(s, "run") == CHMOD);
    match (download, chmod) {
        (Some(d), Some(c)) => {
            if !(d < c && c < gate) {
                problems.push(format!(
                    "{name}: the tool is not downloaded and made executable (`{CHMOD}`), in that order, before the gate"
                ));
            }
        }
        (None, _) => problems.push(format!(
            "{name}: the artifact {TOOL_JOB} is not downloaded into tool"
        )),
        (_, None) => problems.push(format!(
            "{name}: no step makes the tool executable with `{CHMOD}`"
        )),
    }
    problems
}

/// A job writes one suite and no other: every verb of the job is given `--suite <suite>` (the
/// promotion's read job reads both suites, and its index job is given the two directories and the
/// sums), the index is signed in the directory of that suite, a write of `testing` puts the
/// packages in the pool, and a write of `stable` puts none (`--no-pool`): the promotion lists in
/// `stable` the entries `testing` holds, and writes no package.
fn suite_problems(want: &Job, all: &[Node]) -> Vec<String> {
    let name = want.name;
    let stable = want.suite == "stable";
    let other = if stable { "testing" } else { "stable" };
    let own = verb(want.kind, want.suite);
    let mut problems = Vec::new();
    for step in all.iter().filter(|s| text(s, "run").contains(own)) {
        let run = text(step, "run");
        let shown = own.trim_end();
        match (want.kind, stable) {
            (Kind::Index, true) => {
                for argument in [
                    "--testing \"$RUNNER_TEMP/apt-read/testing\" ",
                    "--stable \"$RUNNER_TEMP/apt-read/stable\" ",
                    "--sums dist/SHA256SUMS ",
                ] {
                    if !run.contains(argument) {
                        problems.push(format!("{name}: `{shown}...` is not given {argument}"));
                    }
                }
            }
            (Kind::Read, true) => {
                // One step per suite: both are read, each by a command of its own.
                if run.matches("--suite ").count() != 1 {
                    problems.push(format!(
                        "{name}: `{shown}...` is not given exactly one of `--suite testing` and `--suite stable`"
                    ));
                }
            }
            _ => {
                if !run.contains(&format!("{own}--suite {} ", want.suite)) {
                    problems.push(format!(
                        "{name}: `{shown}...` is not given `--suite {}`",
                        want.suite
                    ));
                }
                if run.contains(&format!("--suite {other} ")) {
                    problems.push(format!(
                        "{name}: names `--suite {other}`; this job is for the suite {}",
                        want.suite
                    ));
                }
            }
        }
        if want.kind == Kind::Write {
            if stable && !run.contains(" --no-pool ") {
                problems.push(format!(
                    "{name}: does not pass --no-pool; the promotion writes the index only"
                ));
            }
            if stable && run.contains("--debs ") {
                problems.push(format!(
                    "{name}: passes --debs; the promotion uploads no package"
                ));
            }
            if !stable && run.contains("--no-pool") {
                problems.push(format!(
                    "{name}: passes --no-pool; the release's write puts the packages in the pool"
                ));
            }
            if !stable && !run.contains("--debs dist --version \"$VERSION\" ") {
                problems.push(format!(
                    "{name}: does not upload the .debs of dist for the release's version"
                ));
            }
        }
        if want.kind == Kind::Index && !stable && !run.contains("--version \"$VERSION\" ") {
            problems.push(format!(
                "{name}: does not index the release's version ($VERSION, the plan's)"
            ));
        }
    }
    if want.kind == Kind::Read && stable {
        for suite in ["testing", "stable"] {
            let wanted = format!("{own}--suite {suite} ");
            if !all.iter().any(|s| text(s, "run").contains(&wanted)) {
                problems.push(format!(
                    "{name}: no step runs `{}...` with `--suite {suite}`: the promotion reads both",
                    own.trim_end()
                ));
            }
        }
    }
    let dir = format!("--dir \"$RUNNER_TEMP/apt-next/dists/{}\" ", want.suite);
    for step in all.iter().filter(|s| text(s, "run").contains(SIGN_ANY)) {
        if !text(step, "run").contains(&dir) {
            problems.push(format!(
                "{name}: the index is not signed in dists/{}, the suite the job writes",
                want.suite
            ));
        }
    }
    problems
}

/// The gate of the job `want`: its command, the settings and the answers of earlier jobs.
pub fn gate_problems(want: &Job, gate: &Node) -> Vec<String> {
    let name = want.name;
    let run = text(gate, "run");
    let mut problems = Vec::new();
    let start = format!("tool/xtask channels gate --label {name} ");
    if !run.starts_with(&start) {
        problems.push(format!("{name}: the gate does not run `{start}...`"));
    }
    let settings: &[&str] = match want.kind {
        Kind::Index => &SIGNING_SETTINGS,
        _ => &PUBLISH_SETTINGS,
    };
    for setting in settings {
        if !run.contains(&format!("--setting \"{setting}=${{{setting}:+set}}\"")) {
            problems.push(format!(
                "{name}: the gate is not given the setting {setting} as NAME=${{NAME:+set}}"
            ));
        }
    }
    // The gate reads whether a value is set, never the value.
    if run.matches("--setting ").count() != settings.len() {
        problems.push(format!(
            "{name}: the gate is given a setting it should not be"
        ));
    }
    for require in want.requires {
        if !run.contains(&format!("--require \"{require}=${require}\"")) {
            problems.push(format!(
                "{name}: the gate is not given --require \"{require}=${require}\""
            ));
        }
    }
    if run.matches("--require ").count() != want.requires.len() {
        problems.push(format!(
            "{name}: the gate is given a requirement it should not be"
        ));
    }
    if run.contains("secrets.") || run.contains("vars.") {
        problems.push(format!("{name}: the gate's command line holds a value"));
    }
    problems
}

/// The index of the first of `after` that satisfies `wanted`.
fn first(after: &[Node], wanted: impl Fn(&Node) -> bool) -> Option<usize> {
    after.iter().position(wanted)
}

/// The command that verifies the `.deb` of `arch` from its bundle, up to the flags: a command of its
/// own for each architecture.
fn verify_start(arch: &str) -> String {
    format!(
        "gh attestation verify \"dist/js-agent_${{VERSION}}_{arch}.deb\" --bundle \"dist/js-agent_${{VERSION}}_{arch}.deb.sigstore.json\" "
    )
}

/// The flags every `gh attestation verify` of a `.deb` is given: the signer, ref and commit
/// artifacts.yml signed it at, from a hosted runner.
const VERIFY_FLAGS: [&str; 5] = [
    "--repo \"$REPO\" ",
    "--signer-workflow \"$REPO/.github/workflows/artifacts.yml\" ",
    "--source-ref refs/heads/main ",
    "--source-digest \"$COMMIT\" ",
    "--deny-self-hosted-runners",
];

/// The jobs that use `dist` hold it to the checks before the verb: the assets and the bundles are
/// downloaded into `dist`, then `sha256sum --check --strict SHA256SUMS` runs there, then the `.deb`
/// of each architecture is verified from its bundle (a step each), and only then the verb runs.
fn dist_problems(want: &Job, after: &[Node]) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    for artifact in ["release-assets", "bundles"] {
        let downloaded = first(after, |s| {
            text(s, "uses").starts_with("actions/download-artifact@")
                && at(s, &["with", "name"]) == Some(artifact)
                && at(s, &["with", "path"]) == Some("dist")
        });
        if downloaded.is_none() {
            problems.push(format!(
                "{name}: the artifact {artifact} is not downloaded into dist"
            ));
        }
    }
    let sums = first(after, |s| {
        text(s, "run") == "sha256sum --check --strict SHA256SUMS"
            && at(s, &["working-directory"]) == Some("dist")
    });
    let verifies: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(_, s)| text(s, "run").contains("gh attestation verify "))
        .map(|(i, _)| i)
        .collect();
    let use_of = first(after, |s| {
        let run = text(s, "run");
        run.contains(verb(want.kind, want.suite)) || run.contains(SIGN)
    });
    for arch in ["amd64", "arm64"] {
        let start = verify_start(arch);
        let Some(at_step) = verifies
            .iter()
            .copied()
            .find(|&i| text(&after[i], "run").starts_with(&start))
        else {
            problems.push(format!(
                "{name}: no step verifies the {arch} .deb from its bundle with `{}...`",
                start.trim_end()
            ));
            continue;
        };
        let run = text(&after[at_step], "run");
        for flag in VERIFY_FLAGS {
            if !run.contains(flag) {
                problems.push(format!(
                    "{name}: the verification of the {arch} .deb lacks {flag}"
                ));
            }
        }
        for (var, want) in [
            ("GH_TOKEN", "${{ github.token }}"),
            ("REPO", "${{ github.repository }}"),
        ] {
            if at(&after[at_step], &["env", var]) != Some(want) {
                problems.push(format!(
                    "{name}: the verification of the {arch} .deb does not read {var} from {want}"
                ));
            }
        }
    }
    match (sums, verifies.first(), verifies.last()) {
        (Some(sums), Some(&first_verify), Some(&last_verify)) => {
            if sums > first_verify {
                problems.push(format!(
                    "{name}: the .debs are verified before the files are checked against SHA256SUMS"
                ));
            }
            if use_of.is_some_and(|u| u < last_verify) {
                problems.push(format!(
                    "{name}: the verb runs before the .debs are verified from their bundles"
                ));
            }
        }
        (None, _, _) => problems.push(format!(
            "{name}: no step checks dist with `sha256sum --check --strict SHA256SUMS`"
        )),
        (_, _, _) => problems.push(format!(
            "{name}: no step verifies each .deb from its bundle with gh attestation verify"
        )),
    }
    problems
}

/// The jobs that read and write: one step assumes the role, before the verb, and no step names
/// a secret.
fn role_problems(want: &Job, after: &[Node]) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    let roles: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(_, s)| text(s, "run").contains("tool/xtask channels assume"))
        .map(|(i, _)| i)
        .collect();
    let verbs: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(_, s)| text(s, "run").contains(verb(want.kind, want.suite)))
        .map(|(i, _)| i)
        .collect();
    // The promotion's read job reads two suites, one command (one step) each.
    let verb_steps = if want.kind == Kind::Read && want.suite == "stable" {
        2
    } else {
        1
    };
    match (roles.as_slice(), verbs.as_slice()) {
        ([role], [at_verb, ..]) if verbs.len() == verb_steps => {
            let given = entry(&after[*role], "env")
                .and_then(|e| entry(e, "ROLE_ARN"))
                .and_then(Node::as_str);
            if given != Some(ROLE) {
                problems.push(format!(
                    "{name}: the role assumed is not {ROLE}, the environment's"
                ));
            }
            let assume = text(&after[*role], "run");
            for argument in [
                "--role-arn \"$ROLE_ARN\"",
                "--session-name \"jobscheduler-",
                "--region ",
                "--env-file \"$GITHUB_ENV\"",
            ] {
                if !assume.contains(argument) {
                    problems.push(format!("{name}: the assume step is not given {argument}"));
                }
            }
            if role > at_verb {
                problems.push(format!("{name}: the role is assumed after the verb runs"));
            }
            let last = text(&after[verbs[verbs.len() - 1]], "run").contains(" --last ");
            if want.kind == Kind::Write && last != want.last {
                problems.push(format!(
                    "{name}: `--last` must be passed by the last attempt's write and by no other"
                ));
            }
        }
        _ => problems.push(format!(
            "{name}: it needs one step that assumes the role and one that runs `{}...`",
            verb(want.kind, want.suite).trim_end()
        )),
    }
    problems
}

/// The index jobs: no role, the key in the gate and the signing step alone, the merge before the
/// signature.
fn index_problems(want: &Job, job: &Node, after: &[Node]) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    for word in ["aws-actions/", "id-token", "AWS_", "APT_PUBLISH_"] {
        if mentions(job, word) {
            problems.push(format!(
                "{name}: names {word}; this job holds the key, not the role"
            ));
        }
    }
    let merge = after
        .iter()
        .position(|s| text(s, "run").contains(verb(Kind::Index, want.suite)));
    let sign = after.iter().position(|s| text(s, "run").contains(SIGN));
    match (merge, sign) {
        (Some(m), Some(s)) if m < s => {
            if text(&after[s], "id") != "sign" {
                problems.push(format!(
                    "{name}: the signing step does not have the id `sign`"
                ));
            }
            for secret in ["secrets.APT_GPG_PRIVATE_KEY", "secrets.APT_GPG_PASSPHRASE"] {
                if !mentions(&after[s], secret) {
                    problems.push(format!("{name}: the signing step does not read {secret}"));
                }
            }
            // The secret material goes to the verb in the environment, as one command: the verb
            // writes and removes the files itself.
            let sign = text(&after[s], "run");
            for flag in [
                "--key-env APT_GPG_PRIVATE_KEY ",
                "--passphrase-env APT_GPG_PASSPHRASE ",
            ] {
                if !sign.contains(flag) {
                    problems.push(format!("{name}: the signing step is not given {flag}"));
                }
            }
            for flag in ["--key-file", "--passphrase-file"] {
                if sign.contains(flag) {
                    problems.push(format!(
                        "{name}: the signing step passes {flag}; the secrets go by --key-env and --passphrase-env"
                    ));
                }
            }
            for (i, step) in after.iter().enumerate() {
                if i != s && mentions(step, "secrets.") {
                    problems.push(format!(
                        "{name}: the step `{}` names a secret; only the gate and the signing step may",
                        text(step, "name")
                    ));
                }
            }
        }
        _ => problems.push(format!(
            "{name}: it needs a step that merges (`{}...`) and then one that signs (`{}...`)",
            verb(Kind::Index, want.suite).trim_end(),
            SIGN.trim_end()
        )),
    }
    problems
}
