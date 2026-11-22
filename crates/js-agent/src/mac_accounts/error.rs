//! The errors of the module: the step an account's deletion or making stopped at
//! ([`Step`], [`Gone`]), the account a deletion left ([`Held`]) and everything else that
//! can refuse ([`AccountError`]).

use std::fmt;
use std::path::PathBuf;

use super::Account;

/// The items of the proof that a deleted account is gone, in the order they are looked at.
/// The first that does not hold, or cannot be looked at, fails the deletion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gone {
    /// No user record by the account's name, and none with its uid.
    Record,
    /// No live process of the uid, and its launchd domains read as gone.
    Processes,
    /// The home folder's path does not exist (`lstat` answers "no such file or directory").
    Home,
    /// Every per-user folder the uid owns is emptied: nothing but directories at its top
    /// level, and nothing in them.
    Folders,
    /// Nothing owned by the uid, or named for the account, is left in the shared places.
    Places,
    /// A fresh listing of System V IPC objects shows none of the uid's.
    Ipc,
}

impl fmt::Display for Gone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Record => "record",
            Self::Processes => "processes",
            Self::Home => "home",
            Self::Folders => "folders",
            Self::Places => "places",
            Self::Ipc => "ipc",
        })
    }
}

/// The step of making or deleting an account that failed. Its `Display` is the kebab-case
/// word the ledger's `held` record keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Ending the processes of the uid: a process could not be listed or signalled, or one
    /// is left after the last round.
    Kill,
    /// The uid's launchd domains still print after the last round, or could not be asked.
    Domains,
    /// The crontab or an `at` job of the account could not be removed.
    Schedules,
    /// The uid's System V IPC objects could not be listed or removed.
    Ipc,
    /// Something of the uid in the shared places could not be removed.
    Sweep,
    /// The user record could not be made, looked up or deleted.
    Record,
    /// The home folder could not be made or removed.
    Home,
    /// A per-user folder of the uid could not be found, looked at or emptied.
    Folder,
    /// An item of the proof did not hold.
    Proof(Gone),
    /// The ledger could not record what happened, or does not allow it.
    Ledger,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kill => f.write_str("kill"),
            Self::Domains => f.write_str("domains"),
            Self::Schedules => f.write_str("schedules"),
            Self::Ipc => f.write_str("ipc"),
            Self::Sweep => f.write_str("sweep"),
            Self::Record => f.write_str("record"),
            Self::Home => f.write_str("home"),
            Self::Folder => f.write_str("folder"),
            Self::Proof(item) => write!(f, "proof-{item}"),
            Self::Ledger => f.write_str("ledger"),
        }
    }
}

/// A step that failed, and why: what the steps of a deletion return before the pool knows
/// which account it was.
#[derive(Debug)]
pub(super) struct Failure {
    pub(super) step: Step,
    pub(super) why: String,
}

impl Failure {
    pub(super) fn new(step: Step, why: impl fmt::Display) -> Self {
        Self {
            step,
            why: why.to_string(),
        }
    }
}

/// An account whose deletion did not complete: its uid stays out of use, the ledger holds
/// it as `held`, [`super::Pool::held`] lists it and [`super::Pool::retire`] tries it again.
#[derive(Debug, thiserror::Error)]
#[error("account {} (uid {}) is held at {step}: {why}", .account.name, .account.uid)]
pub struct Held {
    /// The account.
    pub account: Account,
    /// The step that failed.
    pub step: Step,
    /// Why, as the step words it.
    pub why: String,
}

/// Why the pool refused to open, lend or refill.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// The program does not hold Full Disk Access, without which macOS refuses root the
    /// deletion of an account, its home and its per-user folders: nothing was made.
    #[error(
        "js-agent does not hold Full Disk Access, so it cannot delete accounts: install the \
         privacy profile packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig \
         through device management and restart the agent"
    )]
    NoFullDiskAccess,
    /// The state directory or the ledger in it is unusable (not a directory, not root's,
    /// open to others, unreadable, a line that does not parse or does not follow, a write
    /// that failed).
    #[error("the account ledger in {}: {why}", .path.display())]
    Ledger {
        /// The state directory or the ledger file.
        path: PathBuf,
        /// What is wrong.
        why: String,
    },
    /// An account could not be made, or a leftover account could not be deleted at open.
    /// What was made is deleted again and shown gone where that can be, and `why` says
    /// which.
    #[error("making account {account}: {step}: {why}")]
    Make {
        /// The account's name.
        account: String,
        /// The step that failed.
        step: Step,
        /// Why.
        why: String,
    },
    /// No uid of the range is free.
    #[error("no free uid in {range}: {}", .reasons.join("; "))]
    NoFreeUid {
        /// The range, as `<first>-<last>`.
        range: String,
        /// Why each uid is not free, for the first few, then how many more.
        reasons: Vec<String>,
    },
    /// The uid range the agent was given is not one slots may take.
    #[error("the account uid range: {0}")]
    BadSlots(String),
    /// The job id is not one to 63 of `a-z`, `0-9` and `-`.
    #[error("job id {0:?} is not 1 to 63 characters of a-z, 0-9 and -")]
    BadJob(String),
    /// The host refused a question the pool had to ask.
    #[error("{what}: {source}")]
    Host {
        /// What was asked.
        what: &'static str,
        /// The host's answer.
        source: std::io::Error,
    },
}
