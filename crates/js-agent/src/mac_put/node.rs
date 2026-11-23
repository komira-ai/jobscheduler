//! The put directory handed to the tierstore node, before anything is written in it: the
//! node runs as its own user, stores what is committed and then removes the file and the
//! job's directory, so it must own the folders of the key and be able to write in the job's
//! directory.

use std::io;
use std::os::unix::fs::lchown;
use std::path::Path;

use rustix::fs::{Mode, OFlags};

use super::commit::PutTarget;

/// The mode of a job's put directory: its owner's (the agent's) and its group's (the
/// node's), no one else's.
pub const DIR_MODE: u32 = 0o770;

/// The user the node runs as: its uid and the group the job's directory is given to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    /// The node's uid.
    pub uid: u32,
    /// The node's group.
    pub gid: u32,
}

/// Hands the put directory of `target` below `root` to `node`: `cas`, `cas/b` and
/// `cas/b/<build>` become the node's (user and group), so it may remove the job's directory
/// once it has stored the object; the job's directory stays `owner`'s (the agent's) with the
/// node's group, mode [`DIR_MODE`], so the node may read the file and remove it, and no one
/// else may enter. Every name is given without following a link (`lchown`, and the mode set
/// on the directory opened `O_NOFOLLOW`): a link in the job directory's place fails the
/// hand-over and leaves what it names as it was.
///
/// # Errors
///
/// A change of owner or of mode failed; the error names the path.
pub fn hand_over(root: &Path, target: &PutTarget, owner: u32, node: Node) -> io::Result<()> {
    let named = |path: &Path, why: io::Error| {
        io::Error::new(why.kind(), format!("{}: {why}", path.display()))
    };
    let mut at = root.to_owned();
    let folders = target.key_prefix.split('/').take(3);
    for part in folders {
        at.push(part);
        lchown(&at, Some(node.uid), Some(node.gid)).map_err(|why| named(&at, why))?;
    }
    let dir = &target.dir;
    lchown(dir, Some(owner), Some(node.gid))
        .and_then(|()| chmod_no_follow(dir))
        .map_err(|why| named(dir, why))
}

/// Sets [`DIR_MODE`] on the directory `dir` itself: it is opened without following a link
/// (`O_NOFOLLOW`), so a link in its place is an error and what it names keeps its mode.
fn chmod_no_follow(dir: &Path) -> io::Result<()> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = rustix::fs::open(dir, flags, Mode::empty())?;
    // `RWXU | RWXG` is [`DIR_MODE`], spelled by flags so it fits each system's mode type.
    Ok(rustix::fs::fchmod(&opened, Mode::RWXU | Mode::RWXG)?)
}
