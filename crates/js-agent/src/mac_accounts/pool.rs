//! The pool: its state, opening it, lending an account, retiring one, filling it.

use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::error::Failure;
use super::host::AccountHost;
use super::ledger::{self, Ledger, Record, State, name, parse_name, valid_job};
use super::settings::Settings;
use super::{Account, AccountError, Held, Lent, Retired, Step};

/// The accounts of the pool and what became of them. See the module documentation.
pub struct Pool {
    pub(super) host: Arc<dyn AccountHost>,
    pub(super) settings: Settings,
    /// The ledger file, for the errors that name it.
    pub(super) path: PathBuf,
    ledger: Mutex<Ledger>,
    /// Held while an account is made: accounts are made one at a time.
    maker: Mutex<()>,
    /// Run by a test before each record is taken (outside the ledger's lock).
    #[cfg(test)]
    pub(super) before_record: Mutex<Option<RecordHook>>,
}

/// What a test runs before a record is taken.
#[cfg(test)]
pub(super) type RecordHook = Box<dyn FnMut(&Record) + Send>;

/// The error for a ledger that could not record `why`.
pub(super) fn ledger_error(path: &Path, why: impl std::fmt::Display) -> AccountError {
    AccountError::Ledger {
        path: path.to_owned(),
        why: why.to_string(),
    }
}

/// Checks that `state` is a directory (not a link) that `owner` owns, when one is named,
/// and that neither the group nor others may write in: nobody else can replace the ledger.
fn check_state(state: &Path, owner: Option<u32>) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(state).map_err(|why| why.to_string())?;
    if !meta.is_dir() {
        return Err("not a directory".to_owned());
    }
    if let Some(owner) = owner.filter(|owner| *owner != meta.uid()) {
        return Err(format!("owned by uid {}, not uid {owner}", meta.uid()));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "mode {:o} lets others write in it",
            meta.mode() & 0o7777
        ));
    }
    Ok(())
}

impl Pool {
    /// Opens the pool of a Mac: asks whether Full Disk Access is held, opens the ledger in
    /// `state` (a directory root owns), sweeps what a crash left, and makes accounts until
    /// `ready` are ready.
    ///
    /// # Errors
    /// [`AccountError::NoFullDiskAccess`] (nothing is made or deleted), a state directory or
    /// ledger that is unusable, an account the sweep could not delete, or an account that
    /// could not be made.
    #[cfg(target_os = "macos")]
    pub fn open(state: &Path, ready: usize) -> Result<Self, AccountError> {
        Self::with_host(state, ready, Arc::new(super::macos::MacHost))
    }

    /// [`Pool::open`] over `host`, with a Mac's other settings.
    ///
    /// # Errors
    /// As [`Pool::open`].
    #[doc(hidden)]
    pub fn with_host(
        state: &Path,
        ready: usize,
        host: Arc<dyn AccountHost>,
    ) -> Result<Self, AccountError> {
        Self::with_settings(state, Settings::macos(ready), host)
    }

    /// [`Pool::open`] over `host` with `settings`: how the tests give their own paths and
    /// uids.
    ///
    /// # Errors
    /// As [`Pool::open`].
    #[doc(hidden)]
    pub fn with_settings(
        state: &Path,
        settings: Settings,
        host: Arc<dyn AccountHost>,
    ) -> Result<Self, AccountError> {
        match host.full_disk_access() {
            Ok(()) => {}
            Err(why) if why.kind() == io::ErrorKind::PermissionDenied => {
                return Err(AccountError::NoFullDiskAccess);
            }
            Err(source) => {
                return Err(AccountError::Host {
                    what: "asking whether Full Disk Access is held",
                    source,
                });
            }
        }
        check_state(state, settings.state_owner).map_err(|why| ledger_error(state, why))?;
        let path = state.join(ledger::FILE);
        let ledger = Ledger::open(&path).map_err(|why| ledger_error(&path, why))?;
        let pool = Self {
            host,
            settings,
            path,
            ledger: Mutex::new(ledger),
            maker: Mutex::new(()),
            #[cfg(test)]
            before_record: Mutex::new(None),
        };
        pool.sweep()?;
        pool.refill()?;
        Ok(pool)
    }

    /// The ledger, locked: for the length of one record, never across a call to the
    /// operating system.
    pub(super) fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `record`, durably.
    pub(super) fn record(&self, record: &Record) -> io::Result<()> {
        #[cfg(test)]
        if let Some(hook) = self
            .before_record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            hook(record);
        }
        self.ledger().append(record)
    }

    /// The accounts are made one at a time: held for the making of one.
    pub(super) fn maker(&self) -> MutexGuard<'_, ()> {
        self.maker.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The account number `n`, as a caller holds it.
    pub(super) fn account(&self, n: u64, uid: u32) -> Account {
        Account {
            name: name(n),
            uid,
            gid: self.settings.gid,
        }
    }

    /// Lends a ready account to the job `job` (one to 63 of `a-z`, `0-9` and `-`): the
    /// oldest ready one, recorded `lent` before it is returned, so an account is lent once,
    /// ever. With none ready, one is made on the spot (`made_now`).
    ///
    /// # Errors
    /// [`AccountError::BadJob`] (nothing is recorded), a ledger that could not record the
    /// loan, or an account that could not be made or found a uid.
    pub fn lend(&self, job: &str) -> Result<Lent, AccountError> {
        if !valid_job(job) {
            return Err(AccountError::BadJob(job.to_owned()));
        }
        if let Some(account) = self.take_ready(job)? {
            return Ok(Lent {
                account,
                made_now: false,
            });
        }
        let _maker = self.maker();
        self.lend_or_make(job)
    }

    /// With the maker held: lends the oldest ready account (one may have been made while
    /// this waited for the maker), or makes one.
    pub(super) fn lend_or_make(&self, job: &str) -> Result<Lent, AccountError> {
        match self.take_ready(job)? {
            Some(account) => Ok(Lent {
                account,
                made_now: false,
            }),
            None => Ok(Lent {
                account: self.make(Some(job))?,
                made_now: true,
            }),
        }
    }

    /// The oldest ready account, recorded `lent` to `job`.
    fn take_ready(&self, job: &str) -> Result<Option<Account>, AccountError> {
        let mut ledger = self.ledger();
        let Some((n, entry)) = ledger.first_in(State::Ready) else {
            return Ok(None);
        };
        ledger
            .append(&Record::Lent(n, job.to_owned()))
            .map_err(|why| ledger_error(&self.path, why))?;
        Ok(Some(self.account(n, entry.uid)))
    }

    /// Ends `account` after its job: kills everything of its uid, deletes it and shows it
    /// gone (the steps are in the module documentation), records it `gone`, then fills the
    /// pool again. An account that is held is tried again the same way. A refill that fails
    /// is logged and is no failure of the retirement.
    ///
    /// # Errors
    /// [`Held`]: the account is not lent (or held) in the ledger, or a step failed (the
    /// ledger then records it `held` at that step, and its uid is chosen by no one).
    pub fn retire(&self, account: Account) -> Result<Retired, Held> {
        let held = |step, why: String| Held {
            account: account.clone(),
            step,
            why,
        };
        let lent = parse_name(&account.name)
            .and_then(|n| self.ledger().get(n).map(|entry| (n, entry)))
            .filter(|(_, entry)| {
                matches!(entry.state, State::Lent | State::Held) && entry.uid == account.uid
            });
        let Some((n, _)) = lent else {
            return Err(held(Step::Ledger, "not lent".to_owned()));
        };
        let start = self.host.now_ms();
        let mut killed = Vec::new();
        let done = self.teardown(&account.name, account.uid, &mut killed);
        let gone = self
            .settle(n, done)
            .map_err(|f: Failure| held(f.step, f.why))?;
        let deleted = self.host.now_ms();
        if let Err(why) = self.refill() {
            tracing::warn!(
                "filling the pool of accounts after retiring {}: {why}",
                account.name
            );
        }
        killed.sort_unstable();
        killed.dedup();
        Ok(Retired {
            killed,
            gone,
            delete_ms: deleted.saturating_sub(start),
            refill_ms: self.host.now_ms().saturating_sub(deleted),
        })
    }

    /// Makes accounts until `ready` are ready; returns how many were made.
    ///
    /// # Errors
    /// An account could not be made (it is deleted again where that can be shown, and
    /// otherwise held).
    pub fn refill(&self) -> Result<usize, AccountError> {
        let mut made = 0;
        loop {
            let _maker = self.maker();
            if self.ready_count() >= self.settings.ready {
                return Ok(made);
            }
            self.make(None)?;
            made += 1;
        }
    }

    /// The accounts whose deletion did not complete: their uids are chosen by no one until
    /// [`Pool::retire`] shows them gone.
    #[must_use]
    pub fn held(&self) -> Vec<Account> {
        self.ledger()
            .in_state(State::Held)
            .into_iter()
            .map(|(n, entry)| self.account(n, entry.uid))
            .collect()
    }

    /// How many accounts are ready to be lent.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.ledger().in_state(State::Ready).len()
    }
}

#[cfg(test)]
mod tests;
