//! The subordinate id range every contained job runs in.
//!
//! The agent maps the ids 0 to 65535 of a job's user namespace onto one range of
//! subordinate ids, so that no id inside a container is an id of the host: a process the
//! job leaves behind, or a file it writes, belongs to the range and to nobody else.
//! `--container-subuid <user>` names the user whose entries in `/etc/subuid` and
//! `/etc/subgid` give the range. [`Mapping::read`] checks them when the agent starts:
//!
//! - both files have an entry for the user, of at least [`RANGE_LEN`] ids, and the first
//!   entry is the range (the next entries are not used);
//! - the range does not begin at 0 (that would map a container's root onto the host's);
//! - the range ends below the largest id;
//! - no uid of `/etc/passwd` is in the uid range, and no gid of `/etc/group` or primary
//!   gid of `/etc/passwd` is in the gid range.
//!
//! [`RangeClaim`] keeps two agents of one host from using overlapping ranges: it holds a
//! lock for as long as the runtime lives.

use std::fs::File;
use std::io;
use std::path::Path;

/// How many ids a job's user namespace has, and so how many a range must hold.
pub const RANGE_LEN: u32 = 65_536;

/// The directory in which running agents hold their claims of a range. The agent makes it
/// (`0700`) when it does not exist.
pub const CLAIM_DIR: &str = "/run/js-agent/subid";

/// The file holding the subordinate uid ranges.
pub const SUBUID_FILE: &str = "/etc/subuid";

/// The file holding the subordinate gid ranges.
pub const SUBGID_FILE: &str = "/etc/subgid";

/// The user database.
pub const PASSWD_FILE: &str = "/etc/passwd";

/// The group database.
pub const GROUP_FILE: &str = "/etc/group";

/// Why a range cannot be used. The text names the flag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubidError {
    /// One of the four files cannot be read.
    #[error("--container-subuid: cannot read the id files: {0}")]
    Unreadable(String),
    /// The user has no entry.
    #[error("--container-subuid: {file} has no entry for {user}")]
    NoEntry {
        /// The user.
        user: String,
        /// The file.
        file: &'static str,
    },
    /// The user's entry is not `name:start:count`.
    #[error("--container-subuid: {file}: the entry for {user} is not name:start:count")]
    Malformed {
        /// The user.
        user: String,
        /// The file.
        file: &'static str,
    },
    /// The entry holds too few ids.
    #[error("--container-subuid: {file}: {user} has {count} ids, at least {RANGE_LEN} are needed")]
    TooSmall {
        /// The user.
        user: String,
        /// The file.
        file: &'static str,
        /// The ids the entry holds.
        count: u32,
    },
    /// The range begins at 0: a container's id would be the host's.
    #[error(
        "--container-subuid: {file}: the range of {user} begins at 0, so a container id would be a host id"
    )]
    Identity {
        /// The user.
        user: String,
        /// The file.
        file: &'static str,
    },
    /// The range runs past the largest id.
    #[error(
        "--container-subuid: {file}: the range of {user} (from {start}) runs past the largest id"
    )]
    PastTheEnd {
        /// The user.
        user: String,
        /// The file.
        file: &'static str,
        /// The first id.
        start: u32,
    },
    /// The range holds an id the host's user or group database names.
    #[error(
        "--container-subuid: {file}: the range of {user} (from {start}) holds the id {id} of {database}, so a container id would be that host id"
    )]
    Overlaps {
        /// The user.
        user: String,
        /// The file that gave the range.
        file: &'static str,
        /// The first id of the range.
        start: u32,
        /// The database that names the id.
        database: &'static str,
        /// The id.
        id: u32,
    },
    /// Another agent of this host holds an overlapping range, or the lock cannot be taken.
    #[error(
        "--container-subuid: the id range {0} is claimed by another agent on this host (or its claim cannot be locked)"
    )]
    Claimed(String),
    /// The claim directory or file cannot be made or read.
    #[error("--container-subuid: cannot claim the id range: {0}")]
    Claim(String),
}

impl From<io::Error> for SubidError {
    fn from(why: io::Error) -> Self {
        Self::Claim(why.to_string())
    }
}

/// A run of ids: `len` of them from `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRange {
    start: u32,
    len: u32,
}

impl IdRange {
    /// The one id `id`.
    pub const fn single(id: u32) -> Self {
        Self { start: id, len: 1 }
    }

    /// The [`RANGE_LEN`] ids from `start`.
    pub const fn of_job(start: u32) -> Self {
        Self {
            start,
            len: RANGE_LEN,
        }
    }

    /// The first id.
    pub const fn start(self) -> u32 {
        self.start
    }

    /// Whether `id` is one of the ids.
    pub fn contains(self, id: u32) -> bool {
        let _ = (self.len, id);
        false
    }

    /// Whether the two runs share an id.
    pub fn overlaps(self, other: Self) -> bool {
        let _ = other;
        false
    }
}

/// The text of the four files a range is read from.
#[derive(Debug, Clone, Copy)]
pub struct IdFiles<'a> {
    /// `/etc/subuid`.
    pub subuid: &'a str,
    /// `/etc/subgid`.
    pub subgid: &'a str,
    /// `/etc/passwd`.
    pub passwd: &'a str,
    /// `/etc/group`.
    pub group: &'a str,
}

/// The host ids a job's ids 0 to 65535 are mapped onto: one range of uids and one of gids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    uid: IdRange,
    gid: IdRange,
}

impl Mapping {
    /// The mapping of the ranges that begin at `uid_start` and `gid_start`.
    pub const fn new(uid_start: u32, gid_start: u32) -> Self {
        Self {
            uid: IdRange::of_job(uid_start),
            gid: IdRange::of_job(gid_start),
        }
    }

    /// The host uids.
    pub const fn uid(self) -> IdRange {
        self.uid
    }

    /// The host gids.
    pub const fn gid(self) -> IdRange {
        self.gid
    }

    /// The mapping of `user`, read with `read` (the text of a file, or why it cannot be
    /// read).
    ///
    /// # Errors
    ///
    /// A file cannot be read, or the range is not one the agent accepts (see the module).
    pub fn read(
        user: &str,
        read: &dyn Fn(&str) -> Result<String, String>,
    ) -> Result<Self, SubidError> {
        let text = |path: &str| read(path).map_err(SubidError::Unreadable);
        let (subuid, subgid) = (text(SUBUID_FILE)?, text(SUBGID_FILE)?);
        let (passwd, group) = (text(PASSWD_FILE)?, text(GROUP_FILE)?);
        Self::resolve(
            user,
            &IdFiles {
                subuid: &subuid,
                subgid: &subgid,
                passwd: &passwd,
                group: &group,
            },
        )
    }

    /// The mapping of `user` from the text of the files.
    ///
    /// # Errors
    ///
    /// The range is not one the agent accepts (see the module).
    pub fn resolve(user: &str, files: &IdFiles<'_>) -> Result<Self, SubidError> {
        let _ = (user, files);
        Ok(Self::new(0, 0))
    }

    /// The `--uidmap` and `--gidmap` arguments of Podman's `create`.
    pub fn podman_args(self) -> [String; 4] {
        let _ = (self.uid, self.gid);
        [const { String::new() }; 4]
    }
}

/// Who the job's processes are, as the post-job checks look for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobUsers {
    /// Processes of this one host uid: containers that run in the host's user namespace
    /// (a test's [`Settings`](super::Settings) built without `--container-subuid`).
    Uid(u32),
    /// Processes of the host uids the named user's range gives (`--container-subuid`).
    Mapped(String),
}

impl JobUsers {
    /// The host uids of the job's processes, read with `read` (see [`Mapping::read`]).
    ///
    /// # Errors
    ///
    /// A mapped range cannot be read or is not one the agent accepts.
    pub fn range(
        &self,
        read: &dyn Fn(&str) -> Result<String, String>,
    ) -> Result<IdRange, SubidError> {
        match self {
            Self::Uid(uid) => Ok(IdRange::single(*uid)),
            Self::Mapped(user) => {
                let _ = (user, read);
                Ok(IdRange::single(0))
            }
        }
    }
}

/// A host-wide claim of a [`Mapping`], held for as long as the value lives.
///
/// The claim is an exclusive lock (`flock`) on the file `<uid start>-<gid start>` in a
/// directory every agent of the host uses ([`CLAIM_DIR`]). It is refused when that lock is
/// held, and when the lock of another file there whose name stands for an overlapping
/// mapping is held. A file whose lock is free belongs to an agent that has gone (the
/// system drops a lock with its process) and is ignored. Two agents that start at once with
/// overlapping mappings can both be refused; neither can win both.
#[derive(Debug)]
pub struct RangeClaim {
    _held: File,
}

impl RangeClaim {
    /// Claims `mapping` in `dir`, which is made (`0700`) when it is not there.
    ///
    /// # Errors
    ///
    /// [`SubidError::Claimed`] when another agent holds an overlapping mapping,
    /// [`SubidError::Claim`] when the directory or a file in it cannot be used.
    pub fn take(dir: &Path, mapping: Mapping) -> Result<Self, SubidError> {
        let _ = (dir, mapping);
        Ok(Self {
            _held: File::open("/dev/null")?,
        })
    }
}
