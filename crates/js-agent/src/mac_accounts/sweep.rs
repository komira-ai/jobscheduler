//! What [`Pool::open`] does about what a crash, a restart or a lost ledger left: replays the
//! ledger and ends every account it does not show gone, then looks for records of the pool
//! that the ledger does not know, then empties the per-user folders of the slots no account
//! holds.

use std::collections::BTreeSet;
use std::path::Path;

use super::ledger::{State, name, parse_name};
use super::pool::{Pool, ledger_error};
use super::{AccountError, PREFIX, Step, folder};

/// Whether the account `name` has its record with `uid` and its home folder in `homes`: a
/// ready account that has both is as it was made.
fn intact(pool: &Pool, name: &str, uid: u32, homes: &Path) -> bool {
    let record = pool.host.account(name);
    let home = std::fs::symlink_metadata(homes.join(name));
    matches!(record, Ok(Some(found)) if found.uid == uid) && home.is_ok_and(|meta| meta.is_dir())
}

impl Pool {
    /// Replays the ledger: a `ready` account that is still as it was made stays ready;
    /// every other account that is not gone (`made` and not `ready`, `lent`, `held`) is
    /// deleted as [`Pool::retire`] does, with no refill, and recorded `gone` or `held`.
    /// Then each record named `kbf-acct-*` in the range that the ledger does not know (or
    /// knows as gone) is deleted by name the same way. Then the per-user folders of every uid
    /// of the range that no live account of the ledger holds are emptied (a crash between an
    /// account's deletion and the emptying of its folder leaves something in one); a folder
    /// that cannot be emptied is logged, and its slot is not free until it is.
    ///
    /// # Errors
    /// The ledger could not record an outcome, the records could not be listed, or a record
    /// the ledger does not know could not be deleted (an account the ledger does not hold
    /// cannot be recorded held, so the open fails naming it and the next open tries again).
    pub(super) fn sweep(&self) -> Result<(), AccountError> {
        let todo = self.ledger().live();
        for (n, entry) in todo {
            let name = name(n);
            if entry.state == State::Ready && intact(self, &name, entry.uid, &self.settings.homes) {
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

    /// Empties the per-user folders of every uid of the range that no live account holds.
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
            if let Err(why) = folder::empty(&self.settings.user_folders, uid, &*self.os) {
                tracing::warn!(uid, "emptying the per-user folders of a free uid: {why}");
            }
        }
    }
}

#[cfg(test)]
mod tests;
