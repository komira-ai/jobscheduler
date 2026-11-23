//! Where a Mac's bundle store may lie: beside the scratch root, not in it, and below only
//! directories a lease user can walk through.
//!
//! A native job runs as a lease user, a pool account that is not root's, and the bundle's
//! programs run from the store's trees, so every directory from `/` down to the store must be
//! searchable by others (`o+x`). Each must also be root's and closed to writes by group and
//! others: a directory another account could write would let it swap what a root-owned store
//! stands on. The agent's start sweeps every entry of the scratch root that is not a run's
//! directory, so a store inside the scratch root would be deleted at each start; the volume
//! the scratch root is on is mounted on its parent, and the store is a directory beside it.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

/// The mode of the store directory the agent makes: search only for others, so that a lease
/// user can walk down through it and cannot list it.
pub const STORE_MODE: u32 = 0o711;

/// What the check needs to know of a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirFacts {
    /// Whether it is a directory (a link followed).
    pub is_dir: bool,
    /// Its owner.
    pub uid: u32,
    /// Its permission bits.
    pub mode: u32,
}

/// Why the store `store` cannot be where the scratch root `scratch` is: it is inside it, where
/// the agent's start would sweep it away. `None` for a store that lies elsewhere.
#[must_use]
pub fn inside_scratch(store: &Path, scratch: &Path) -> Option<String> {
    store.starts_with(scratch).then(|| {
        format!(
            "{} is inside --scratch-root {}: the agent's start removes every entry of the \
             scratch root that is not a run's directory, so the store would be deleted at each \
             start; put it beside the scratch root, on the same volume",
            store.display(),
            scratch.display()
        )
    })
}

/// Checks that a lease user can search every directory from `/` down to `store` itself: each
/// is a directory of `owner`, no group or others can write it, and others can search it
/// (`o+x`). `look` reads a directory, a link followed.
///
/// # Errors
///
/// The text names the first directory, from the top, that fails, and how.
pub fn lease_users_reach(
    store: &Path,
    owner: u32,
    look: &dyn Fn(&Path) -> io::Result<DirFacts>,
) -> Result<(), String> {
    let mut chain: Vec<&Path> = store.ancestors().collect();
    chain.reverse();
    for dir in chain {
        let fault = match look(dir) {
            Err(why) => format!("cannot be looked at: {why}"),
            Ok(facts) if !facts.is_dir => "is not a directory".to_owned(),
            Ok(facts) if facts.uid != owner => format!("is not owned by uid {owner}"),
            Ok(facts) if facts.mode & 0o022 != 0 => "is writable by group or others".to_owned(),
            Ok(facts) if facts.mode & 0o001 == 0 => {
                "cannot be searched by others (no o+x)".to_owned()
            }
            Ok(_) => continue,
        };
        return Err(format!(
            "{} {fault}: a lease user could not walk down to the store through it",
            dir.display()
        ));
    }
    Ok(())
}

/// Makes the directory `store`, mode [`STORE_MODE`] whatever the umask says, when it is not
/// there; its parent must be. A store that is there is left as it is.
///
/// # Errors
///
/// The directory cannot be made or its mode set.
pub fn make_store(store: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(STORE_MODE).create(store) {
        Ok(()) => fs::set_permissions(store, Permissions::from_mode(STORE_MODE)),
        Err(why) if why.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(why) => Err(why),
    }
}

/// What [`lease_users_reach`] reads of the directory `dir` on this machine.
pub(crate) fn look(dir: &Path) -> io::Result<DirFacts> {
    fs::metadata(dir).map(|meta| DirFacts {
        is_dir: meta.is_dir(),
        uid: meta.uid(),
        mode: meta.mode(),
    })
}
