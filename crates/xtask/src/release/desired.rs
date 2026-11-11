//! `release desired --repo <owner/repo> --version <X.Y.Z or ""> --after-run <run id or ""> --dir
//! <dir> [--output <file>] [--apt-url <url>] [--gh <program>] [--curl <program>]`: what
//! `.github/workflows/hostkeeper-desired.yml` sets as the desired version of the hostkeeper
//! service `js-agent`, or why it sets nothing. It runs before any step of that workflow can hold
//! a token, so a refusal here means the coordinator is never called.
//!
//! Which release:
//!
//! - `--version X.Y.Z` (a run by hand): the release `vX.Y.Z`, whichever it is, an older one
//!   included (a rollback).
//! - `--after-run <id>` (the completion of a run of `release.yml`): when the job `publish` of
//!   that run succeeded, the newest published release of `<owner/repo>` (the highest `vX.Y.Z`,
//!   not a draft, not a pre-release). Release runs are one at a time, so that is the release the
//!   run published, or a newer one whose run then comes next. When `publish` did not succeed
//!   (most runs plan no release, and their `publish` is skipped) nothing is set: the output is
//!   `set=false` and the verb succeeds.
//!
//! Exactly one of the two is given; an empty one counts as not given, so the workflow passes
//! both from its two events and one is always empty.
//!
//! The release must be a published GitHub release (not a draft, not a pre-release). Its
//! `SHA256SUMS` is downloaded into `<dir>`, and then:
//!
//! - `SHA256SUMS` lists `js-agent_<X.Y.Z>_amd64.deb` and `js-agent_<X.Y.Z>_arm64.deb`;
//! - the `Packages` index of the suite `stable` for each architecture, read through the CDN as
//!   a host reads it (`--apt-url`), has a stanza whose `Filename` is
//!   `pool/jobscheduler/<that .deb>` with the SHA-256 `SHA256SUMS` gives. The stable suite is
//!   what hosts install from: the release promoted the package there after its install checks.
//!
//! Every problem found is reported, one line each. When there is none, `--output` (on Actions,
//! `GITHUB_OUTPUT`) gets `set=true`, `package-version=<X.Y.Z>` and `targets=<one-line JSON>`:
//! the proto3 JSON of hostkeeper's `TargetList` per platform ([`manifest`]; no `macos-arm64`,
//! which has no package).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use serde_json::Value;

use super::deb::{Platform, file_name};
use super::{Version, io_error, latest};

pub mod manifest;
pub mod repo;

#[cfg(test)]
pub(crate) mod tests;

/// The job of `release.yml` that publishes a release.
pub const PUBLISH_JOB: &str = "publish";

/// The release asset that lists the digest of every other.
pub const SUMS: &str = "SHA256SUMS";

/// The directory of the repository's pool that holds the package, as a `Packages` stanza's
/// `Filename` names it: the publisher's own.
pub use crate::channels::apt::state::POOL_DIR;

/// Why `release desired` set nothing.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Flags that do not fit together (exit status 2, like a clap error).
    #[error("{0}")]
    Usage(String),
    /// The release is refused, for each of these reasons.
    #[error("{}", .0.join("\n"))]
    Refused(Vec<String>),
    /// An answer from GitHub or the repository is not what was asked for.
    #[error("{0}")]
    Failed(String),
    /// A line of `SHA256SUMS` is not `<sha256>  <name>`.
    #[error("{SUMS} line {line} is not `<sha256>  <name>`: {text:?}")]
    Sums {
        /// The line number, from 1.
        line: usize,
        /// The line.
        text: String,
    },
    /// A program did not run, or failed, or a file could not be read or written.
    #[error(transparent)]
    Release(#[from] super::Error),
}

/// The arguments of `release desired`.
#[derive(clap::Args)]
pub struct DesiredArgs {
    /// `owner/name` of the repository whose release is set.
    #[arg(long)]
    pub repo: String,
    /// A released version, `X.Y.Z`; empty when `--after-run` is given.
    #[arg(long)]
    pub version: String,
    /// The id of a run of `release.yml`; empty when `--version` is given.
    #[arg(long)]
    pub after_run: String,
    /// Where `SHA256SUMS` and the `Packages` files are downloaded.
    #[arg(long)]
    pub dir: PathBuf,
    /// The file the outputs are appended to (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The apt repository's URL, as a host reads it.
    #[arg(long, default_value = "https://packages.komira.ai/apt")]
    pub apt_url: String,
    /// The GitHub CLI.
    #[arg(long, default_value = "gh")]
    pub gh: String,
    /// The program that downloads a file.
    #[arg(long, default_value = "curl")]
    pub curl: String,
}

/// Runs `program args`: its stdout, when it succeeded and printed UTF-8.
pub(crate) fn capture(program: &str, args: &[String]) -> Result<String, super::Error> {
    let command = format!("{program} {}", args.join(" "));
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| super::Error::Spawn {
            program: program.to_owned(),
            why: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(super::Error::Failed {
            command,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
        });
    }
    String::from_utf8(out.stdout).map_err(|_| super::Error::NotUtf8 { command })
}

/// The arguments as owned strings.
pub(crate) fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// What `gh api <path>` answered, as JSON.
pub(crate) fn api(gh: &str, path: &str) -> Result<Value, Error> {
    let text = capture(gh, &strings(&["api", path]))?;
    serde_json::from_str(&text)
        .map_err(|e| Error::Failed(format!("gh api {path} did not print JSON: {e}")))
}

/// The elements of the array `value`, none for anything else.
pub(crate) fn array(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

/// Whether the job [`PUBLISH_JOB`] of the run `run` of `repo` succeeded.
pub(crate) fn published(gh: &str, repo: &str, run: &str) -> Result<bool, Error> {
    let path = format!("repos/{repo}/actions/runs/{run}/jobs?per_page=100&filter=latest");
    let found = api(gh, &path)?;
    let jobs = array(&found["jobs"]);
    // One page holds every job of release.yml; a run with more is refused, not half read.
    if found["total_count"].as_u64() != Some(jobs.len() as u64) {
        return Err(Error::Failed(format!(
            "run {run} of {repo} has {} jobs; one page of {} was read",
            found["total_count"],
            jobs.len()
        )));
    }
    Ok(jobs
        .iter()
        .any(|j| j["name"] == PUBLISH_JOB && j["conclusion"] == "success"))
}

/// The highest version among the published releases of `repo` (their tags `vX.Y.Z`).
pub(crate) fn newest(gh: &str, repo: &str) -> Result<Version, Error> {
    let found = api(gh, &format!("repos/{repo}/releases?per_page=100"))?;
    latest(
        array(&found)
            .iter()
            .filter(|r| r["draft"] == false && r["prerelease"] == false)
            .filter_map(|r| r["tag_name"].as_str()),
    )
    .ok_or_else(|| Error::Failed(format!("{repo} has no published release vX.Y.Z")))
}

/// The version the request names: `--version`, or the newest release when the run of
/// `--after-run` published one; `None` when it published none.
pub(crate) fn chosen(
    gh: &str,
    repo: &str,
    version: &str,
    after_run: &str,
) -> Result<Option<Version>, Error> {
    match (version, after_run) {
        (version, "") if !version.is_empty() => Version::from_str(version)
            .map(Some)
            .map_err(|_| {
                Error::Usage(format!(
                    "--version {version:?} is not <major>.<minor>.<patch>, a release tag without its v"
                ))
            }),
        ("", run) if !run.is_empty() => {
            if !run.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Usage(format!(
                    "--after-run {run:?} is not the id of a workflow run"
                )));
            }
            if published(gh, repo, run)? {
                newest(gh, repo).map(Some)
            } else {
                Ok(None)
            }
        }
        _ => Err(Error::Usage(
            "give one of --version and --after-run, not both and not neither".to_owned(),
        )),
    }
}

/// The names of the assets of the published release `tag` of `repo`.
pub(crate) fn assets(gh: &str, repo: &str, tag: &str) -> Result<Vec<String>, Error> {
    let found = api(gh, &format!("repos/{repo}/releases/tags/{tag}"))?;
    if found["draft"] == true || found["prerelease"] == true {
        return Err(Error::Failed(format!(
            "release {tag} of {repo} is a draft or a pre-release, not a released version"
        )));
    }
    Ok(array(&found["assets"])
        .iter()
        .filter_map(|a| a["name"].as_str().map(str::to_owned))
        .collect())
}

/// Appends `lines` to the file `path`, making it if needed.
pub(crate) fn append_output(path: &Path, lines: &[String]) -> Result<(), Error> {
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|e| io_error(path, &e).into())
}

/// The arguments of the `curl` that reads `url` into `file`: HTTPS only, TLS 1.2 or later, a
/// failure on an HTTP error status and five retries, as a host's tooling reads a repository.
fn curl_args(file: &Path, url: &str) -> Vec<String> {
    let mut args = strings(&[
        "--proto",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--silent",
        "--show-error",
        "--retry",
        "5",
        "--retry-delay",
        "20",
        "--retry-all-errors",
        "--max-time",
        "60",
        "--output",
    ]);
    args.push(file.display().to_string());
    args.push(url.to_owned());
    args
}

/// The `.deb` digests of the platforms, as `SHA256SUMS` gives them.
type Digests = Vec<(Platform, String)>;

/// The digests of the `.deb`s of `version` the stable suite holds, per platform, and the
/// problems found.
fn check_suite(
    args: &DesiredArgs,
    sums: &str,
    version: Version,
    tag: &str,
) -> Result<(Digests, Vec<String>), Error> {
    let mut digests = Vec::new();
    let mut problems = Vec::new();
    for platform in Platform::ALL {
        let arch = platform.architecture();
        let name = file_name(version, platform);
        let Some(digest) = repo::sums_digest(sums, &name)? else {
            problems.push(format!("{SUMS} of release {tag} lists no {name}"));
            continue;
        };
        let file = args.dir.join(format!("Packages-{arch}"));
        let url = format!("{}/dists/stable/main/binary-{arch}/Packages", args.apt_url);
        match capture(&args.curl, &curl_args(&file, &url)) {
            Ok(_) => {}
            Err(super::Error::Failed { stderr, .. }) => {
                problems.push(format!("{url} could not be read: {stderr}"));
                continue;
            }
            Err(e) => return Err(e.into()),
        }
        let text = std::fs::read_to_string(&file).map_err(|e| io_error(&file, &e))?;
        match repo::listed(&text, &format!("{POOL_DIR}/{name}")) {
            repo::Listed::No => problems.push(format!(
                "the stable suite lists no {name}: the release is not promoted there"
            )),
            repo::Listed::Yes(None) => problems.push(format!(
                "the stable suite lists {name} with no SHA256 field"
            )),
            repo::Listed::Yes(Some(listed)) if listed != digest => problems.push(format!(
                "the stable suite lists {name} with sha256 {listed}, but {SUMS} lists {digest}"
            )),
            repo::Listed::Yes(Some(_)) => digests.push((platform, digest)),
        }
    }
    Ok((digests, problems))
}

/// Runs `release desired`: the lines to print, joined by newlines.
///
/// # Errors
/// [`Error::Usage`] for neither or both of `--version` and `--after-run`, a `--version` that is
/// not `<major>.<minor>.<patch>`, an `--after-run` that is not digits or an `--apt-url` that is
/// not an https URL without a trailing `/`; [`Error::Failed`] for a run GitHub lists in more
/// than one page, a repository with no published release or a release that is a draft or a
/// pre-release; [`Error::Refused`], with a line per problem, for a release without `SHA256SUMS`,
/// one whose sums do not list a `.deb`, or a stable suite that does not list a `.deb` with
/// the digest of the sums; [`Error::Release`] for a program that does not run or fails.
pub fn run(args: &DesiredArgs) -> Result<String, Error> {
    if !args.apt_url.starts_with("https://") || args.apt_url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--apt-url {:?} is not an https URL without a trailing /",
            args.apt_url
        )));
    }
    let Some(version) = chosen(&args.gh, &args.repo, &args.version, &args.after_run)? else {
        let lines = [
            format!(
                "run {} of {} did not publish a release ({PUBLISH_JOB} did not succeed): nothing is set",
                args.after_run, args.repo
            ),
            "set=false".to_owned(),
        ];
        if let Some(output) = &args.output {
            append_output(output, &lines[1..])?;
        }
        return Ok(lines.join("\n"));
    };
    let tag = version.tag();
    if !assets(&args.gh, &args.repo, &tag)?
        .iter()
        .any(|n| n == SUMS)
    {
        return Err(Error::Refused(vec![format!(
            "release {tag} has no {SUMS}: there are no digests to hold the stable suite to"
        )]));
    }
    std::fs::create_dir_all(&args.dir).map_err(|e| io_error(&args.dir, &e))?;
    let mut download = strings(&["release", "download", &tag, "--repo", &args.repo, "--dir"]);
    download.extend([
        args.dir.display().to_string(),
        "--pattern".to_owned(),
        SUMS.to_owned(),
    ]);
    capture(&args.gh, &download)?;
    let sums_path = args.dir.join(SUMS);
    let sums = std::fs::read_to_string(&sums_path).map_err(|e| io_error(&sums_path, &e))?;
    let (digests, problems) = check_suite(args, &sums, version, &tag)?;
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let lines = [
        "set=true".to_owned(),
        format!("package-version={version}"),
        format!(
            "targets={}",
            manifest::targets(&version.to_string(), &digests)
        ),
    ];
    if let Some(output) = &args.output {
        append_output(output, &lines)?;
    }
    let mut printed = vec![format!(
        "release {tag}: the stable suite lists js-agent {version} for each of {} architectures, each with the digest {SUMS} lists",
        digests.len()
    )];
    printed.extend(lines);
    Ok(printed.join("\n"))
}
