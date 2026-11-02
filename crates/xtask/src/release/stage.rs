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

/// Runs `release stage`.
///
/// # Errors
/// [`Error::Io`] naming the path: the archive's directory exists already, or a directory cannot
/// be made, or a file cannot be copied.
pub fn run(a: &StageArgs) -> Result<String, Error> {
    let name = format!("{}-{}-{}", a.bin, a.version, a.target);
    std::fs::create_dir_all(&a.out).map_err(|e| io_error(&a.out, &e))?;
    let top = a.out.join(&name);
    let list = members(&a.bin, &a.target);
    // The archive's directory first, which must be new, then the directories inside it.
    let mut dirs = vec![top.clone()];
    dirs.extend(
        list.iter()
            .filter_map(|member| Path::new(&member.path).parent())
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| top.join(parent)),
    );
    dirs.dedup();
    for dir in &dirs {
        std::fs::create_dir(dir).map_err(|e| io_error(dir, &e))?;
    }
    for member in &list {
        let from = a.repo_dir.join(&member.source);
        std::fs::copy(&from, top.join(&member.path)).map_err(|e| io_error(&from, &e))?;
    }
    let paths: Vec<&str> = list.iter().map(|member| member.path.as_str()).collect();
    Ok(format!("staged {}: {}", top.display(), paths.join(", ")))
}
