//! The boot reset.
//!
//! `boot-reset [held]` runs once per start of the helper, and every other verb but
//! `full-disk-access` is refused until it has succeeded: the daemon calls it before its
//! `Hello`, naming the leases the server still holds for the node. The held ids are parsed
//! strictly, so a malformed one refuses the reset, but they keep nothing: no lease keeps a
//! session over a restart. The reset first clears auto-login: the login window's
//! `autoLoginUser` and `/etc/kcpassword` ([`crate::autologin::clear_auto_login`]), so a
//! Mac of the pool never logs a user in at its start. Then every live lease is ended as
//! `user-delete` ends it (its pool user reset, or replaced; the user of a lease from before
//! the pool killed and deleted): a Mac that lost power mid-lease never keeps a dead lease's
//! user. Then every pool slot that is resetting, or leased to a lease that is not live, is
//! reset, and last `pool-create` makes the slots that are missing and replaces the broken
//! ones. A reset that fails part-way can be repeated; a pool user left broken, or a slot
//! that could not be made, does not fail it.

use std::sync::atomic::Ordering;

use js_types::LeaseId;

use super::Helper;
use super::pool::Settled;
use crate::autologin;
use crate::lease::{Dotted, parse_lease};
use crate::ledger::Ledger;

/// What the boot reset did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reset {
    /// The leases whose users were deleted.
    pub deleted: Vec<LeaseId>,
}

impl Helper {
    /// Whether the boot reset has run since the helper started.
    #[must_use]
    pub fn reset_done(&self) -> bool {
        self.reset_done.load(Ordering::Acquire)
    }

    /// `boot-reset [held]`: see the module documentation.
    ///
    /// # Errors
    /// It ran before, a held lease id does not parse, or clearing auto-login, killing or
    /// deleting a leftover user failed (each failure is named; the rest still ran).
    pub fn boot_reset(&self, held: &[String]) -> Result<Reset, String> {
        for lease in held {
            parse_lease(lease)?;
        }
        let mut ledger = self.lock();
        if self.reset_done() {
            return Err(super::RESET_ALREADY.to_owned());
        }
        autologin::clear_auto_login(self.host.as_ref(), &self.settings.login)?;
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for lease in ledger.live_leases() {
            match self.remove_leftover(&mut ledger, lease) {
                Ok(()) => deleted.push(lease),
                Err(why) => errors.push(format!("lease {}: {why}", Dotted(lease))),
            }
        }
        errors.extend(self.reset_orphans(&mut ledger));
        if !errors.is_empty() {
            return Err(format!(
                "the boot reset is incomplete: {}",
                errors.join("; ")
            ));
        }
        // A slot that could not be made is named in the log and in `pool-status`; it
        // does not stop the Mac from serving with the rest of the pool.
        self.make_pool(&mut ledger);
        self.reset_done.store(true, Ordering::Release);
        tracing::info!(deleted = deleted.len(), "boot reset done");
        Ok(Reset { deleted })
    }

    /// Ends a lease left from before this start: its pool user is reset (or replaced),
    /// the user of a lease from before the pool killed and deleted. A pool user left
    /// broken does not stop the reset: it is lent to no one, and `pool-create` replaces it.
    fn remove_leftover(&self, ledger: &mut Ledger, lease: LeaseId) -> Result<(), String> {
        let user = self.lease_user(ledger, lease)?;
        if let Some(slot) = user.slot {
            return match self.release(ledger, lease, slot)? {
                Settled::Free => Ok(()),
                Settled::Broken(why) => {
                    tracing::warn!(lease = %Dotted(lease), "{why}");
                    Ok(())
                }
            };
        }
        self.kill_locked(ledger, lease)?;
        Ok(self.end_locked(ledger, lease).map(drop)?)
    }
}

#[cfg(test)]
mod tests;
