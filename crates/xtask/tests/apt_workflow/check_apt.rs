//! The rules for the job `check-apt` of `release.yml`, which sits between the publication to the
//! suite `testing` and the promotion into `stable`. [`check_apt_problems`] reads the parsed
//! workflow.
//!
//! - The job holds `contents: read` alone, names no environment, no secret, no variable and no
//!   token: an install check installs what is public and needs nothing private.
//! - It waits for every attempt of the apt write of testing, and runs only when one of them wrote
//!   (`conflict=false` is what a write that landed appends), after the attempts that were needed.
//! - It runs in a clean Ubuntu container whose image is pinned by its digest, on a hosted runner of
//!   each architecture (amd64 and arm64), with the `xtask` the runner built mounted read-only, and
//!   runs `channels check-apt` with the architecture and the version of the release.
//! - A container image anywhere in `release.yml` is pinned by digest, and `check-apt` alone runs
//!   the verb or docker.

use super::support::{at, entries, entry, job, mentions, names, permissions, steps, text};
use super::yaml::Node;

/// The `if:` of `check-apt`: a write of testing wrote, after the attempts that were needed.
pub const APT_IF: &str = "${{ !cancelled() && !failure() && github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.plan.outputs.release == 'true' && (needs.testing-apt-write.outputs.conflict == 'false' || needs.testing-apt-write-2.outputs.conflict == 'false' || needs.testing-apt-write-3.outputs.conflict == 'false') }}";

/// The jobs `check-apt` waits for, sorted.
const NEEDS: [&str; 4] = [
    "plan",
    "testing-apt-write",
    "testing-apt-write-2",
    "testing-apt-write-3",
];

/// Is `word` a digest-pinned image reference: `@sha256:` and 64 hex digits?
fn pinned(word: &str) -> bool {
    word.split_once("@sha256:")
        .is_some_and(|(_, d)| d.len() >= 64 && d[..64].bytes().all(|b| b.is_ascii_hexdigit()))
}

/// One line per way the install check of the `release.yml` document `doc` is not as the module says.
pub fn check_apt_problems(doc: &Node) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let mut problems = Vec::new();
    match job(jobs, "check-apt") {
        Some(found) => problems.extend(job_problems(found)),
        None => problems.push("check-apt: there is no such job".to_owned()),
    }
    for (name, found) in jobs {
        let name = name.as_str().unwrap_or_default();
        let runs = |word: &str| steps(found).iter().any(|s| text(s, "run").contains(word));
        if name != "check-apt" && (runs("channels check-apt") || runs("docker ")) {
            problems.push(format!(
                "{name}: runs `channels check-apt` or docker; only check-apt may"
            ));
        }
        if let Some(container) = entry(found, "container") {
            let image = container.as_str().or_else(|| at(container, &["image"]));
            if !image.is_some_and(pinned) {
                problems.push(format!(
                    "{name}: its container image is not pinned by digest (image@sha256:<64 hex digits>)"
                ));
            }
        }
    }
    problems
}

fn job_problems(job: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if names(entry(job, "needs")) != NEEDS {
        problems.push(format!("check-apt: needs are not {NEEDS:?}"));
    }
    if entry(job, "if").and_then(Node::as_str) != Some(APT_IF) {
        problems.push(format!("check-apt: `if` is not {APT_IF}"));
    }
    if entry(job, "environment").is_some() {
        problems
            .push("check-apt: names an environment; it needs no setting and no secret".to_owned());
    }
    if permissions(Some(job)) != ["contents: read"] {
        problems.push(format!(
            "check-apt: permissions are {:?}, not contents: read alone",
            permissions(Some(job))
        ));
    }
    for word in ["secrets.", "vars.", "id-token", "github.token", "GH_TOKEN"] {
        if mentions(job, word) {
            problems.push(format!("check-apt: names {word}; this job holds no secret"));
        }
    }
    problems.extend(matrix_problems(job));
    problems.extend(run_problems(job));
    problems
}

/// The matrix: a hosted runner of each architecture, and the job runs on the leg's runner.
fn matrix_problems(job: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if at(job, &["runs-on"]) != Some("${{ matrix.runner }}") {
        problems.push("check-apt: runs-on is not ${{ matrix.runner }}".to_owned());
    }
    let strategy = entry(job, "strategy");
    for runner in ["ubuntu-24.04", "ubuntu-24.04-arm"] {
        if !strategy.is_some_and(|s| entry(s, "matrix").is_some_and(|m| mentions(m, runner))) {
            problems.push(format!(
                "check-apt: the matrix does not run on {runner}: both architectures are checked"
            ));
        }
    }
    for arch in ["amd64", "arm64"] {
        if !strategy.is_some_and(|s| mentions(s, arch)) {
            problems.push(format!("check-apt: the matrix has no {arch}"));
        }
    }
    problems
}

/// The steps: the toolchain and the build of xtask come first, then one `docker run` of the
/// pinned image with the binary mounted read-only and the check given what it needs.
fn run_problems(job: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    let all = steps(job);
    let build = all
        .iter()
        .position(|s| text(s, "run") == "cargo xtask --help");
    let containers: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, s)| text(s, "run").contains("docker run"))
        .map(|(i, _)| i)
        .collect();
    let [at_run] = containers.as_slice() else {
        problems.push("check-apt: it needs one step that runs `docker run`".to_owned());
        return problems;
    };
    if build.is_none_or(|b| b > *at_run) {
        problems.push(
            "check-apt: xtask is not compiled (`cargo xtask --help`) before the container runs"
                .to_owned(),
        );
    }
    let run = text(&all[*at_run], "run");
    if !run
        .split_whitespace()
        .any(|w| w.starts_with("ubuntu@") && pinned(w))
    {
        problems.push(
            "check-apt: the container image is not ubuntu@sha256:<digest>, pinned by digest"
                .to_owned(),
        );
    }
    if !run.contains("target/debug/xtask:/usr/local/bin/xtask:ro") {
        problems.push("check-apt: the xtask binary is not mounted read-only (:ro)".to_owned());
    }
    for argument in [
        "xtask channels check-apt --arch \"$ARCH\" ",
        "--version \"$VERSION\"",
    ] {
        if !run.contains(argument) {
            problems.push(format!("check-apt: the check is not given {argument}"));
        }
    }
    if run.contains("--commit") {
        problems.push(
            "check-apt: the check is given --commit; js-agent's version has no commit suffix"
                .to_owned(),
        );
    }
    if run.contains("--privileged") || run.contains("--network host") {
        problems.push("check-apt: the container is given more than a clean Ubuntu has".to_owned());
    }
    if at(&all[*at_run], &["env", "ARCH"]) != Some("${{ matrix.arch }}") {
        problems.push("check-apt: ARCH is not the leg's ${{ matrix.arch }}".to_owned());
    }
    if at(job, &["env", "VERSION"]) != Some("${{ needs.plan.outputs.version }}") {
        problems.push(
            "check-apt: VERSION is not the plan's ${{ needs.plan.outputs.version }}".to_owned(),
        );
    }
    problems
}
