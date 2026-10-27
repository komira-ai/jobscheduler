//! `release plan --head <sha> --tip <sha>`: whether the commit `--head`, which passed `ci`
//! on main, is released, and as which version. The checkout in `--repo-dir` must be at
//! `--head` and hold every tag (`fetch-depth: 0`).
//!
//! In order, the first rule that applies decides:
//!
//! 1. `--tip` (the commit main is at when the run starts) is not `--head`: not released.
//!    The attestation of a build records the commit the workflow run is at, which for a run
//!    started by another workflow is the tip of main, not the commit that passed. A build
//!    of an older commit would be attested as another one, so a newer push releases (its
//!    change is measured from the latest tag, so nothing is lost).
//! 2. No `v*` release tag exists: released, as the workspace version.
//! 3. The latest tag names `--head`: a re-run of a release that was started. Released,
//!    as that tag's version; the tag is kept as it is.
//! 4. Some path changed between the latest tag and `--head` ships ([`super::is_shipped`]):
//!    released, as [`super::next`]. Otherwise not released.
//!
//! `--dry-run` (a pull request that changes the workflow) takes the same decision and then
//! says released, so the build is exercised; the workflow never tags or publishes then.
//!
//! Prints `release`, `version`, `tag`, `tag_exists` and `reason` as `key=value` lines, and
//! appends them to `--output` (on Actions, `GITHUB_OUTPUT`). The version and tag are those
//! the release would have, whether or not it is released.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Error, Metadata, Version, first_shipped, latest, next, read, workspace_version};

/// The arguments of `release plan`.
#[derive(clap::Args)]
pub struct PlanArgs {
    /// The commit that passed `ci`, a full SHA (on Actions, the workflow run's `head_sha`).
    #[arg(long)]
    pub head: String,
    /// The commit main is at now (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub tip: String,
    /// A checkout at `--head` holding the whole history and every tag.
    #[arg(long, default_value = ".")]
    pub repo_dir: PathBuf,
    /// A crate whose binaries are shipped; its workspace dependencies ship with it.
    #[arg(
        long = "ship",
        default_values_t = ["js-lease-server".to_owned(), "js-cli".to_owned(), "js-agent".to_owned()]
    )]
    pub ship: Vec<String>,
    /// Decide as usual, then say released (see the module docs).
    #[arg(long)]
    pub dry_run: bool,
    /// Append the lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The `git` program.
    #[arg(long, default_value = "git")]
    pub git: PathBuf,
    /// The `cargo` program.
    #[arg(long, default_value = "cargo")]
    pub cargo: PathBuf,
}

/// What the plan decided.
struct Verdict {
    /// Whether a release is made.
    release: bool,
    /// The tag that already names `--head`, if one does.
    existing: Option<Version>,
    /// One line saying why.
    reason: String,
}

impl Verdict {
    /// Released, for `reason`.
    fn release(reason: String) -> Verdict {
        Verdict {
            release: true,
            existing: None,
            reason,
        }
    }

    /// Not released, for `reason`.
    fn skip(reason: String) -> Verdict {
        Verdict {
            release: false,
            existing: None,
            reason,
        }
    }
}

/// Runs `program args` in `dir`: its stdout, when it succeeded and printed UTF-8.
fn capture(program: &Path, dir: &Path, args: &[&str]) -> Result<String, Error> {
    let command = format!("{} {}", program.display(), args.join(" "));
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| Error::Spawn {
            program: program.display().to_string(),
            why: e.to_string(),
        })?;
    if !out.status.success() {
        return Err(Error::Failed {
            command,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
        });
    }
    String::from_utf8(out.stdout).map_err(|_| Error::NotUtf8 { command })
}

/// The decision, before `--dry-run`.
fn decide(a: &PlanArgs, tagged: Option<Version>) -> Result<Verdict, Error> {
    if a.tip != a.head {
        return Ok(Verdict::skip(format!(
            "main is at {} now, past {}: the push that moved it releases this change",
            a.tip, a.head
        )));
    }
    let Some(tagged) = tagged else {
        return Ok(Verdict::release(
            "no release tag yet: the first release".to_owned(),
        ));
    };
    let tag = tagged.tag();
    let at = capture(&a.git, &a.repo_dir, &["rev-list", "-n", "1", &tag])?;
    if at.trim() == a.head {
        return Ok(Verdict {
            release: true,
            existing: Some(tagged),
            reason: format!("{tag} already names this commit: a re-run of its release"),
        });
    }
    let diff = capture(
        &a.git,
        &a.repo_dir,
        &["diff", "--name-only", "-z", "--no-renames", &tag, &a.head],
    )?;
    let changed: Vec<String> = diff
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    let json = capture(
        &a.cargo,
        &a.repo_dir,
        &["metadata", "--no-deps", "--format-version", "1"],
    )?;
    let dirs = Metadata::parse(&json)?.shipped_dirs(&a.ship)?;
    Ok(match first_shipped(&changed, &dirs) {
        Some(path) => Verdict::release(format!("{path} changed since {tag}")),
        None => Verdict::skip(format!("nothing that ships changed since {tag}")),
    })
}

/// The `release plan` verb: the lines to print.
pub fn run(a: &PlanArgs) -> Result<String, Error> {
    let at = capture(&a.git, &a.repo_dir, &["rev-parse", "HEAD"])?;
    if at.trim() != a.head {
        return Err(Error::WrongCheckout {
            expected: a.head.clone(),
            actual: at.trim().to_owned(),
        });
    }
    let tags = capture(&a.git, &a.repo_dir, &["tag", "--list", "v*"])?;
    let tagged = latest(tags.lines());
    let cargo = workspace_version(&read(&a.repo_dir.join("Cargo.toml"))?)?;
    let mut verdict = decide(a, tagged)?;
    if a.dry_run {
        verdict = Verdict {
            release: true,
            existing: None,
            reason: format!("dry run (decided: {})", verdict.reason),
        };
    }
    let version = verdict.existing.unwrap_or_else(|| next(tagged, cargo));
    let lines = [
        format!("release={}", verdict.release),
        format!("version={version}"),
        format!("tag={}", version.tag()),
        format!("tag_exists={}", verdict.existing.is_some()),
        // The reason can hold a file name, and a name can hold a line end, which in
        // `GITHUB_OUTPUT` would start another output.
        format!("reason={}", verdict.reason.replace(['\n', '\r'], "?")),
    ]
    .join("\n");
    if let Some(output) = &a.output {
        append(output, &lines)?;
    }
    Ok(lines)
}

/// Appends `lines` and a line end to `path`, creating it.
fn append(path: &Path, lines: &str) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{lines}"))
        .map_err(|e| super::io_error(path, &e))
}
