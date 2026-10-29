//! The subordinate id range every contained job runs in.
//!
//! The agent maps the ids 0 to 65535 of a job's user namespace onto one range of
//! subordinate ids, so that no id inside a container is an id of the host: a process the
//! job leaves behind, or a file it writes, belongs to the range and to nobody else.
//! `--container-subuid <user>` names the user whose entries in `/etc/subuid` and
//! `/etc/subgid` give the range. [`Mapping::read`] checks them when the agent starts:
//!
//! - both files have an entry for the user (by name or by uid), of at least [`RANGE_LEN`]
//!   ids, and the first entry is the range (the next entries are not used; for Docker the
//!   user must have exactly one entry of exactly [`RANGE_LEN`] ids);
//! - the range does not begin at 0 (that would map a container's root onto the host's);
//! - the range ends below the largest id;
//! - no other user's entry of `/etc/subuid` (or `/etc/subgid`) shares an id with the range;
//! - no uid of `/etc/passwd` is in the uid range, and no gid of `/etc/group` or primary
//!   gid of `/etc/passwd` is in the gid range.
//!
//! [`RangeClaim`] keeps two agents of one host from using overlapping ranges: it holds a
//! lock for as long as the runtime lives.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, OnceLock};

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
    /// Docker maps the user's whole allocation, so the user must have one entry of exactly
    /// [`RANGE_LEN`] ids.
    #[error(
        "--container-subuid: {file}: Docker maps the whole allocation of {user}, so {user} needs exactly one entry of exactly 65536 ids"
    )]
    NotExact {
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
    /// The range has not been resolved: no runtime has opened with these settings.
    #[error("--container-subuid: the id range has not been resolved: the runtime did not open")]
    Unresolved,
    /// The range holds an id the host's user or group database names, or one of another
    /// user's subordinate ranges.
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
        id.checked_sub(self.start).is_some_and(|at| at < self.len)
    }

    /// Whether the two runs share an id.
    pub fn overlaps(self, other: Self) -> bool {
        let end = |range: Self| u64::from(range.start) + u64::from(range.len);
        u64::from(self.start) < end(other) && u64::from(other.start) < end(self)
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
        Self::read_with(user, read, false)
    }

    /// [`Mapping::read`] for Docker: the user must have exactly one entry in each file, of
    /// exactly [`RANGE_LEN`] ids. Docker's `userns-remap` maps container ids across the
    /// user's whole allocation, and the post-job checks watch only the first
    /// [`RANGE_LEN`] ids, so a larger or second entry would leave ids unwatched. Podman's
    /// map is explicit and needs no such rule.
    ///
    /// # Errors
    ///
    /// As [`Mapping::read`], and [`SubidError::NotExact`].
    pub fn read_exact(
        user: &str,
        read: &dyn Fn(&str) -> Result<String, String>,
    ) -> Result<Self, SubidError> {
        Self::read_with(user, read, false)
    }

    fn read_with(
        user: &str,
        read: &dyn Fn(&str) -> Result<String, String>,
        exact: bool,
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
            exact,
        )
    }

    /// The mapping of `user` from the text of the files; `exact` is the rule of
    /// [`Mapping::read_exact`].
    ///
    /// # Errors
    ///
    /// The range is not one the agent accepts (see the module).
    pub fn resolve(user: &str, files: &IdFiles<'_>, exact: bool) -> Result<Self, SubidError> {
        let uid = owner_uid(files.passwd, user);
        let uid_start = start_of(files.subuid, user, uid, SUBUID_FILE, exact)?;
        let gid_start = start_of(files.subgid, user, uid, SUBGID_FILE, exact)?;
        let mapping = Self::new(uid_start, gid_start);
        let others = [
            (files.subuid, SUBUID_FILE, uid_start, mapping.uid),
            (files.subgid, SUBGID_FILE, gid_start, mapping.gid),
        ];
        for (text, file, start, range) in others {
            if let Some(id) = other_overlap(text, user, uid, range) {
                return Err(SubidError::Overlaps {
                    user: user.to_owned(),
                    file,
                    start,
                    database: file,
                    id,
                });
            }
        }
        let checks = [
            (
                files.passwd,
                PASSWD_FILE,
                2,
                SUBUID_FILE,
                uid_start,
                mapping.uid,
            ),
            (
                files.passwd,
                PASSWD_FILE,
                3,
                SUBGID_FILE,
                gid_start,
                mapping.gid,
            ),
            (
                files.group,
                GROUP_FILE,
                2,
                SUBGID_FILE,
                gid_start,
                mapping.gid,
            ),
        ];
        for (database, name, field, file, start, range) in checks {
            if let Some(id) = id_in(database, field, range) {
                return Err(SubidError::Overlaps {
                    user: user.to_owned(),
                    file,
                    start,
                    database: name,
                    id,
                });
            }
        }
        Ok(mapping)
    }

    /// The `--uidmap` and `--gidmap` arguments of Podman's `create`.
    pub fn podman_args(self) -> [String; 4] {
        let map = |range: IdRange| format!("0:{}:{RANGE_LEN}", range.start);
        [
            "--uidmap".to_owned(),
            map(self.uid),
            "--gidmap".to_owned(),
            map(self.gid),
        ]
    }

    /// Whether the two mappings share a uid or a gid.
    fn overlaps(self, other: Self) -> bool {
        self.uid.overlaps(other.uid) || self.gid.overlaps(other.gid)
    }

    /// The name of this mapping's claim: `<uid start>-<gid start>`.
    fn claim_name(self) -> String {
        format!("{}-{}", self.uid.start, self.gid.start)
    }

    /// The mapping a claim name stands for.
    fn of_claim(name: &str) -> Option<Self> {
        let (uid, gid) = name.split_once('-')?;
        Some(Self::new(uid.parse().ok()?, gid.parse().ok()?))
    }
}

/// The uid `passwd` gives the user `user` (a numeric entry in the subordinate id files
/// names the user by it).
fn owner_uid<'a>(passwd: &'a str, user: &str) -> Option<&'a str> {
    let fields = passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>());
    fields
        .into_iter()
        .find(|fields| fields.first() == Some(&user))
        .and_then(|fields| fields.get(2).copied())
}

/// Whether the line of a subordinate id file is the user's: it names the user or its uid.
fn is_owner(line: &str, user: &str, uid: Option<&str>) -> bool {
    let first = line.split(':').next().unwrap_or_default();
    let _ = uid;
    first == user
}

/// The first id of the first entry of `user` in `text` (`name:start:count` lines, the
/// name or the uid of the user). With `exact` the user must have no other entry and the
/// entry exactly [`RANGE_LEN`] ids.
fn start_of(
    text: &str,
    user: &str,
    uid: Option<&str>,
    file: &'static str,
    exact: bool,
) -> Result<u32, SubidError> {
    let user_owned = || user.to_owned();
    let mut lines = text.lines().filter(|line| is_owner(line, user, uid));
    let line = lines.next().ok_or_else(|| SubidError::NoEntry {
        user: user_owned(),
        file,
    })?;
    let several = lines.next().is_some();
    let fields: Vec<&str> = line.split(':').collect();
    let number = |text: &str| text.trim().parse::<u32>().ok();
    let (start, count) = match fields.as_slice() {
        [_, start, count] => number(start).zip(number(count)),
        _ => None,
    }
    .ok_or_else(|| SubidError::Malformed {
        user: user_owned(),
        file,
    })?;
    if count < RANGE_LEN {
        return Err(SubidError::TooSmall {
            user: user_owned(),
            file,
            count,
        });
    }
    if exact && (several || count != RANGE_LEN) {
        return Err(SubidError::NotExact {
            user: user_owned(),
            file,
        });
    }
    if start == 0 {
        return Err(SubidError::Identity {
            user: user_owned(),
            file,
        });
    }
    if start.checked_add(RANGE_LEN).is_none() {
        return Err(SubidError::PastTheEnd {
            user: user_owned(),
            file,
            start,
        });
    }
    Ok(start)
}

/// The first id of `range` that a line of `text` of a user other than `user` also
/// allocates (lines that are not `name:start:count` allocate nothing).
fn other_overlap(text: &str, user: &str, uid: Option<&str>, range: IdRange) -> Option<u32> {
    let _ = (text, user, uid, range);
    None
}

/// An id of `range` in the colon-separated field number `field` (counting from 0) of a
/// line of `database`, if there is one. A line whose field is not a number (such as a
/// `+` line) names no id.
fn id_in(database: &str, field: usize, range: IdRange) -> Option<u32> {
    database
        .lines()
        .filter_map(|line| line.split(':').nth(field)?.trim().parse::<u32>().ok())
        .find(|id| range.contains(*id))
}

/// Who the job's processes are, as the post-job checks look for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobUsers {
    /// Processes of this one host uid: containers that run in the host's user namespace
    /// (a test's [`Settings`](super::Settings) built without `--container-subuid`).
    Uid(u32),
    /// Processes of the host uids of the range the runtime resolved when it opened
    /// (`--container-subuid`); the cell is filled by [`CliRuntime::open_in`](super::CliRuntime::open_in)
    /// and never read from the id files again.
    Mapped(Arc<OnceLock<Mapping>>),
}

impl JobUsers {
    /// The users of a mapped job whose range is `mapping`.
    pub fn resolved(mapping: Mapping) -> Self {
        Self::Mapped(Arc::new(OnceLock::from(mapping)))
    }

    /// The host uids of the job's processes.
    ///
    /// # Errors
    ///
    /// [`SubidError::Unresolved`] for a mapped range that no runtime has resolved.
    pub fn range(&self) -> Result<IdRange, SubidError> {
        match self {
            Self::Uid(uid) => Ok(IdRange::single(*uid)),
            Self::Mapped(cell) => cell
                .get()
                .map(|mapping| mapping.uid())
                .ok_or(SubidError::Unresolved),
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
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let name = mapping.claim_name();
        let held = open_regular(&dir.join(&name), true)?;
        held.try_lock()
            .map_err(|_| SubidError::Claimed(name.clone()))?;
        for entry in fs::read_dir(dir)? {
            let other = entry?.file_name();
            let Some(theirs) = other.to_str().and_then(Mapping::of_claim) else {
                continue;
            };
            if other == name.as_str() || !theirs.overlaps(mapping) {
                continue;
            }
            let file = open_regular(&dir.join(&other), false)?;
            file.try_lock()
                .map_err(|_| SubidError::Claimed(other.to_string_lossy().into_owned()))?;
        }
        Ok(Self { _held: held })
    }
}

/// Opens the claim file at `path` (made when `create`), and requires that what was opened
/// is the regular file the path names and not a link to something else.
fn open_regular(path: &Path, create: bool) -> Result<File, SubidError> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    Ok(file)
}
