//! Who a job runs as: container root, or the user its image names.
//!
//! An image config's `User` is resolved as Docker resolves it. `uid`, `uid:gid`, `name`,
//! `name:group`, `uid:group` and `name:gid` are accepted. A name is looked up in the image's
//! own `/etc/passwd` and `/etc/group`, read through the image's root (a link in the path
//! leads only inside the root, never to the host). A numeric user the passwd file does not
//! list keeps group 0 unless a group is given. A user found in the passwd file and given no
//! group also gets the groups whose member list names it. No `User` is root, 0:0.
//!
//! Every id must be one the job's namespace maps (below [`ACTION_SIZE`]).

use std::io::Read as _;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, ResolveFlags};

use super::DaemonIds;

/// The most bytes read of `/etc/passwd` or `/etc/group`.
const FILE_LIMIT: u64 = 1 << 20;

/// The ids a job's process runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The uid in the container.
    pub uid: u32,
    /// The gid in the container.
    pub gid: u32,
    /// The supplementary gids.
    pub additional_gids: Vec<u32>,
}

impl Identity {
    /// Container root: what a job with no `User` runs as.
    pub(crate) fn root() -> Self {
        Self {
            uid: crate::container::ACTION_ID,
            gid: crate::container::ACTION_ID,
            additional_gids: Vec::new(),
        }
    }
}

/// Why a `User` cannot be resolved.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    /// The text is not `user` or `user:group` with each part non-empty.
    #[error("the image's User {0:?} is not user, user:group, uid or uid:gid")]
    Syntax(String),
    /// A name the image's passwd file does not list.
    #[error("the image's User names {0:?}, which the image's /etc/passwd does not list")]
    NoUser(String),
    /// A group name the image's group file does not list.
    #[error("the image's User names the group {0:?}, which the image's /etc/group does not list")]
    NoGroup(String),
    /// An id the job's namespace does not map.
    #[error("the image's User {0:?} has an id above the {1} the job's namespace maps")]
    Range(String, u32),
    /// A passwd or group file that exists could not be read.
    #[error("{}: {why}", path.display())]
    Read {
        /// The file, in the image's root.
        path: PathBuf,
        /// The kernel's answer.
        why: String,
    },
}

/// The identity `user` (an image's `User`, `""` for none) names, in the root `root`.
///
/// # Errors
/// See [`UserError`].
pub(crate) fn resolve(user: &str, root: &Path) -> Result<Identity, UserError> {
    if user.is_empty() {
        return Ok(Identity::root());
    }
    let (name, group) = match user.split_once(':') {
        Some((name, group)) => (name, Some(group)),
        None => (user, None),
    };
    if name.is_empty() || group == Some("") {
        return Err(UserError::Syntax(user.to_owned()));
    }
    let passwd = read_table(root, "etc/passwd")?;
    let groups = read_table(root, "etc/group")?;
    // passwd: name:pw:uid:gid:...   group: name:pw:gid:member,member
    let entry = passwd
        .iter()
        .find(|f| f.first().is_some_and(|n| n == name) || f.get(2).is_some_and(|uid| uid == name));
    let listed = entry.and_then(|f| {
        Some((
            f.get(2)?.parse::<u32>().ok()?,
            f.get(3)?.parse::<u32>().ok()?,
        ))
    });
    let (uid, primary) = match (listed, name.parse::<u32>()) {
        (Some(ids), _) => ids,
        (None, Ok(uid)) => (uid, crate::container::ACTION_ID),
        (None, Err(_)) => return Err(UserError::NoUser(name.to_owned())),
    };
    let gid = match group {
        None => primary,
        Some(group) => {
            let found = groups.iter().find(|f| {
                f.first().is_some_and(|n| n == group) || f.get(2).is_some_and(|g| g == group)
            });
            match (
                found.and_then(|f| f.get(2)?.parse::<u32>().ok()),
                group.parse::<u32>(),
            ) {
                (Some(gid), _) | (None, Ok(gid)) => gid,
                (None, Err(_)) => return Err(UserError::NoGroup(group.to_owned())),
            }
        }
    };
    let additional_gids = match (group, entry.and_then(|f| f.first())) {
        (None, Some(login)) => groups
            .iter()
            .filter(|f| f.get(3).is_some_and(|m| m.split(',').any(|m| m == login)))
            .filter_map(|f| f.get(2)?.parse().ok())
            .filter(|g| *g != gid)
            .collect(),
        _ => Vec::new(),
    };
    let mapped = DaemonIds::ACTION_SIZE;
    let all = [uid, gid]
        .into_iter()
        .chain(additional_gids.iter().copied());
    if all.into_iter().any(|id| id >= mapped) {
        return Err(UserError::Range(user.to_owned(), mapped));
    }
    Ok(Identity {
        uid,
        gid,
        additional_gids,
    })
}

/// The colon-separated lines of `rel` in `root`, comments and blank lines left out; none for a
/// file that is not there.
fn read_table(root: &Path, rel: &str) -> Result<Vec<Vec<String>>, UserError> {
    let path = root.join(rel);
    let read = |why: String| UserError::Read {
        path: path.clone(),
        why,
    };
    let dir = rustix::fs::openat(
        CWD,
        root,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| read(e.to_string()))?;
    let resolve = ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS;
    let file = match rustix::fs::openat2(
        &dir,
        rel,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    ) {
        Ok(file) => std::fs::File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
        Err(e) => return Err(read(e.to_string())),
    };
    let mut text = String::new();
    file.take(FILE_LIMIT)
        .read_to_string(&mut text)
        .map_err(|e| read(e.to_string()))?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split(':').map(str::to_owned).collect())
        .collect())
}
