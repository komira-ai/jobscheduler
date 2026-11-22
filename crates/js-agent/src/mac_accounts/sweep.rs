//! What [`Pool::open`] does about what a crash, a restart or a lost ledger left: replays the
//! ledger, keeps the accounts of the jobs the agent adopted, and ends every other account it
//! does not show gone; then looks for records of the pool that the ledger does not know; then
//! empties the per-user folders of the slots no account holds.

use std::collections::BTreeSet;
use std::path::Path;

use super::ledger::{State, name, parse_name};
use super::pool::{Pool, ledger_error};
use super::{AccountError, PREFIX, Step, folder};

/// Whether the account `name` has its record with `uid` and its home folder in `homes`:
/// `Ok(true)` when both are there as they were made, `Ok(false)` when either is missing or
/// is not what was made, and `Err` naming what could not be looked at (which says neither).
fn intact(pool: &Pool, name: &str, uid: u32, homes: &Path) -> Result<bool, String> {
    let record = match pool.host.account(name) {
        Ok(record) => record.is_some_and(|found| found.uid == uid),
        Err(why) => return Err(format!("looking up its record: {why}")),
    };
    let home = homes.join(name);
    let home = match std::fs::symlink_metadata(&home) {
        Ok(meta) => meta.is_dir(),
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => false,
        Err(why) => return Err(format!("looking at {}: {why}", home.display())),
    };
    Ok(record && home)
}

impl Pool {
    /// Replays the ledger. A `ready` account that is still as it was made stays ready. A
    /// `lent` account whose job is in `adopted` (a job the restarted agent took back, still
    /// running) is kept as it is: nothing of it is killed or deleted, and its uid stays held
    /// until [`Pool::retire`] ends it; when its record or its home is no longer there it is
    /// logged and deleted like any other, since its job cannot be running without them, and
    /// when they cannot be looked at it is logged and kept, since a failed look is no proof
    /// that the job is gone. Every other account that is not gone (`made` and not `ready`,
    /// `lent` to a job that is over, `held`) is deleted as [`Pool::retire`] does, with no
    /// refill, and recorded `gone` or `held`. Then each record named `kbf-acct-*` in the
    /// range that the ledger does not know (or knows as gone) is deleted by name the same
    /// way. Then the per-user folders of every uid of the range that no live account of the
    /// ledger holds are emptied (a crash between an account's deletion and the emptying of
    /// its folder leaves something in one); a folder that cannot be emptied is logged, and
    /// its slot is not free until it is.
    ///
    /// # Errors
    /// The ledger could not record an outcome, the records could not be listed, or a record
    /// the ledger does not know could not be deleted (an account the ledger does not hold
    /// cannot be recorded held, so the open fails naming it and the next open tries again).
    pub(super) fn sweep(&self, adopted: &BTreeSet<String>) -> Result<(), AccountError> {
        let todo = self.ledger().live();
        for (n, entry) in todo {
            let name = name(n);
            let homes = &self.settings.homes;
            let keep = match entry.state {
                State::Ready => intact(self, &name, entry.uid, homes) == Ok(true),
                State::Lent => self.keeps_adopted(n, &name, entry.uid, adopted),
                State::Made | State::Held | State::Gone => false,
            };
            if keep {
                continue;
            }
            let done = self.teardown(&name, entry.uid, &mut Vec::new());
            if let Err(failed) = self.settle(n, done) {
                if failed.step == Step::Ledger {
                    return Err(ledger_error(&self.path, failed.why));
                }
                tracing::warn!(account = %name, "held at {}: {}", failed.step, failed.why);
            }
        }
        let records = self
            .host
            .list_accounts(PREFIX)
            .map_err(|source| AccountError::Host {
                what: "listing the accounts of the pool",
                source,
            })?;
        for (name, uid) in records {
            let known = parse_name(&name)
                .and_then(|n| self.ledger().get(n))
                .is_some_and(|entry| entry.state != State::Gone);
            if known || !name.starts_with(PREFIX) || !self.settings.range.contains(&uid) {
                continue;
            }
            self.teardown(&name, uid, &mut Vec::new())
                .map_err(|failed| AccountError::Make {
                    account: name.clone(),
                    step: failed.step,
                    why: failed.why,
                })?;
            tracing::info!(account = %name, uid, "deleted an account the ledger did not know");
        }
        self.empty_free_folders();
        Ok(())
    }

    /// Empties the per-user folders of every uid of the range that no live account holds and
    /// that nothing else is using: a uid any user record has (an account this pool did not
    /// make, such as an earlier release's), or that a process runs as, is left alone, and so is
    /// one that cannot be looked at. Those folders are someone's working files.
    fn empty_free_folders(&self) {
        let held: BTreeSet<u32> = self
            .ledger()
            .live()
            .iter()
            .map(|(_, entry)| entry.uid)
            .collect();
        for uid in self
            .settings
            .range
            .clone()
            .filter(|uid| !held.contains(uid))
        {
            if let Some(why) = self.in_use(uid) {
                tracing::debug!(uid, "the per-user folders are left alone: {why}");
                continue;
            }
            if let Err(why) = folder::empty(&self.settings.user_folders, uid, &*self.os) {
                tracing::warn!(uid, "emptying the per-user folders of a free uid: {why}");
            }
        }
    }
}

impl Pool {
    /// Whether the lent account `n`, named `name` with `uid`, is kept at open: its job is one
    /// of `adopted`, and its record and home are there (or cannot be looked at).
    fn keeps_adopted(&self, n: u64, name: &str, uid: u32, adopted: &BTreeSet<String>) -> bool {
        let job = self.ledger().job(n).map(str::to_owned);
        let Some(job) = job.filter(|job| adopted.contains(job)) else {
            return false;
        };
        match intact(self, name, uid, &self.settings.homes) {
            Ok(true) => {
                tracing::info!(account = %name, uid, job = %job, "kept the account of an adopted job");
                true
            }
            Ok(false) => {
                tracing::warn!(
                    account = %name,
                    uid,
                    job = %job,
                    "the account of an adopted job has lost its record or its home, so it is deleted"
                );
                false
            }
            Err(why) => {
                tracing::warn!(
                    account = %name,
                    uid,
                    job = %job,
                    "kept the account of an adopted job that could not be looked at: {why}"
                );
                true
            }
        }
    }

    /// Why `uid` is in use by something the pool does not hold, or `None` when nothing is:
    /// a user record has it, a process runs as it, or one of the two could not be asked.
    fn in_use(&self, uid: u32) -> Option<String> {
        match self.host.uid_taken(uid) {
            Ok(false) => {}
            Ok(true) => return Some("a user record has the uid".to_owned()),
            Err(why) => return Some(format!("asking for a record of the uid: {why}")),
        }
        match self.host.processes(uid) {
            Ok(found) if found.is_empty() => None,
            Ok(found) => Some(format!("{} process(es) run as the uid", found.len())),
            Err(why) => Some(format!("listing the processes of the uid: {why}")),
        }
    }
}

#[cfg(test)]
mod tests;
