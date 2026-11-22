//! The accounts native Mac jobs run as: one account per job, never used twice, deleted and
//! shown gone afterwards, inside `js-agent`.
//!
//! A native job on a Mac runs as an account that has never run a job. The [`Pool`] keeps a
//! few such accounts ready, made ahead of time; [`Pool::lend`] hands one to a job, and
//! [`Pool::retire`] ends everything of it after the job, deletes it and shows it gone,
//! then [`Pool::refill`] makes a new one. A uid is not used again until its account is
//! shown gone. The module is functions in `js-agent`: no helper program and no socket.
//! Everything it asks of the operating system goes through [`AccountHost`]; `macos::MacHost`
//! is the macOS implementation (OpenDirectory for the records, `libproc` and `kill` for the
//! processes, `launchctl` for the domains), and the tests run the same code over a fake.
//! What the caller does itself: the job's folder, the sandbox profile, starting the job as
//! the lent uid, its outputs.
//!
//! # The account
//!
//! Named `kbf-acct-<n>` (the next number the ledger has not used, at most 31 bytes), with a
//! uid of the range [`UID_RANGE`] (the first free uid after the one given last, wrapping),
//! hidden, shell `/usr/bin/false`, no password and so no login and no secure token, the
//! primary group `staff` and no other, never an administrator ([`account_attributes`]). Its
//! home is `/Users/<name>`, mode 0700, made before the account's record and refused if the
//! path exists.
//!
//! A uid is free when it is in the range; no account of the ledger that is not gone has it;
//! no record has it; no live process runs as it; nothing is owned by it two levels down in
//! `/private/var/folders`; and nothing is owned by it in the shared places (`/Users/Shared`,
//! `/private/tmp`, `/private/var/tmp`, `/private/var/at/jobs`, launchd's per-uid files). An
//! error in looking is not "free": the uid is passed over and the refusal names why.
//!
//! # The life cycle
//!
//! 1. **Made.** The ledger records `made <name> <uid>` before anything exists; then the
//!    home, the record, and `ready`. A failure after `made` deletes what was made as in
//!    steps 3 to 6 below, and records `gone` when that is shown, `held` when it is not.
//! 2. **Lent.** [`Pool::lend`] takes the oldest ready account and records `lent` before it
//!    returns: an account is lent once, ever. With none ready it makes one on the spot.
//! 3. **Retired.** After the job, [`Pool::retire`]:
//!    1. kills every process of the uid and boots out its launchd domains, until none is
//!       left (judged by outcome, not by what a call answered); removes its crontab and `at`
//!       jobs; kills once more;
//!    2. removes its System V IPC objects;
//!    3. removes what it owns in the shared places and launchd's per-uid files;
//!    4. deletes the record, the home, and every directory it owns in the per-user folders,
//!       whole;
//!    5. proves it gone: no record by name or uid, no process and no domain, no home, no
//!       per-user folder, nothing in the shared places, no IPC object, each looked at again
//!       after the deletion (an item that cannot be looked at does not hold);
//!    6. only then records `gone`; the uid is free from then on.
//!
//!    Then the pool is filled again; a refill that fails is logged and does not fail the
//!    retirement of an account that was deleted.
//!
//! # What is held
//!
//! A step that fails leaves the account **held**: the ledger records `held` with the step
//! ([`Step`]), the uid is chosen by no one, [`Pool::held`] lists the account and
//! [`Pool::retire`] on it tries every step again. [`Pool::retire`] returns the failure as a
//! [`Held`]. Nothing held is ever used silently again.
//!
//! # Crash points
//!
//! The ledger (`<state>/accounts`, see `ledger`) is written before each action and
//! `fsync`ed. [`Pool::open`] replays it and sweeps: an account `made` and not `ready`, a
//! `lent` one (its job is over with the process), and a `held` one are deleted as above and
//! recorded `gone` or `held`; a `ready` one is kept if its record and home are still there,
//! deleted otherwise; a record named `kbf-acct-*` in the range that the ledger does not know
//! (a lost ledger) is deleted by name, and if that fails the open fails naming it. Then the
//! pool is filled to `ready`. Without Full Disk Access nothing is made or deleted:
//! [`AccountError::NoFullDiskAccess`].
//!
//! # Concurrency
//!
//! A [`Pool`] is shared by reference between threads. It locks only its ledger, for the
//! length of a record, and never across a call to the operating system; accounts are made
//! one at a time; a deletion holds no lock while it runs, so a job can be lent an account
//! while another's is being deleted.

mod error;
mod home;
mod host;
mod ipc;
mod launchd;
mod ledger;
#[cfg(target_os = "macos")]
mod macos;
mod make;
mod pool;
mod proof;
mod settings;
mod sweep;
mod teardown;
#[cfg(test)]
mod tests;
mod tree;
mod uid;

pub use error::{AccountError, Gone, Held, Step};
pub use home::{SHELL, account_attributes, make_home};
pub use host::{AccountHost, CommandOutput, LiveProcess, NewUser, PrintAnswer, UserRecord};
#[cfg(target_os = "macos")]
pub use macos::MacHost;
pub use pool::Pool;
pub use settings::{Places, STAFF, Settings, UID_RANGE};

/// The prefix of every account's name; nothing outside it is ever made or deleted.
pub const PREFIX: &str = "kbf-acct-";

/// The longest account name macOS's directory service takes.
pub const MAX_NAME: usize = 31;

/// How many rounds of kill and bootout end the processes and domains of a uid before the
/// account is held.
pub const KILL_ROUNDS: usize = 10;

/// An account lent to a job. Never an administrator, and never has run a job before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// Its name, `kbf-acct-<n>`.
    pub name: String,
    /// Its uid.
    pub uid: u32,
    /// Its primary group.
    pub gid: u32,
}

/// What [`Pool::lend`] hands out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lent {
    /// The account.
    pub account: Account,
    /// Whether none was ready and the account was made for this job, on the spot.
    pub made_now: bool,
}

/// What was looked at after an account's deletion and found gone, one field per item of
/// the proof ([`Gone`] names the items in the order they are looked at). A [`Proof`] is
/// returned only for an account that was shown gone, so every field holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proof {
    /// No user record by the account's name and none with its uid.
    pub record: bool,
    /// No live process of the uid, and its launchd domains read as gone.
    pub processes: bool,
    /// The home folder's path does not exist.
    pub home: bool,
    /// No directory two levels down in the per-user folders is owned by the uid.
    pub folders: bool,
    /// Nothing owned by the uid, or named for the account, is left in the shared places.
    pub places: bool,
    /// A fresh listing of System V IPC objects shows none of the uid's.
    pub ipc: bool,
}

impl Proof {
    /// The proof of an account every item of which was looked at and held.
    pub(super) const fn shown() -> Self {
        Self {
            record: true,
            processes: true,
            home: true,
            folders: true,
            places: true,
            ipc: true,
        }
    }
}

/// What [`Pool::retire`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retired {
    /// The pids of the processes that were alive when the kill rounds signalled the uid's
    /// processes, each once, in order.
    pub killed: Vec<i32>,
    /// What was shown gone.
    pub gone: Proof,
    /// Milliseconds from the start of the kill to `gone` recorded, by the host's clock.
    pub delete_ms: u64,
    /// Milliseconds the refill after it took, by the host's clock.
    pub refill_ms: u64,
}
