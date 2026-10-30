//! Sealing: making an extracted stage a read-only tree, and undoing it for deletion.

use std::path::Path;

use rustix::process::{Gid, Uid, getegid, geteuid};

/// What sealing a stage found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sealed {
    /// The bytes the tree takes on the volume (`st_blocks` times the block size).
    pub bytes: u64,
}

/// A user and group that own a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    uid: Uid,
    gid: Gid,
}

impl Owner {
    /// The user and group with these ids; `None` for `u32::MAX`, which `chown` reads as
    /// "leave as it is".
    #[must_use]
    pub fn new(uid: u32, gid: u32) -> Option<Owner> {
        (uid != u32::MAX && gid != u32::MAX).then(|| Owner {
            uid: Uid::from_raw(uid),
            gid: Gid::from_raw(gid),
        })
    }

    /// `root:wheel`, for the sealer that runs as root.
    #[must_use]
    pub fn root() -> Owner {
        Owner {
            uid: Uid::ROOT,
            gid: Gid::ROOT,
        }
    }

    /// The caller's own user and group, for a sealer that does not run as root.
    #[must_use]
    pub fn caller() -> Owner {
        Owner {
            uid: geteuid(),
            gid: getegid(),
        }
    }

    /// The user and group ids.
    #[must_use]
    pub fn ids(self) -> (u32, u32) {
        (self.uid.as_raw(), self.gid.as_raw())
    }

    pub(crate) fn uid(self) -> Uid {
        self.uid
    }

    pub(crate) fn gid(self) -> Gid {
        self.gid
    }
}

/// The sealer failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SealError(pub String);

impl From<std::io::Error> for SealError {
    fn from(error: std::io::Error) -> SealError {
        SealError(error.to_string())
    }
}

impl From<rustix::io::Errno> for SealError {
    fn from(error: rustix::io::Errno) -> SealError {
        SealError(std::io::Error::from(error).to_string())
    }
}

/// The platform's way to make a tree immutable. The macOS sealer (root, `uchg`) is a
/// separate component; the tests use a fake.
///
/// Implementations must not panic.
pub trait Sealer: Send + Sync + 'static {
    /// Re-checks the stage, normalises owner and modes, makes it immutable.
    ///
    /// **Durability:** it returns only after the tree's file data and its directories have
    /// been synced to disk (`fsync` of every file and directory, or the platform's
    /// equivalent). The store writes the tree's record after this and after syncing
    /// `trees/` for the rename, so a record that survives a power loss never sits beside a
    /// tree with missing data. `unpack_layer` does not sync.
    ///
    /// # Errors
    /// The stage breaks a rule or a call fails.
    fn seal(&self, stage: &Path) -> Result<Sealed, SealError>;
    /// Clears immutability on a tree, or on a stage that was sealed in part, and on
    /// everything under it, so that it can be deleted. Called on a tree after it was moved to
    /// `trash/`, and before a second try to move it when the first move was refused (the move
    /// of an immutable directory fails on some systems). It must be safe to call again, and
    /// on a tree that is not sealed. A file with another hard link is not unflagged (its
    /// inode may not be the tree's) and is reported. If the second move is refused too, the store calls
    /// [`Sealer::seal`] on the tree where it stands, so `seal` must accept a tree in place.
    ///
    /// # Errors
    /// A call fails.
    fn unseal_for_delete(&self, tree: &Path) -> Result<(), SealError>;
    /// Makes `dst`, a copy of the sealed tree or file `src` that `owner` can write and that
    /// shares its data blocks with `src` where the platform allows. `dst` must not exist
    /// and must have a parent directory that does. A link at `src` is refused, not
    /// followed. Directories end `0700`, files `0700` if they were executable and `0600`
    /// otherwise, all owned by `owner` and with no immutable flag or access control list;
    /// `src` is not changed.
    ///
    /// The copy is complete before it returns. If it fails after `dst` was made, `dst` is
    /// removed again (the error says so if that fails too). The parent of `dst` is opened
    /// once and everything is done relative to it, but the directories above it must not be
    /// writable by anyone the caller does not trust.
    ///
    /// # Errors
    /// `src` is not a directory or regular file, the tree holds an entry the sealer does not
    /// admit (a hard link, a device or pipe, nesting past the depth limit), or a call fails.
    fn clone_writable(&self, src: &Path, dst: &Path, owner: Owner) -> Result<(), SealError>;
}
