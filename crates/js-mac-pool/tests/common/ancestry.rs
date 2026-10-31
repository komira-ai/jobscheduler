//! Whether the directories above a work root can be trusted: a root-owned directory is
//! no safer than the path that leads to it, because whoever owns or can write a directory
//! on that path can swap what it holds.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

const GROUP_OR_OTHER_WRITE: u32 = 0o022;
const STICKY: u32 = 0o1000;

/// Why an ancestor is not trusted.
#[derive(Debug, PartialEq, Eq)]
pub enum Reason {
    /// Owned by a uid that is neither root nor the root's parent's owner.
    Owner(u32),
    /// Group- or other-writable without the sticky bit (the full mode is kept).
    Writable(u32),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub path: PathBuf,
    pub reason: Reason,
}

/// The uids whose ancestors are acceptable: root, and the owner of `root`'s own parent
/// (the invoking user, when the work root sits in the directory that user made).
pub fn allowed_owners(root: &Path) -> Vec<u32> {
    let parent = root.parent().unwrap_or(root);
    let owner = std::fs::symlink_metadata(parent)
        .expect("the work root's parent")
        .uid();
    vec![0, owner]
}

/// The nearest ancestor of `root`, up to and including `top`, that is owned by a uid not
/// in `allowed`, or is group- or other-writable without the sticky bit.
pub fn untrusted_ancestor(root: &Path, top: &Path, allowed: &[u32]) -> Option<Refusal> {
    for dir in root.ancestors().skip(1) {
        let meta = std::fs::symlink_metadata(dir).expect("an ancestor of the work root");
        let mode = meta.mode();
        if !allowed.contains(&meta.uid()) {
            return Some(Refusal {
                path: dir.to_owned(),
                reason: Reason::Owner(meta.uid()),
            });
        }
        if mode & GROUP_OR_OTHER_WRITE != 0 && mode & STICKY == 0 {
            return Some(Refusal {
                path: dir.to_owned(),
                reason: Reason::Writable(mode & 0o7777),
            });
        }
        if dir == top {
            break;
        }
    }
    None
}
