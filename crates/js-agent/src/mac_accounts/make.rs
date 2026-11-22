//! Making an account: choosing its uid, recording it, making its home and its record.

use super::error::Failure;
use super::host::NewUser;
use super::ledger::{Record, name};
use super::pool::Pool;
use super::{Account, AccountError, Step, tree, uid};

/// How many uids a refusal for want of a free one names; it counts the rest.
const REASONS: usize = 8;

impl Pool {
    /// The first free uid of the range after the one the ledger's last `made` record names.
    ///
    /// # Errors
    /// No uid is free: the refusal names up to [`REASONS`] uids and why each is not.
    fn choose_uid(&self) -> Result<u32, AccountError> {
        let last = self.ledger().last_uid();
        let mut busy = Vec::new();
        for uid in uid::after(&self.settings.range, last) {
            match self.why_busy(uid) {
                None => return Ok(uid),
                Some(why) => busy.push(format!("{uid}: {why}")),
            }
        }
        let more = busy.len().saturating_sub(REASONS);
        busy.truncate(REASONS);
        if more > 0 {
            busy.push(format!("and {more} more"));
        }
        Err(AccountError::NoFreeUid {
            range: format!(
                "{}-{}",
                self.settings.range.start(),
                self.settings.range.end()
            ),
            reasons: busy,
        })
    }

    /// Why `uid` is not free; `None` when it is. The first reason found: an account of the
    /// ledger that is not gone, a record, a live process, an entry in the per-user folders,
    /// an entry in the shared places. An error in looking is a reason.
    fn why_busy(&self, uid: u32) -> Option<String> {
        if let Some(n) = self.ledger().holding(uid) {
            return Some(format!("held by {}", name(n)));
        }
        match self.host.uid_taken(uid) {
            Err(why) => return Some(format!("looking up its record: {why}")),
            Ok(true) => return Some("a user record has it".to_owned()),
            Ok(false) => {}
        }
        match self.host.processes(uid) {
            Err(why) => return Some(format!("listing its processes: {why}")),
            Ok(left) if !left.is_empty() => {
                return Some(format!("{} processes run as it", left.len()));
            }
            Ok(_) => {}
        }
        match tree::folder_entries(&self.settings.user_folders, uid) {
            Err(why) => return Some(format!("searching the per-user folders: {why}")),
            Ok(found) if !found.is_empty() => {
                return Some(format!("{} is owned by it", found[0].display()));
            }
            Ok(_) => {}
        }
        match tree::left_in(&self.settings.swept_places(), None, uid) {
            Err(why) => Some(format!("looking in the shared places: {why}")),
            Ok(found) if !found.is_empty() => {
                Some(format!("{} is owned by it", found[0].display()))
            }
            Ok(_) => None,
        }
    }

    /// Makes an account, with the maker held: chooses its uid, records `made` (durably,
    /// before anything exists), makes its home and its record, then records `ready`, or,
    /// when it is made for the lend of `job`, `lent` to it directly (never `ready`, which a
    /// lend beside this one could take). A failure after `made` deletes
    /// what was made and records how that ended ([`Pool::unmake`]).
    ///
    /// # Errors
    /// No free uid, or a step failed (named, with how the deletion of what was made ended).
    pub(super) fn make(&self, job: Option<&str>) -> Result<Account, AccountError> {
        let uid = self.choose_uid()?;
        let n = self.ledger().next();
        let name = name(n);
        self.record(&Record::Made(n, uid))
            .map_err(|why| make_error(&name, Step::Ledger, format!("recording made: {why}")))?;
        let gid = self.settings.gid;
        let home = self.settings.homes.join(&name);
        let user = NewUser {
            name: name.clone(),
            uid,
            gid,
            home: home.clone(),
        };
        let made = self
            .host
            .make_home(&self.settings.homes, &name, uid, gid)
            .map_err(|why| Failure::new(Step::Home, format!("making {}: {why}", home.display())))
            .and_then(|()| {
                self.host.create_account(&user).map_err(|why| {
                    Failure::new(Step::Record, format!("creating the record: {why}"))
                })
            })
            .and_then(|()| match job {
                // Made for a lend: `made` goes straight to `lent`, so the account is never
                // visible as ready to a lend that runs beside this one.
                Some(job) => self
                    .record(&Record::Lent(n, job.to_owned()))
                    .map_err(|why| Failure::new(Step::Ledger, format!("recording lent: {why}"))),
                None => self
                    .record(&Record::Ready(n))
                    .map_err(|why| Failure::new(Step::Ledger, format!("recording ready: {why}"))),
            });
        match made {
            Ok(()) => {
                tracing::info!(account = %name, uid, "made an account");
                Ok(Account { name, uid, gid })
            }
            Err(failed) => Err(self.unmake(n, uid, &failed)),
        }
    }

    /// Deletes what a failed making left of account `n` (steps 3 to 6 of a deletion: nothing
    /// of it ever ran), records `gone` when it is shown gone and `held` when it is not, and
    /// the error that says what failed and how that ended.
    fn unmake(&self, n: u64, uid: u32, failed: &Failure) -> AccountError {
        let name = name(n);
        let ended = match self.settle(n, self.cleanup(&name, uid)) {
            Ok(_) => "what was made was deleted and shown gone".to_owned(),
            Err(held) => format!(
                "deleting what was made did not complete, at {}: {}",
                held.step, held.why
            ),
        };
        make_error(&name, failed.step, format!("{}; {ended}", failed.why))
    }
}

/// [`AccountError::Make`] for the account `account` and the step that failed.
fn make_error(account: &str, step: Step, why: String) -> AccountError {
    AccountError::Make {
        account: account.to_owned(),
        step,
        why,
    }
}

#[cfg(test)]
mod tests;
