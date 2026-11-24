//! The input root of a farm action as REAPI's Merkle tree: every file and symlink at its path,
//! each directory a `Directory` message whose files, directories and symlinks are sorted by
//! name, and every message and file content a blob keyed by its SHA-256.

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;

use std::collections::BTreeMap;

use super::proto::reapi::{Digest, Directory, DirectoryNode, FileNode, SymlinkNode};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

/// One entry of the input root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A regular file.
    File {
        /// Its contents.
        bytes: Vec<u8>,
        /// Whether it is executable.
        executable: bool,
    },
    /// A symlink and its target, as it is: neither checked nor followed.
    Symlink(String),
}

/// Why the entries make no tree.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// A path that is empty, absolute, or has an empty, `.` or `..` component.
    #[error("the path {0:?} is not a relative path of plain components")]
    BadPath(String),
    /// Two entries at one path.
    #[error("the path {0:?} is given twice")]
    Twice(String),
    /// A path below another entry's file, or a file where another entry's directory is.
    #[error("the path {0:?} is both a file and a directory")]
    FileAndDirectory(String),
}

/// An input root: its digest and every blob it is made of, by hash.
#[derive(Debug, Default)]
pub struct Tree {
    /// The digest of the root `Directory`.
    pub root: Digest,
    /// Every blob of the tree (file contents and `Directory` messages) by its SHA-256 in hex.
    pub blobs: BTreeMap<String, Vec<u8>>,
}

/// The digest of `bytes`.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
    }
}

/// The directories of `path` and its last name: `/`-separated components, none empty, `.` or
/// `..`.
///
/// # Errors
/// [`TreeError::BadPath`] for a path that is not plain.
pub fn components(path: &str) -> Result<(Vec<&str>, &str), TreeError> {
    let (dirs, name) = match path.rsplit_once('/') {
        Some((dirs, name)) => (dirs.split('/').collect(), name),
        None => (Vec::new(), path),
    };
    let bad = |p: &str| p.is_empty() || p == "." || p == "..";
    if bad(name) || dirs.iter().any(|d| bad(d)) {
        return Err(TreeError::BadPath(path.to_owned()));
    }
    Ok((dirs, name))
}

#[derive(Default)]
struct Node {
    entries: BTreeMap<String, Entry>,
    dirs: BTreeMap<String, Node>,
}

/// The tree of `entries`, each at its `/`-separated path relative to the root.
///
/// # Errors
/// [`TreeError`] for a path that is not plain, given twice, or both a file and a directory.
pub fn build(entries: Vec<(String, Entry)>) -> Result<Tree, TreeError> {
    let mut root = Node::default();
    for (path, entry) in entries {
        let (dirs, name) = components(&path)?;
        let mut node = &mut root;
        for dir in dirs {
            if node.entries.contains_key(dir) {
                return Err(TreeError::FileAndDirectory(path.clone()));
            }
            node = node.dirs.entry(dir.to_owned()).or_default();
        }
        if node.dirs.contains_key(name) {
            return Err(TreeError::FileAndDirectory(path.clone()));
        }
        if node.entries.insert(name.to_owned(), entry).is_some() {
            return Err(TreeError::Twice(path.clone()));
        }
    }
    let mut blobs = BTreeMap::new();
    let root = encode(root, &mut blobs);
    Ok(Tree { root, blobs })
}

/// Adds `bytes` to `blobs`; its digest.
pub fn add(blobs: &mut BTreeMap<String, Vec<u8>>, bytes: Vec<u8>) -> Digest {
    let digest = digest_of(&bytes);
    blobs.insert(digest.hash.clone(), bytes);
    digest
}

/// The `Directory` message of `node`, and of each directory below it, into `blobs`; its digest.
fn encode(node: Node, blobs: &mut BTreeMap<String, Vec<u8>>) -> Digest {
    let mut dir = Directory::default();
    for (name, child) in node.dirs {
        let digest = encode(child, blobs);
        dir.directories.push(DirectoryNode {
            name,
            digest: Some(digest),
        });
    }
    for (name, entry) in node.entries {
        match entry {
            Entry::File { bytes, executable } => {
                let digest = add(blobs, bytes);
                dir.files.push(FileNode {
                    name,
                    digest: Some(digest),
                    is_executable: executable,
                    ..FileNode::default()
                });
            }
            Entry::Symlink(target) => dir.symlinks.push(SymlinkNode {
                name,
                target,
                ..SymlinkNode::default()
            }),
        }
    }
    add(blobs, dir.encode_to_vec())
}
