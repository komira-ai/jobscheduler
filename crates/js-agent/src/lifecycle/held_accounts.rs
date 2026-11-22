//! The accounts of native jobs that the helper did not delete: what its answer to `kill-uid`
//! or `user-delete` means for the lease, and the set of leases whose account is held.
//!
//! The helper makes each native job an account of its own and deletes it when the job ends,
//! shown gone. A deletion that fails at a step, or whose proof fails, is refused by name: the
//! lease stays live in the helper's ledger and its uid is held, so the helper's range has one
//! uid fewer to give, which is one native job fewer at a time (a Mac agent has no native slot
//! count of its own). The agent keeps the lease in its journal and in a [`HeldAccounts`],
//! tries it again at the end of each later job and at its next start, and ends it there when
//! the helper shows the account gone. The lifecycle taints the host when
//! [`ACCOUNTS_HELD_TAINT`](crate::lifecycle::ACCOUNTS_HELD_TAINT) accounts are held at once
//! ([`RuntimeError::account_held`]).
//!
//! A refusal is a hold. A call that failed on the socket, or that the helper refused because
//! it needs its boot reset again, decided nothing about the account and keeps the handling
//! it always had (the error, the lease live, the call made again).
//!
//! This module holds no platform code, so its rules are tested on every platform; only the
//! native runtime of a Mac (`native/held.rs`) calls the helper.

use std::collections::BTreeSet;

use js_mac_session::client::ClientError;
use js_mac_session::helper::RESET_NEEDED;

use super::RuntimeError;

/// What the helper's answer to `kill-uid` or `user-delete` says of the lease's account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Nothing is left to end: the helper says the lease has no user or that its user was
    /// deleted, which is the end that was wanted.
    Gone,
    /// The helper refused to end the account (a step failed, or the proof that it is gone
    /// did not hold): the lease stays live in its ledger, its uid held.
    Held,
    /// The call decided nothing: the socket failed, the helper answered something the
    /// request does not expect, or it refused because it needs its boot reset again.
    Failed,
}

/// Whether the helper refused because the lease has no user or its user was deleted.
#[must_use]
pub fn gone(why: &ClientError) -> bool {
    matches!(why, ClientError::Refused(reason)
        if reason.contains("has no user") || reason.contains("user was deleted"))
}

/// What the failure `why` of `kill-uid` or `user-delete` means for the lease's account.
#[must_use]
pub fn ending(why: &ClientError) -> Ending {
    match why {
        _ if gone(why) => Ending::Gone,
        ClientError::Refused(reason) if reason != RESET_NEEDED => Ending::Held,
        _ => Ending::Failed,
    }
}

/// What one pass of [`HeldAccounts::retry`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Retried {
    /// The leases whose account is now gone, which are no longer held.
    pub ended: Vec<String>,
    /// The leases still held, each with why the end did not complete.
    pub failed: Vec<(String, String)>,
}

/// The leases, as `<term>.<seq>`, whose account the helper did not delete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeldAccounts {
    leases: BTreeSet<String>,
}

impl HeldAccounts {
    /// Holds `lease` (holding it again changes nothing) and returns how many accounts are
    /// held, this one among them.
    pub fn hold(&mut self, lease: &str) -> usize {
        self.leases.insert(lease.to_owned());
        self.leases.len()
    }

    /// Gives `lease` up: its account is gone.
    pub fn release(&mut self, lease: &str) {
        self.leases.remove(lease);
    }

    /// How many accounts are held.
    #[must_use]
    pub fn count(&self) -> usize {
        self.leases.len()
    }

    /// The failure of a removal that ended the run and left its account held, for the
    /// lifecycle: `why` is what the helper said, and the count is the accounts held now.
    #[must_use]
    pub fn error(&self, why: impl Into<String>) -> RuntimeError {
        RuntimeError::account_held(why, self.count())
    }

    /// Tries `end` on every held lease, in the order the leases sort: a lease it ends is
    /// given up, one it cannot end stays held with the reason it gave.
    pub fn retry(&mut self, mut end: impl FnMut(&str) -> Result<(), String>) -> Retried {
        let mut retried = Retried::default();
        for lease in self.leases.clone() {
            match end(&lease) {
                Ok(()) => {
                    self.release(&lease);
                    retried.ended.push(lease);
                }
                Err(why) => retried.failed.push((lease, why)),
            }
        }
        retried
    }
}

#[cfg(test)]
#[path = "held_accounts_tests.rs"]
mod tests;
