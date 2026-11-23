//! The accounts native Mac jobs run as: one account per job, never used twice, deleted and
//! shown gone afterwards, inside `js-agent`.
//!
//! A native job on a Mac runs as an account that has never run a job. The [`Pool`] keeps a
//! few such accounts ready, made ahead of time; [`Pool::lend`] hands one to a job, and
//! [`Pool::retire`] ends everything of it after the job, deletes it and shows it gone,
//! then [`Pool::refill`] makes a new one. A uid is not used again until its account is
//! shown gone. The module is functions in `js-agent`: no helper program and no socket.
//! Everything it asks of the operating system goes through [`AccountHost`] (and, for the
//! removal of what is in a per-user folder, the file calls of `tree::Os`); `macos::MacHost`
//! is the macOS implementation (OpenDirectory for the records, `libproc` and `kill` for the
//! processes, `launchctl` for the domains), and the tests run the same code over a fake.
//! What the caller does itself: the job's folder, the sandbox profile, starting the job as
//! the lent uid, its outputs. [`cycle`] is a development verb that runs the life cycle once
//! on a Mac, to prove it.
//!
//! # The slot
//!
//! A uid of the range the agent is given ([`Slots`], a setting with no default) is a *slot*: a fixed pair of the uid and a `GeneratedUID`
//! ([`generated_uid`]: a fixed prefix and the uid, so the same slot always gets the same
//! identifier, and a lost ledger cannot lose it). For each job the module makes a *fresh*
//! record (a new name, never used again) and a fresh home for a free slot; after the job it
//! kills the uid, deletes the record and the home, and *empties* the slot's per-user folder.
//! The folder stays, one per slot, for good.
//!
//! Why it stays: macOS gives a user a per-user folder, `/private/var/folders/<xx>/<hash>`
//! with directories `0`, `T`, `C` and `X`, found by the record's uid and `GeneratedUID`. With
//! System Integrity Protection on, the folder and its four directories carry `sunlnk` and the
//! `com.apple.rootless` attribute: `rmdir`, `chflags` and `xattr -d` are refused to root, and
//! `sysadminctl -deleteUser` leaves the folder too. What is below the four directories can be
//! removed, except what macOS's own per-user daemons seal there: after a real build the
//! folder held Launch Services' data vault (`0/com.apple.LaunchServices.dv`) and the trust
//! daemon's folder (`T/com.apple.trustd`), which root may not remove, list or even examine.
//! Those two, when sealed, are kept with the folder (`tree::kept::SYSTEM_KEPT`): a job cannot open
//! them, only the daemon can. A record made with its `GeneratedUID` from the start (the directory service takes
//! it only at creation; root may not change it afterwards) and the same uid gets the same
//! folder again, found empty once the last account's contents were removed. So the module
//! makes the record with the slot's `GeneratedUID` among its first attributes
//! ([`creation_attributes`]), reads it back, and fails the making if it is not the slot's.
//!
//! # The account
//!
//! Named `kbf-acct-<n>` (the next number the ledger has not used, at most 31 bytes), with a
//! uid of the range (the first free slot after the one given last, wrapping), hidden, shell
//! `/usr/bin/false`, no password and so no login and no secure token, the primary group
//! `staff` and no other, never an administrator ([`account_attributes`]). Its home is
//! `/Users/<name>`, mode 0700, made before the account's record and refused if the path
//! exists.
//!
//! A slot is free when it is in the range; no account of the ledger that is not gone has the
//! uid; no record has the uid and none has the slot's `GeneratedUID`; no live process runs as
//! the uid once the kill rounds of a retirement have ended any that did (with no ledger
//! account and no record, such a process is nobody's work: macOS starts per-user daemons such
//! as `distnoted` by itself; one that survives the rounds keeps the slot busy, named by its
//! pid); every per-user folder the uid owns two levels down in `/private/var/folders` holds
//! nothing below its top-level directories but the sealed system items (and nothing but
//! directories at its top level);
//! and nothing is owned by the uid in the shared places (`/Users/Shared`, `/private/tmp`,
//! `/private/var/tmp`, `/private/var/at/jobs`, launchd's per-uid files). An error in looking
//! is not "free": the slot is passed over and the refusal names why.
//!
//! # The life cycle
//!
//! 1. **Made.** The ledger records `made <name> <uid>` before anything exists; then the slot's
//!    per-user folders are emptied again (a crash may have left something in them), and the
//!    home, the record and `ready` follow. A failure after `made` ends the uid's processes as
//!    in step 1 below and deletes what was made as in steps 3 to 6, and records `gone` when
//!    that is shown, `held` when it is not.
//! 2. **Lent.** [`Pool::lend`] takes the oldest ready account and records `lent` before it
//!    returns: an account is lent once, ever. With none ready it makes one on the spot,
//!    recorded `made` and then `lent`
//!    (never `ready`, which another lend could take).
//! 3. **Retired.** After the job, [`Pool::retire`]:
//!    1. kills every process of the uid and boots out its launchd domains, until no process
//!       is left and its login domain does not print (judged by outcome, not by what a call
//!       answered); removes its crontab and `at` jobs; kills once more. launchd is asked only
//!       while the account's record is there, and its user domain is never printed: a print
//!       of `user/<uid>` makes the domain again, and launchd then keeps trying to start its
//!       per-user services for a uid with no account. Without the record the rounds judge by
//!       the processes alone;
//!    2. removes its System V IPC objects;
//!    3. removes what it owns in the shared places and launchd's per-uid files;
//!    4. deletes the record and the home, and empties every per-user folder the uid owns
//!       (an older account of the same uid may have left another besides the slot's): each
//!       folder and its top-level directories stay, everything below them goes (except the
//!       sealed system items above) and so does any non-directory at the top level. The removal is root's way: no link is followed
//!       (a link is removed, its target kept), no other filesystem is entered, user flags
//!       (`uchg`, `uappnd`) are cleared on every entry before it is removed, an entry whose
//!       removal is refused has its ACL stripped and is removed again, a directory is made
//!       searchable before it is entered, and a tree of any depth is moved up and removed in
//!       passes;
//!    5. proves it gone: no record by name or uid in the directory node it was deleted from
//!       (a lookup through the system's cache that fails also fails the proof, one that
//!       still answers for the record does not), no process (launchd is not asked after
//!       the deletion), no home, every
//!       per-user folder of the uid emptied (the folders being there is no fault), nothing in
//!       the shared places, no IPC object, each looked at again after the deletion (an item
//!       that cannot be looked at does not hold);
//!    6. only then records `gone`; the slot is free from then on.
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
//! `fsync`ed. [`Pool::open`] is given the jobs the restarted agent adopted (a job outlives a
//! restart of the agent under its supervisor) and replays the ledger and sweeps: an account
//! `lent` to one of those jobs is kept as it is, its uid still held, for [`Pool::retire`] to
//! end when the job is over (unless its record or its home is gone, when it is deleted); an
//! account `made` and not `ready`, one `lent` to any other job (that job is over), and a
//! `held` one are deleted as above and recorded `gone` or `held`; a `ready` one is kept if its
//! record and home are still there, deleted otherwise; a record named `kbf-acct-*` in the range that the ledger does not know
//! (a lost ledger) is deleted by name, and if that fails the open fails naming it. Then the
//! per-user folders of every uid of the range that no live account of the ledger holds are
//! emptied (a crash between an account's deletion and the emptying of its folders leaves
//! something in one); a folder that cannot be emptied is logged, and its slot is not free
//! until it is. Then the pool is filled to `ready`. Without Full Disk Access nothing is made
//! or deleted: [`AccountError::NoFullDiskAccess`].
//!
//! # Concurrency
//!
//! A [`Pool`] is shared by reference between threads. It locks only its ledger, for the
//! length of a record, and never across a call to the operating system; accounts are made
//! one at a time; a deletion holds no lock while it runs, so a job can be lent an account
//! while another's is being deleted.

pub mod cycle;
mod error;
mod folder;
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
pub mod user_dirs;

pub use error::{AccountError, Gone, Held, Step};
pub use home::{SHELL, account_attributes, creation_attributes, generated_uid, make_home};
pub use host::{AccountHost, CommandOutput, LiveProcess, NewUser, PrintAnswer, UserRecord};
#[cfg(target_os = "macos")]
pub use macos::{MacHost, own_folders};
pub use pool::Pool;
pub use settings::{Places, STAFF, Settings, Slots};

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
    /// The `GeneratedUID` of its record ([`generated_uid`]): the slot's, fixed by the uid.
    pub generated_uid: String,
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
    /// No live process of the uid after the deletion (its launchd domains were judged by the
    /// kill rounds, while its record was there).
    pub processes: bool,
    /// The home folder's path does not exist.
    pub home: bool,
    /// Every per-user folder the uid owns is emptied: nothing but directories at its top
    /// level, and nothing below its top-level directories. The folders themselves stay.
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
