//! Whether a directory is a job's put directory: `<put root>/cas/b/<build>/<nonce>`.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::commit::PutTarget;

/// The first two components of every put directory below the put root.
const PREFIX: [&str; 2] = ["cas", "b"];

/// Why a directory is not a job's put directory: the directory (or the part of its path
/// that is wrong) and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {why}", path.display())]
pub struct Refusal {
    /// The path refused.
    pub path: PathBuf,
    /// Why.
    pub why: String,
}

impl Refusal {
    fn of(path: &Path, why: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            why: why.into(),
        }
    }
}

/// The put target `dir` names below `put_root`, which is canonical (the runtime
/// canonicalizes it once when it starts). `dir` must lie strictly beneath the root with no
/// `..`, and be exactly `cas/b/<build>/<nonce>` below it: four components, each valid UTF-8
/// and none starting with a dot (so neither the root itself nor `cas/b/<build>`). What of it
/// exists must be directories, none of them a link, so that what the commit makes stays
/// below the root; what does not exist yet is made by the commit. Its key prefix is that
/// relative path, and its directory the root joined with it.
///
/// # Errors
///
/// [`Refusal`], naming `dir`, or the part of it that is a link, is not a directory or
/// cannot be looked at.
pub fn target_of(put_root: &Path, dir: &Path) -> Result<PutTarget, Refusal> {
    if dir.components().any(|part| part == Component::ParentDir) {
        return Err(Refusal::of(dir, "names `..`"));
    }
    let below = dir
        .strip_prefix(put_root)
        .map_err(|_| Refusal::of(dir, "not beneath the put root"))?;
    let parts = below
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<&str>>>()
        .ok_or_else(|| Refusal::of(dir, "not valid UTF-8"))?;
    // A component of a path is never empty, so a dot is the one thing to look for.
    let shaped =
        parts.len() == 4 && parts[..2] == PREFIX && parts.iter().all(|part| !part.starts_with('.'));
    if !shaped {
        return Err(Refusal::of(
            dir,
            "not of the shape cas/b/<build>/<nonce> below the put root",
        ));
    }
    let mut at = put_root.to_owned();
    for part in &parts {
        at.push(part);
        let meta = match fs::symlink_metadata(&at) {
            Err(why) if why.kind() == io::ErrorKind::NotFound => break,
            found => found.map_err(|why| Refusal::of(&at, why.to_string()))?,
        };
        if !meta.is_dir() {
            return Err(Refusal::of(&at, "a link or not a directory"));
        }
    }
    let key_prefix = parts.join("/");
    Ok(PutTarget {
        dir: put_root.join(&key_prefix),
        key_prefix,
    })
}
