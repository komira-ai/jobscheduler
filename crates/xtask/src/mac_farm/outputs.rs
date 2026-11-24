//! The outputs of a farm action: the files and directories its Command declares
//! (`output_paths`, from `--output`), fetched from the farm's CAS once the command passed and
//! written below a local directory at the same paths.
//!
//! The farm's answer is not trusted further than the CAS is: a path the Command did not declare,
//! a declared path the result lacks, a symlink (which could point a later write elsewhere), a
//! name in a directory's tree that is not a plain name or is given twice, a tree of more than
//! [`MAX_ENTRIES`] entries, and a blob whose bytes are not its digest ([`Farm::read`]) each fail
//! the run before anything is written.

#[cfg(test)]
#[path = "outputs_tests.rs"]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use prost::Message as _;

use super::client::{Farm, FarmError};
use super::proto::reapi::{ActionResult, Digest, Directory, Tree};
use super::tree::digest_of;

/// The most files and directories one output directory may hold: a tree whose directories name
/// one child many times could otherwise stand for more files than any build makes.
pub const MAX_ENTRIES: usize = 100_000;

/// Why the outputs could not be fetched or written.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    /// A declared output the command did not make.
    #[error("the command passed but made no {0}")]
    Missing(String),
    /// An output in the result that the Command did not declare.
    #[error("the farm answered with the output {0}, which the command did not declare")]
    Undeclared(String),
    /// An output, or an entry of an output directory, that is a symlink.
    #[error("the output {0} is a symlink; only files and directories are fetched")]
    Symlink(String),
    /// An output file or directory with no digest.
    #[error("the farm answered with the output {0} and no digest")]
    NoDigest(String),
    /// A directory's tree that is not a `Tree`, or names a directory it does not hold.
    #[error("the tree of the output {path} does not hold together: {why}")]
    Tree {
        /// The output directory.
        path: String,
        /// What is wrong with it.
        why: String,
    },
    /// An output directory's tree with more files and directories than [`MAX_ENTRIES`].
    #[error("the output directory {0} holds more than {MAX_ENTRIES} files and directories")]
    TooMany(String),
    /// A name given twice in one directory of an output's tree.
    #[error("the output directory {at} names {name:?} twice")]
    Twice {
        /// The directory the name is in.
        at: String,
        /// The name.
        name: String,
    },
    /// A name in an output directory that is empty, `.`, `..` or holds a `/`.
    #[error("the output directory {at} names {name:?}, which is not a plain name")]
    BadName {
        /// The directory the name is in.
        at: String,
        /// The name.
        name: String,
    },
    /// A file or directory that could not be written.
    #[error("cannot write {path}: {why}")]
    Write {
        /// The local path.
        path: String,
        /// Why not.
        why: String,
    },
    /// A read from the farm that failed.
    #[error(transparent)]
    Farm(#[from] FarmError),
}

/// One file to fetch: its path below the output directory, its blob, and whether it is
/// executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    /// The path, relative to the working directory of the action.
    pub path: String,
    /// The blob of its bytes.
    pub digest: Digest,
    /// Whether it is executable.
    pub executable: bool,
}

/// What a result holds of the declared outputs: the files to fetch, and the directories with
/// the digests of their trees.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The output files.
    pub files: Vec<Want>,
    /// Each output directory and the digest of its `Tree` message.
    pub directories: Vec<(String, Digest)>,
}

/// The outputs of `result`, held to the `declared` paths: every declared path is a file or a
/// directory of the result, and the result has nothing else.
///
/// # Errors
/// [`OutputError`] for an output that is undeclared, missing, a symlink, or has no digest.
pub fn plan(declared: &BTreeSet<String>, result: &ActionResult) -> Result<Plan, OutputError> {
    // A farm of REAPI before 2.1 lists symlinks in the two fields 2.1 deprecated.
    #[allow(
        deprecated,
        reason = "an older farm answers with the deprecated symlink lists"
    )]
    let link = result
        .output_symlinks
        .iter()
        .map(|s| &s.path)
        .chain(result.output_file_symlinks.iter().map(|s| &s.path))
        .chain(result.output_directory_symlinks.iter().map(|s| &s.path))
        .next();
    if let Some(link) = link {
        return Err(OutputError::Symlink(link.clone()));
    }
    let mut seen = BTreeSet::new();
    let mut out = Plan::default();
    for file in &result.output_files {
        let digest = known(declared, &mut seen, &file.path, file.digest.as_ref())?;
        out.files.push(Want {
            path: file.path.clone(),
            digest,
            executable: file.is_executable,
        });
    }
    for dir in &result.output_directories {
        let digest = known(declared, &mut seen, &dir.path, dir.tree_digest.as_ref())?;
        out.directories.push((dir.path.clone(), digest));
    }
    if let Some(missing) = declared.difference(&seen).next() {
        return Err(OutputError::Missing(missing.clone()));
    }
    Ok(out)
}

/// The digest of the output at `path`, which must be declared and not seen before.
fn known(
    declared: &BTreeSet<String>,
    seen: &mut BTreeSet<String>,
    path: &str,
    digest: Option<&Digest>,
) -> Result<Digest, OutputError> {
    if !declared.contains(path) || !seen.insert(path.to_owned()) {
        return Err(OutputError::Undeclared(path.to_owned()));
    }
    digest
        .cloned()
        .ok_or_else(|| OutputError::NoDigest(path.to_owned()))
}

/// The files and the directories of the output directory `path` whose `Tree` message is
/// `tree`: each file at its path below `path`, and every directory, the empty ones too.
///
/// # Errors
/// [`OutputError`] for a tree that does not decode or names a child it lacks, a name that is not
/// plain, or a symlink.
pub fn walk(path: &str, tree: &[u8]) -> Result<(Vec<Want>, Vec<String>), OutputError> {
    let broken = |why: String| OutputError::Tree {
        path: path.to_owned(),
        why,
    };
    let tree = Tree::decode(tree).map_err(|e| broken(e.to_string()))?;
    let children: BTreeMap<String, &Directory> = tree
        .children
        .iter()
        .map(|d| (digest_of(&d.encode_to_vec()).hash, d))
        .collect();
    let root = tree.root.unwrap_or_default();
    let mut files = Vec::new();
    let mut dirs = vec![path.to_owned()];
    let mut todo = vec![(path.to_owned(), &root)];
    while let Some((at, dir)) = todo.pop() {
        if let Some(link) = dir.symlinks.first() {
            return Err(OutputError::Symlink(format!("{at}/{}", link.name)));
        }
        let mut names = BTreeSet::new();
        let fresh = |names: &mut BTreeSet<String>, name: &str| {
            if names.insert(name.to_owned()) {
                Ok(())
            } else {
                Err(OutputError::Twice {
                    at: at.clone(),
                    name: name.to_owned(),
                })
            }
        };
        if files.len() + dirs.len() + dir.files.len() + dir.directories.len() > MAX_ENTRIES {
            return Err(OutputError::TooMany(path.to_owned()));
        }
        for file in &dir.files {
            fresh(&mut names, &file.name)?;
            let inside = plain(&at, &file.name)?;
            let digest = file
                .digest
                .clone()
                .ok_or_else(|| OutputError::NoDigest(inside.clone()))?;
            files.push(Want {
                path: inside,
                digest,
                executable: file.is_executable,
            });
        }
        for sub in &dir.directories {
            fresh(&mut names, &sub.name)?;
            let inside = plain(&at, &sub.name)?;
            let hash = sub.digest.as_ref().map(|d| d.hash.as_str()).unwrap_or("");
            let child = children
                .get(hash)
                .ok_or_else(|| broken(format!("{inside} is not among its directories")))?;
            dirs.push(inside.clone());
            todo.push((inside, child));
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    dirs.sort();
    Ok((files, dirs))
}

/// `<at>/<name>`, when `name` is a plain name.
fn plain(at: &str, name: &str) -> Result<String, OutputError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(OutputError::BadName {
            at: at.to_owned(),
            name: name.to_owned(),
        });
    }
    Ok(format!("{at}/{name}"))
}

/// Writes `bytes` as the file `<into>/<path>`, its directories made, with mode 0755 when it is
/// executable and 0644 when not.
///
/// # Errors
/// [`OutputError::Write`].
pub fn write(into: &Path, path: &str, bytes: &[u8], executable: bool) -> Result<(), OutputError> {
    use std::os::unix::fs::PermissionsExt as _;
    let local = into.join(path);
    let failed = |e: std::io::Error| OutputError::Write {
        path: local.display().to_string(),
        why: e.to_string(),
    };
    std::fs::create_dir_all(local.parent().unwrap_or(into)).map_err(failed)?;
    std::fs::write(&local, bytes).map_err(failed)?;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode)).map_err(failed)
}

/// Fetches the outputs `declared` of `result` from `farm` and writes them below `into`; the
/// number of files and of bytes written.
///
/// # Errors
/// [`OutputError`]: an output the plan refuses, a read that fails, a blob that is not its
/// digest, a file that cannot be written.
pub async fn fetch(
    farm: &Farm,
    declared: &BTreeSet<String>,
    result: &ActionResult,
    into: &Path,
) -> Result<(usize, usize), OutputError> {
    let plan = plan(declared, result)?;
    let mut files = plan.files;
    let mut dirs = Vec::new();
    for (path, digest) in &plan.directories {
        let (inside, made) = walk(path, &farm.read(digest).await?)?;
        files.extend(inside);
        dirs.extend(made);
    }
    let mut fetched = Vec::with_capacity(files.len());
    for want in &files {
        fetched.push(farm.read(&want.digest).await?);
    }
    for dir in &dirs {
        let local = into.join(dir);
        std::fs::create_dir_all(&local).map_err(|e| OutputError::Write {
            path: local.display().to_string(),
            why: e.to_string(),
        })?;
    }
    let mut bytes = 0;
    for (want, contents) in files.iter().zip(&fetched) {
        write(into, &want.path, contents, want.executable)?;
        bytes += contents.len();
    }
    Ok((files.len(), bytes))
}
