//! `release stage --bin <name> --target <triple> --version X.Y.Z --out <dir> [--repo-dir <dir>]`:
//! lays down the directory a release archive is made of, `<out>/<bin>-<version>-<target>/`, with
//! exactly the files [`members`] lists, copied from the checkout. The workflow then archives it
//! (`.github/workflows/release.yml`, docs/release.md).
//!
//! Every archive holds its binary and the repository's `LICENSE`. The Linux archive of `js-agent`
//! also holds the host updater's declaration as `updater.d/js-agent.toml` ([`DECLARATION_MEMBER`]),
//! the name kbf's release unpacker takes it by, so the updater reads the rights of a release
//! from the release itself. The Mac archive does not: a Mac runs no generic updater, and its
//! package carries its own declaration.

use std::path::{Path, PathBuf};

use super::{Error, Version, io_error};

/// The host updater's declaration of js-agent in the repository: the file the deb carries too.
pub const DECLARATION_SOURCE: &str = "packaging/js-agent/updater.d/js-agent.toml";

/// The declaration's path inside the archive's directory.
pub const DECLARATION_MEMBER: &str = "updater.d/js-agent.toml";

/// The arguments of `release stage`.
#[derive(clap::Args)]
pub struct StageArgs {
    /// The binary the archive is of (`js-agent`).
    #[arg(long)]
    pub bin: String,
    /// The target it was built for (`x86_64-unknown-linux-musl`).
    #[arg(long)]
    pub target: String,
    /// The release version, `MAJOR.MINOR.PATCH`.
    #[arg(long)]
    pub version: Version,
    /// The directory to make the archive's directory in; made if missing.
    #[arg(long)]
    pub out: PathBuf,
    /// The checkout the files are copied from.
    #[arg(long, default_value = ".")]
    pub repo_dir: PathBuf,
}

/// One file of an archive: where it comes from in the checkout, and its path in the archive's
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The file in the checkout, relative to its root.
    pub source: String,
    /// The path inside `<bin>-<version>-<target>/`.
    pub path: String,
}

impl Member {
    fn new(source: impl Into<String>, path: impl Into<String>) -> Member {
        Member {
            source: source.into(),
            path: path.into(),
        }
    }
}

/// The files of the archive of `bin` for `target`, in the order they are copied.
#[must_use]
pub fn members(bin: &str, target: &str) -> Vec<Member> {
    let mut list = vec![
        Member::new(format!("target/{target}/release/{bin}"), bin),
        Member::new("LICENSE", "LICENSE"),
    ];
    if bin == "js-agent" && target.contains("-linux-") {
        list.push(Member::new(DECLARATION_SOURCE, DECLARATION_MEMBER));
    }
    list
}

/// Whether `word` can be a part of an archive's name, which is a directory name: ASCII
/// letters, digits, `.`, `_` and `-`, not empty, and not starting with `.` or `-` (so not `.`,
/// `..`, a hidden name or something a program reads as a flag). No `/`: a name never reaches
/// outside `--out`.
fn is_name(word: &str) -> bool {
    let first_ok = word
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric());
    first_ok
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Puts the file `from` at `to`: what `release stage` does with each member.
pub type Put<'a> = &'a dyn Fn(&Path, &Path) -> std::io::Result<()>;

/// The copy [`run`] puts each member with.
fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to).map(|_| ())
}

/// Runs `release stage`.
///
/// Nothing is written unless `--bin` and `--target` are names ([`is_name`]). The archive's
/// directory must be new; one that exists is left as it is. When a later file cannot be put
/// in, the directory this run made is removed again, so a retry after the cause is fixed
/// starts clean; when it cannot be removed either, the error says so and names it.
///
/// # Errors
/// [`Error::BadName`] for `--bin` or `--target`; [`Error::Io`] naming the path: the archive's
/// directory exists already, a directory cannot be made, or a file cannot be copied;
/// [`Error::Incomplete`] when the directory of a run that failed could not be removed.
pub fn run(a: &StageArgs) -> Result<String, Error> {
    run_with(a, &copy)
}

/// [`run`], with `put` putting each file in place: the seam the tests fail a write through.
///
/// # Errors
/// As [`run`].
pub fn run_with(a: &StageArgs, put: Put<'_>) -> Result<String, Error> {
    for (flag, word) in [("--bin", &a.bin), ("--target", &a.target)] {
        if !is_name(word) {
            return Err(Error::BadName {
                flag,
                value: word.clone(),
            });
        }
    }
    let name = format!("{}-{}-{}", a.bin, a.version, a.target);
    std::fs::create_dir_all(&a.out).map_err(|e| io_error(&a.out, &e))?;
    let top = a.out.join(&name);
    std::fs::create_dir(&top).map_err(|e| io_error(&top, &e))?;
    let list = members(&a.bin, &a.target);
    if let Err(failed) = fill(&top, &a.repo_dir, &list, put) {
        // What this run put there is incomplete; the directory was new, so it is all ours.
        return Err(match std::fs::remove_dir_all(&top) {
            Ok(()) => failed,
            Err(e) => Error::Incomplete {
                path: top.display().to_string(),
                why: failed.to_string(),
                cleanup: e.to_string(),
            },
        });
    }
    let paths: Vec<&str> = list.iter().map(|member| member.path.as_str()).collect();
    Ok(format!("staged {}: {}", top.display(), paths.join(", ")))
}

/// Puts each member into `top`, making the directory it lies in. A failure names the source,
/// and the destination in its reason.
fn fill(top: &Path, repo: &Path, list: &[Member], put: Put<'_>) -> Result<(), Error> {
    for member in list {
        let (from, to) = (repo.join(&member.source), top.join(&member.path));
        let dir = to.parent().unwrap_or(top);
        std::fs::create_dir_all(dir)
            .and_then(|()| put(&from, &to))
            .map_err(|e| Error::Io {
                path: from.display().to_string(),
                why: format!("to {}: {e}", to.display()),
            })?;
    }
    Ok(())
}
