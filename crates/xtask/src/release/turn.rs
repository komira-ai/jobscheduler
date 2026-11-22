//! `release turn --repo <owner/name> --workflow <file> --run-number <N> --version <X.Y.Z>
//! [--poll-seconds <S>] [--output <file>] [--gh <program>]`: the job `turn` of
//! `.github/workflows/release.yml`, which sits before `publish`. Every push to main starts a
//! run, and the runs build and test in parallel with no concurrency group, so none is ever
//! cancelled. This verb serializes the publish and deploy of the runs and skips the ones a
//! later run has overtaken. `gh` reads its token from the environment; this verb reads none.
//!
//! Each round reads, through `gh api`:
//!
//! - the runs of the workflow on `main` that are in progress from a push, other than this
//!   one (`--run-number`);
//! - for each of them, its jobs: the state of the job [`TURN_JOB`], of the job [`PUBLISH_JOB`],
//!   and of the job [`DEPLOYED_JOB`], which is the last of the run's publish and deploy jobs and
//!   runs `if: always()`. A job that is absent counts as not started;
//! - the newest published release (the highest `vX.Y.Z`, not a draft, not a pre-release), if
//!   there is one.
//!
//! Each list is one page of 100 entries (`PER_PAGE`). A full page that GitHub says holds fewer
//! entries than it counts is an error, not half read. A page that is not full and still lists
//! fewer entries than GitHub counts is GitHub's listing behind its own count (it has answered a
//! `total_count` of 1 over an empty list of runs while this run was the one in progress): the
//! round decides nothing, prints one line saying so, sleeps `--poll-seconds` and reads again.
//! On the tenth such round in a row (`BEHIND_READS`) it is an error.
//!
//! Then [`decide`], the first rule that applies:
//!
//! 1. A published release is newer than `--version`: this run is superseded.
//! 2. A newer run (a higher run number) has its `turn` job in progress or succeeded: this run
//!    is superseded, the newer run publishes instead.
//! 3. An older run has its `turn` job in progress, or succeeded while its `deployed` job has
//!    not ended and its `publish` job has not ended other than in success (a run whose `turn`
//!    said `go=false` has its `publish` skipped, and one whose `publish` failed deploys nothing,
//!    so neither holds the turn while its `deployed` waits for the rest of its jobs, such as the
//!    macOS package's): this run waits.
//! 4. Otherwise this run goes.
//!
//! Older runs never wait for newer ones, so the waiting cannot deadlock. A newer run waits for
//! every older run that holds the turn until that run's `deployed` job has ended, whatever its
//! conclusion. An older run that reaches its turn while a newer one already has, or after a
//! newer release was published, skips publish and deploy: when several runs are queued only the
//! newest deploys. A run whose `turn` job is queued (no runner yet) or not reached does not
//! hold the turn, and neither does a `turn` job that failed or was cancelled.
//!
//! On a wait the verb prints one line saying why, sleeps `--poll-seconds` and reads again. On a
//! go or a supersession it prints the reason and `go=true` or `go=false`, and appends `go=...`
//! and `reason=...` to `--output` (on Actions, `GITHUB_OUTPUT`). Both exit 0. Any `gh` failure,
//! answer that is not the JSON asked for, full page short of the count, or list behind its
//! count on ten rounds in a row is an error naming the call.

use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde_json::Value;

use super::desired::{capture, strings};
use super::{Version, io_error, latest};

#[cfg(test)]
mod tests;

/// The job of `release.yml` that takes the turn to publish and deploy.
pub const TURN_JOB: &str = "turn";

/// The job of `release.yml` that publishes the release; skipped when `turn` said `go=false`.
pub const PUBLISH_JOB: &str = "publish";

/// The last of the publish and deploy jobs of `release.yml`; it runs whatever the others did.
pub const DEPLOYED_JOB: &str = "deployed";

/// The entries of one page of every list `release turn` reads (`per_page`).
const PER_PAGE: usize = 100;

/// The rounds in a row whose lists GitHub counts longer than it lists, the last of which is an
/// error rather than another read.
const BEHIND_READS: u32 = 10;

/// Why `release turn` did not decide.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Flags that do not fit (exit status 2, like a clap error).
    #[error("{0}")]
    Usage(String),
    /// An answer from GitHub is not what was asked for.
    #[error("{0}")]
    Failed(String),
    /// A program did not run, or failed, or the output file could not be written.
    #[error(transparent)]
    Release(#[from] super::Error),
}

/// The arguments of `release turn`.
#[derive(clap::Args)]
pub struct TurnArgs {
    /// `owner/name` of the repository.
    #[arg(long)]
    pub repo: String,
    /// The file name of the release workflow, `release.yml`.
    #[arg(long)]
    pub workflow: String,
    /// The number of this run of the workflow.
    #[arg(long)]
    pub run_number: u64,
    /// The version this run would release, `X.Y.Z`.
    #[arg(long)]
    pub version: String,
    /// Seconds between two reads while waiting.
    #[arg(long, default_value_t = 20)]
    pub poll_seconds: u64,
    /// The file the outputs are appended to (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The GitHub CLI.
    #[arg(long, default_value = "gh")]
    pub gh: String,
}

/// How far a job of another run has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// There is no such job in the run.
    Absent,
    /// The job exists but has no runner: queued, waiting or pending.
    Queued,
    /// The job is running.
    Running,
    /// The job ended with `success`.
    Succeeded,
    /// The job ended with anything else: failed, cancelled, skipped.
    Ended,
}

/// Another run of the workflow, as far as the turn is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The run number.
    pub number: u64,
    /// The state of its `turn` job.
    pub turn: Job,
    /// The state of its `publish` job.
    pub publish: Job,
    /// The state of its `deployed` job.
    pub deployed: Job,
}

/// What this run does.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Publish and deploy now.
    Go(String),
    /// Skip publish and deploy: a later release or run takes over.
    Superseded(String),
    /// Hold on and read again.
    Wait(String),
}

/// The decision of the run `mine` that would release `version`, given the newest published
/// release and the other runs: the rules in the module docs, the first that applies.
pub fn decide(mine: u64, version: Version, newest: Option<Version>, peers: &[Peer]) -> Decision {
    if let Some(newest) = newest.filter(|n| *n > version) {
        return Decision::Superseded(format!(
            "release {} is published, newer than {}",
            newest.tag(),
            version.tag()
        ));
    }
    if let Some(newer) = peers
        .iter()
        .find(|p| p.number > mine && matches!(p.turn, Job::Running | Job::Succeeded))
    {
        return Decision::Superseded(format!(
            "run #{}, newer, reached its turn: it publishes instead",
            newer.number
        ));
    }
    let holding: Vec<String> = peers
        .iter()
        .filter(|p| p.number < mine)
        .filter_map(|p| match (p.turn, p.publish, p.deployed) {
            (Job::Running, _, _) => Some(format!("run #{} has its turn", p.number)),
            (Job::Succeeded, Job::Ended, _) | (Job::Succeeded, _, Job::Succeeded | Job::Ended) => {
                None
            }
            (Job::Succeeded, _, _) => Some(format!(
                "run #{} has published and not yet deployed",
                p.number
            )),
            _ => None,
        })
        .collect();
    if holding.is_empty() {
        Decision::Go("no newer release, no newer run at its turn, no older run holding it".into())
    } else {
        Decision::Wait(holding.join("; "))
    }
}

/// What `gh api <path>` answered, as JSON.
fn api(gh: &str, path: &str) -> Result<Value, Error> {
    let text = capture(gh, &strings(&["api", path]))?;
    serde_json::from_str(&text)
        .map_err(|e| Error::Failed(format!("gh api {path} did not print JSON: {e}")))
}

/// What one page of a list holds.
#[derive(Debug)]
enum Listed<'a> {
    /// Every entry GitHub counts.
    All(&'a [Value]),
    /// A page that is not full and lists fewer entries than GitHub counts; the line says so.
    Behind(String),
}

/// The entries of `found[key]` when the page holds all of them, [`Listed::Behind`] when a page
/// that is not full lists fewer than `total_count`.
fn page<'a>(found: &'a Value, key: &str, what: &str) -> Result<Listed<'a>, Error> {
    let Some(items) = found[key].as_array() else {
        return Err(Error::Failed(format!("{what}: `{key}` is not a list")));
    };
    let Some(total) = found["total_count"].as_u64() else {
        return Err(Error::Failed(format!(
            "{what}: `total_count` is not a number"
        )));
    };
    if total <= items.len() as u64 {
        return Ok(Listed::All(items));
    }
    if items.len() >= PER_PAGE {
        return Err(Error::Failed(format!(
            "{what}: {total} entries, one page of {} was read",
            items.len()
        )));
    }
    Ok(Listed::Behind(format!(
        "{what}: GitHub counts {total} entries and lists {}",
        items.len()
    )))
}

/// What one round read of the other runs.
#[derive(Debug, PartialEq, Eq)]
enum Round {
    /// Every other run in progress, with the state of its jobs.
    Peers(Vec<Peer>),
    /// A list GitHub counts longer than it lists; the line says which.
    Behind(String),
}

/// A field of a JSON object that must be a number.
fn number(value: &Value, key: &str, what: &str) -> Result<u64, Error> {
    value[key]
        .as_u64()
        .ok_or_else(|| Error::Failed(format!("{what}: `{key}` is not a number")))
}

/// A field of a JSON object that must be a string.
fn text<'a>(value: &'a Value, key: &str, what: &str) -> Result<&'a str, Error> {
    value[key]
        .as_str()
        .ok_or_else(|| Error::Failed(format!("{what}: `{key}` is not a string")))
}

/// The state of the job `name` among `jobs`: [`Job::Absent`] when there is none.
fn job_state(jobs: &[Value], name: &str, what: &str) -> Result<Job, Error> {
    for job in jobs {
        if text(job, "name", what)? != name {
            continue;
        }
        return Ok(match text(job, "status", what)? {
            "in_progress" => Job::Running,
            "completed" if text(job, "conclusion", what)? == "success" => Job::Succeeded,
            "completed" => Job::Ended,
            _ => Job::Queued,
        });
    }
    Ok(Job::Absent)
}

/// The runs of the workflow in progress on main from a push, other than `mine`, with the state
/// of their `turn`, `publish` and `deployed` jobs; or the first list that is behind its count.
fn peers(args: &TurnArgs) -> Result<Round, Error> {
    let repo = &args.repo;
    let path = format!(
        "repos/{repo}/actions/workflows/{}/runs?branch=main&event=push&status=in_progress&per_page={PER_PAGE}",
        args.workflow
    );
    let found = api(&args.gh, &path)?;
    let what = format!("runs of {} of {repo}", args.workflow);
    let runs = match page(&found, "workflow_runs", &what)? {
        Listed::All(runs) => runs,
        Listed::Behind(why) => return Ok(Round::Behind(why)),
    };
    let mut peers = Vec::new();
    for run in runs {
        let id = number(run, "id", &what)?;
        let run_number = number(run, "run_number", &what)?;
        if run_number == args.run_number {
            continue;
        }
        let what = format!("jobs of run {id} (#{run_number}) of {repo}");
        let path = format!("repos/{repo}/actions/runs/{id}/jobs?per_page={PER_PAGE}&filter=latest");
        let found = api(&args.gh, &path)?;
        let jobs = match page(&found, "jobs", &what)? {
            Listed::All(jobs) => jobs,
            Listed::Behind(why) => return Ok(Round::Behind(why)),
        };
        peers.push(Peer {
            number: run_number,
            turn: job_state(jobs, TURN_JOB, &what)?,
            publish: job_state(jobs, PUBLISH_JOB, &what)?,
            deployed: job_state(jobs, DEPLOYED_JOB, &what)?,
        });
    }
    Ok(Round::Peers(peers))
}

/// The highest version among the published releases of `repo`, if there is one.
fn newest(gh: &str, repo: &str) -> Result<Option<Version>, Error> {
    let found = api(gh, &format!("repos/{repo}/releases?per_page=100"))?;
    let Some(releases) = found.as_array() else {
        return Err(Error::Failed(format!(
            "releases of {repo}: the answer is not a list"
        )));
    };
    Ok(latest(
        releases
            .iter()
            .filter(|r| r["draft"] == false && r["prerelease"] == false)
            .filter_map(|r| r["tag_name"].as_str()),
    ))
}

/// Appends `lines` to `path`, making the file if needed.
fn append_output(path: &std::path::Path, lines: &[String]) -> Result<(), Error> {
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(body.as_bytes()))
        .map_err(|e| io_error(path, &e).into())
}

/// Runs `release turn`, printing each waiting line to stdout as it happens: the lines to
/// print when it decided, joined by newlines.
///
/// # Errors
/// See [`run_to`].
pub fn run(args: &TurnArgs) -> Result<String, Error> {
    run_to(args, &mut std::io::stdout())
}

/// Writes `line` to `out` as a waiting line and sleeps `--poll-seconds`.
fn wait(args: &TurnArgs, out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "waiting: {line}");
    let _ = out.flush();
    std::thread::sleep(Duration::from_secs(args.poll_seconds));
}

/// Runs `release turn`, writing a line to `out` for each round that waits: the reason and
/// `go=true` or `go=false`, joined by newlines, once it decided.
///
/// # Errors
/// [`Error::Usage`] for a `--version` that is not `<major>.<minor>.<patch>`;
/// [`Error::Failed`] for an answer of `gh` that is not JSON or lacks a field, a full page
/// short of its count, or a list behind its count on ten rounds in a row (`BEHIND_READS`);
/// [`Error::Release`] for a `gh` that does not run or fails, or an output file that cannot be
/// written.
pub fn run_to(args: &TurnArgs, out: &mut dyn Write) -> Result<String, Error> {
    let version = Version::from_str(&args.version).map_err(|_| {
        Error::Usage(format!(
            "--version {:?} is not <major>.<minor>.<patch>, a release tag without its v",
            args.version
        ))
    })?;
    let mut behind = 0;
    loop {
        let newest = newest(&args.gh, &args.repo)?;
        let peers = match peers(args)? {
            Round::Peers(peers) => {
                behind = 0;
                peers
            }
            Round::Behind(why) => {
                behind += 1;
                if behind >= BEHIND_READS {
                    return Err(Error::Failed(format!("{why}, on {behind} reads in a row")));
                }
                wait(args, out, &format!("{why}; reading again"));
                continue;
            }
        };
        let (go, reason) = match decide(args.run_number, version, newest, &peers) {
            Decision::Wait(why) => {
                wait(args, out, &why);
                continue;
            }
            Decision::Go(why) => (true, why),
            Decision::Superseded(why) => (false, why),
        };
        let lines = [format!("go={go}"), format!("reason={reason}")];
        if let Some(output) = &args.output {
            append_output(output, &lines)?;
        }
        return Ok(format!("{reason}\n{}", lines[0]));
    }
}
