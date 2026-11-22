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

    /// The record named `name`, `None` when there is none.
    ///
    /// # Errors
    /// The lookup failed.
    fn account(&self, name: &str) -> io::Result<Option<UserRecord>>;

    /// Whether a record has the uid `uid`.
    ///
    /// # Errors
    /// The lookup failed.
    fn uid_taken(&self, uid: u32) -> io::Result<bool>;

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

    /// Makes the user record `user`: hidden, shell `/usr/bin/false`, no password, the
    /// primary group `user.gid` and no other, never an administrator
    /// ([`super::account_attributes`]).
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

    /// Waits a moment, between two looks at a uid that has not yet gone quiet.
    fn pause(&self);

    /// Milliseconds on a clock that only goes forward; only differences mean anything.
    fn now_ms(&self) -> u64;
}
