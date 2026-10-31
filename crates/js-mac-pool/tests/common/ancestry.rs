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

/// The uids whose ancestors are acceptable: root, and `owner`, the invoking user's uid as
/// the operator gave it (never read off the file system: whoever made a directory could
/// then make themselves trusted). `owner` is not 0.
pub fn allowed_owners(owner: u32) -> Vec<u32> {
    assert!(owner != 0, "the invoking user is not root");
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
