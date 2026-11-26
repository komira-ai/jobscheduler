//! Every call the pool makes to the operating system that is not a file operation:
//! [`AccountHost`]. The macOS implementation is `macos::MacHost`; the tests use a fake that
//! keeps users, processes, launchd domains and IPC objects in memory.

use std::io;
use std::path::{Path, PathBuf};

/// A user record as the system's lookups answer it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserRecord {
    /// The uid.
    pub uid: u32,
    /// The primary group.
    pub gid: u32,
}

/// A user record to make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewUser {
    /// The account name.
    pub name: String,
    /// The uid.
    pub uid: u32,
    /// The primary group, the only group.
    pub gid: u32,
    /// The home folder.
    pub home: PathBuf,
    /// The record's `GeneratedUID`, which the record is made with ([`super::generated_uid`]):
    /// the slot's, the same for the same uid whatever the account's name.
    pub generated_uid: String,
}

/// A live process of a uid, as a refusal names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveProcess {
    /// Its pid.
    pub pid: i32,
    /// Its command name as the kernel keeps it, or `?` when it could not be read.
    pub command: String,
}

/// What `launchctl print <domain>` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrintAnswer {
    /// Its exit status; `None` when a signal ended it.
    pub code: Option<i32>,
    /// What it printed.
    pub stdout: String,
    /// What it printed to its error stream.
    pub stderr: String,
}

/// What a program the pool runs did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether it exited with status 0.
    pub success: bool,
    /// What it printed.
    pub stdout: String,
    /// What it printed to its error stream.
    pub stderr: String,
}

/// The operating system, as far as the pool asks it anything but about files.
///
/// Calls that fail return the system's error; the pool judges each by its outcome and
/// names the step it failed in. A call may be made from several threads at once (a job's
/// account is deleted while another is lent).
pub trait AccountHost: Send + Sync {
    /// Asks whether the program holds Full Disk Access: `Ok` when the directory of the
    /// privacy database could be listed, [`io::ErrorKind::PermissionDenied`] when macOS
    /// refused, any other error when it could not tell.
    ///
    /// # Errors
    /// The refusal or the failure above.
    fn full_disk_access(&self) -> io::Result<()>;

    /// The record named `name`, `None` when there is none, as every lookup of the system
    /// answers it together (on macOS the local directory node and `getpwnam`, which may
    /// answer from a cache for a while after a record was deleted): present when any says so.
    ///
    /// # Errors
    /// A lookup failed.
    fn account(&self, name: &str) -> io::Result<Option<UserRecord>>;

    /// Whether a record has the uid `uid`, as every lookup of the system answers it together
    /// (on macOS the local directory node and `getpwuid`): taken when any says so.
    ///
    /// # Errors
    /// A lookup failed.
    fn uid_taken(&self, uid: u32) -> io::Result<bool>;

    /// Whether the directory node the pool makes and deletes its records in (on macOS
    /// OpenDirectory's local node) holds a record named `name`: that node's own answer, never
    /// a cache's. It is what decides whether a deleted record is gone.
    ///
    /// # Errors
    /// The directory could not be queried.
    fn in_directory(&self, name: &str) -> io::Result<bool>;

    /// Whether the directory node of [`AccountHost::in_directory`] holds a record with the uid
    /// `uid`.
    ///
    /// # Errors
    /// The directory could not be queried.
    fn uid_in_directory(&self, uid: u32) -> io::Result<bool>;

    /// Whether a record has the `GeneratedUID` `guid`.
    ///
    /// # Errors
    /// The lookup failed.
    fn guid_taken(&self, guid: &str) -> io::Result<bool>;

    /// The `GeneratedUID` of the record named `name`, `None` when there is none.
    ///
    /// # Errors
    /// The lookup failed, or the record has no `GeneratedUID`.
    fn record_guid(&self, name: &str) -> io::Result<Option<String>>;

    /// The records whose name starts with `prefix`, each with its uid; a record whose uid
    /// cannot be read is passed over (and logged), not an error.
    ///
    /// # Errors
    /// The directory could not be queried.
    fn list_accounts(&self, prefix: &str) -> io::Result<Vec<(String, u32)>>;

    /// Makes the home folder `name` in `homes`, owned by `uid` and `gid`, mode 0700. A
    /// folder that exists is an error: a new account takes nothing of what was there.
    ///
    /// # Errors
    /// The folder exists, or could not be made or given away.
    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()>;

    /// Makes the user record `user`: made with its `GeneratedUID` (never set afterwards: it
    /// cannot be changed), hidden, shell `/usr/bin/false`, no password, the primary group
    /// `user.gid` and no other, never an administrator ([`super::creation_attributes`],
    /// [`super::account_attributes`]).
    ///
    /// # Errors
    /// The record could not be made.
    fn create_account(&self, user: &NewUser) -> io::Result<()>;

    /// Deletes the record `name`, and its membership of the `admin` group; returns whether
    /// a record was there. A missing record is not an error.
    ///
    /// # Errors
    /// The deletion was refused or failed.
    fn delete_account(&self, name: &str) -> io::Result<bool>;

    /// The live processes whose effective or real uid is `uid`; an exited process that
    /// waits to be reaped is not live.
    ///
    /// # Errors
    /// The process table could not be read.
    fn processes(&self, uid: u32) -> io::Result<Vec<LiveProcess>>;

    /// Sends SIGKILL to every process of `uid`, in one call the uid's processes cannot
    /// outrun by forking.
    ///
    /// # Errors
    /// The signal could not be sent.
    fn kill_all(&self, uid: u32) -> io::Result<()>;

    /// `launchctl print <domain>`, as it answered.
    ///
    /// # Errors
    /// `launchctl` could not be run.
    fn launchctl_print(&self, domain: &str) -> io::Result<PrintAnswer>;

    /// `launchctl bootout <domain>`. Its failure decides nothing (it fails for a domain
    /// that is not there): the pool looks at the outcome.
    ///
    /// # Errors
    /// `launchctl` could not be run or said no.
    fn bootout(&self, domain: &str) -> io::Result<()>;

    /// Runs the program `program` with `args` (`ipcs`, `ipcrm`) and collects what it said.
    ///
    /// # Errors
    /// The program could not be run.
    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;

    /// Waits a moment: between two looks at a uid that has not yet gone quiet, and between
    /// two asks to delete a record the directory refused.
    fn pause(&self);

    /// Milliseconds on a clock that only goes forward; only differences mean anything.
    fn now_ms(&self) -> u64;
}

/// Whether a record is present, from two sources that each answer present, absent or an
/// error: absent only when both say absent, an error when either fails (a failed lookup is
/// not an absence), present otherwise. The macOS host asks OpenDirectory, which reports
/// errors, and `getpw*`, which answers absent for a failed lookup too.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn present_in_any(
    directory: io::Result<bool>,
    getpw: io::Result<bool>,
) -> io::Result<bool> {
    match (directory, getpw) {
        (Err(why), _) | (_, Err(why)) => Err(why),
        (Ok(directory), Ok(getpw)) => Ok(directory || getpw),
    }
}

/// The record `getpw` found, judged with what the directory said ([`present_in_any`]):
/// `None` only when both say absent. A record the directory has and `getpw` does not show
/// is an error, since there is nothing to return and the record is not absent.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn merge_record<T>(
    directory: io::Result<bool>,
    getpw: io::Result<Option<T>>,
) -> io::Result<Option<T>> {
    let seen = getpw
        .as_ref()
        .map(Option::is_some)
        .map_err(|why| io::Error::new(why.kind(), why.to_string()));
    let present = present_in_any(directory, seen)?;
    match getpw? {
        Some(record) => Ok(Some(record)),
        None if present => Err(io::Error::other(
            "the directory has the record and getpw does not show it",
        )),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err() -> io::Result<bool> {
        Err(io::Error::other("lookup failed"))
    }

    /// Asserts that a record is absent only when every source says absent, and that an error
    /// from either source is an error, never an absence, whatever the other says.
    #[test]
    fn a_record_is_absent_only_when_every_source_says_so() {
        assert!(!present_in_any(Ok(false), Ok(false)).unwrap());
        assert!(present_in_any(Ok(true), Ok(false)).unwrap());
        assert!(present_in_any(Ok(false), Ok(true)).unwrap());
        assert!(present_in_any(Ok(true), Ok(true)).unwrap());
        for (directory, getpw) in [
            (err(), Ok(false)),
            (Ok(false), err()),
            (err(), Ok(true)),
            (Ok(true), err()),
            (err(), err()),
        ] {
            assert_eq!(
                present_in_any(directory, getpw).unwrap_err().to_string(),
                "lookup failed"
            );
        }
    }

    /// Asserts that a found record is returned, that none is returned only when both sources
    /// say absent, that an error of either source is an error, and that a record only the
    /// directory has is an error and not an absence.
    #[test]
    fn a_record_is_none_only_when_the_directory_and_getpw_agree() {
        assert_eq!(merge_record(Ok(true), Ok(Some(7_u8))).unwrap(), Some(7));
        assert_eq!(merge_record(Ok(false), Ok(Some(7_u8))).unwrap(), Some(7));
        assert_eq!(merge_record::<u8>(Ok(false), Ok(None)).unwrap(), None);
        assert!(merge_record::<u8>(Ok(true), Ok(None)).is_err());
        assert!(merge_record::<u8>(err(), Ok(None)).is_err());
        assert!(merge_record(err(), Ok(Some(7_u8))).is_err());
        assert!(merge_record::<u8>(Ok(false), Err(io::Error::other("getpw"))).is_err());
        assert!(merge_record::<u8>(Ok(true), Err(io::Error::other("getpw"))).is_err());
    }
}
