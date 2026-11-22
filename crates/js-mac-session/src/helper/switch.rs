//! The GUI session switch, `session-login` and `session-idle` (S4.2; fleet-updates.md
//! 10.2, layer L2), and the boot reset.
//!
//! - `session-login <lease>` needs a live lease user and FileVault off. It writes the
//!   intent record naming the lease, gives the user a fresh random password, writes
//!   `/etc/kcpassword` for it, sets `autoLoginUser` to the user, and only then asks for
//!   a userspace restart. Refused while the record names another lease.
//! - `session-idle` clears `autoLoginUser` and `/etc/kcpassword`, removes the intent
//!   record, and asks for the same restart: the Mac rests at the login window. With a
//!   VM user configured (`--vm-user`) it rests there instead: it needs the VM user's
//!   account and FileVault off, reads the VM user's password from the file
//!   `kbf-mac-provision` wrote (it never makes or resets one, which would strand the
//!   account's login keychain; a missing or invalid file fails, naming the path),
//!   writes `/etc/kcpassword` for it if that differs, sets `autoLoginUser` to it, then
//!   the intent record `vm`, and last asks for the restart. It switches a lease's
//!   session away too, as `session-idle` always has.
//! - `session-login <lease>` switches away from the VM user's session, the one session
//!   a lease may replace. That no VM runs then is the daemon's rule (it drains first).
//! - `boot-reset [held]` runs once per start of the helper, and every other verb is
//!   refused until it has succeeded: the daemon calls it before its `Hello`, naming the
//!   leases the server still holds for the node. The lease the intent record names is
//!   kept, with its auto-login, only if it is among them and its user is live. With a
//!   VM user configured and an account the helper acts on, the Mac's rest is kept too:
//!   when the record is `vm`, or when there is no record and `autoLoginUser` already
//!   names the VM user (provisioning set it; the record is written then). Otherwise
//!   auto-login and `/etc/kcpassword` are cleared and the record removed: a stale
//!   lease session never survives, and the Mac then sits at the login window until
//!   `session-idle` rests it at the VM user. The VM user is never deleted or killed
//!   here: the leases whose users are deleted are those the ledger holds, and a lease
//!   user's name cannot be the VM user's ([`crate::lease::VmUser`]). Then, when the helper
//!   holds Full Disk Access ([`Helper::full_disk_access`]; without it nothing of this
//!   paragraph's account deletions can work, and native jobs are refused anyway), every
//!   account named `kbf-lease-*` with a uid of the lease range that the ledger holds no
//!   live lease for is found in the directory ([`super::Host::lease_accounts`]) and ended:
//!   one named for a lease the ledger has not seen (a lost ledger) is recorded as
//!   `created` and ended with the ledger's leases below; an account of the pool a release
//!   before this one kept (`kbf-lease-pool-<n>`) is deleted by its name, after the pool's
//!   file is removed and the removal made durable (so a rollback finds no pool, and makes
//!   its own afresh); any other (a lease the ledger has as deleted, a name that is not a
//!   lease's, a uid the ledger holds for another lease) is deleted by its name. Those
//!   deletions are the ones of `user-delete` (the processes killed, the sweeps, the proof
//!   that the account is gone), with no lease to record. The account of the lease whose
//!   session is kept is never one of them. A deletion by name that fails fails the boot
//!   reset, naming the account and the step: a Mac where the grant does not let the helper
//!   delete must not start serving per-job accounts. Then every other live lease is ended as
//!   `user-delete` ends it (its processes killed, its account deleted and shown gone): a
//!   Mac that lost power mid-lease never stays logged in as a dead lease user. A lease
//!   whose account is not shown gone does not fail the boot reset: it stays live, its uid
//!   held, is logged, and is named in the reply ([`Reset::held`]), and the caller repeats
//!   its delete.
//!
//! The restart ends the daemon's connection, and the helper with it, so the reply to a
//! switch may never arrive; the daemon learns of the switch from its own restart.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use js_types::LeaseId;

use super::Helper;
use super::role::{Gui, Role};
use crate::autologin::{
    self, Intent, ensure_kcpassword, read_intent, remove, write_intent, write_kcpassword,
};
use crate::full_disk_access::FullDiskAccess;
use crate::lease::{Dotted, parse_lease, parse_lease_user, parse_pool_user};
use crate::ledger::{Entry, Ledger};

/// What the boot reset did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reset {
    /// The lease whose session was kept.
    pub kept: Option<LeaseId>,
    /// The leases whose accounts were deleted and shown gone.
    pub deleted: Vec<LeaseId>,
    /// The leases whose accounts were not shown gone: each stays live in the ledger with
    /// its uid held.
    pub held: Vec<LeaseId>,
}

/// What the boot reset does with a `kbf-lease-*` account of the lease range.
#[derive(Debug, PartialEq, Eq)]
enum Leftover {
    /// A live lease the ledger holds with this uid has it: the ledger's leases below end it.
    Held,
    /// Named for a lease the ledger has not seen: recorded as `created`, then ended with the
    /// ledger's leases.
    Unknown(LeaseId),
    /// Deleted by its name, with no lease to record; `pool` for an account of the pool a
    /// release before this one kept.
    ByName { pool: bool },
}

impl Helper {
    /// Whether the boot reset has run since the helper started.
    #[must_use]
    pub fn reset_done(&self) -> bool {
        self.reset_done.load(Ordering::Acquire)
    }

    /// `session-login <lease>`: auto-login into the lease's user, then a userspace
    /// restart.
    ///
    /// # Errors
    /// Why it was refused or failed. A failure after the intent record was written
    /// leaves it, so a later `session-idle` or the boot reset clears what was set.
    pub fn session_login(&self, lease: &str) -> Result<(), String> {
        let lease = parse_lease(lease)?;
        let paths = &self.settings.session;
        let ledger = self.lock();
        let name = self.lease_user(&ledger, lease)?.name;
        match read_intent(&paths.intent)? {
            Some(Intent::Lease(other)) if other != lease => {
                return Err(format!(
                    "the session belongs to lease {}; session-idle first",
                    Dotted(other)
                ));
            }
            _ => {}
        }
        autologin::require_filevault_off(self.host.as_ref())?;
        write_intent(&paths.intent, Intent::Lease(lease))
            .map_err(|why| format!("writing {}: {why}", paths.intent.display()))?;
        let password = self
            .host
            .reset_password(&name)
            .map_err(|why| format!("setting the password of {name}: {why}"))?;
        write_kcpassword(&paths.kcpassword, password.as_bytes())
            .map_err(|why| format!("writing {}: {why}", paths.kcpassword.display()))?;
        autologin::set_auto_login(self.host.as_ref(), paths, &name)?;
        drop(ledger);
        tracing::info!(lease = %Dotted(lease), "auto-login set to {name}; restarting userspace");
        autologin::restart_userspace(self.host.as_ref())
    }

    /// `session-idle`: rests the Mac, then a userspace restart: at the VM user when one
    /// is configured, else at the login window (auto-login cleared). A Mac already
    /// resting at a VM user whose GUI session is up is left alone: no restart, so
    /// running VMs survive a repeat.
    ///
    /// # Errors
    /// Auto-login could not be cleared (or set to the VM user, whose account must be
    /// one the helper acts on, with FileVault off), the record changed, or the restart
    /// asked for. A failure while setting the VM user leaves the previous record, so a
    /// lease's user stays guarded until the switch is made; a repeat or the boot reset
    /// finishes or clears what was set.
    pub fn session_idle(&self) -> Result<(), String> {
        let ledger = self.lock();
        if self.settings.vm_user.is_some() {
            let Some(name) = self.rest_at_vm()? else {
                tracing::info!("already resting at the VM user; nothing to restart");
                return Ok(());
            };
            drop(ledger);
            tracing::info!("auto-login set to {name}; restarting userspace");
        } else {
            self.clear_session()?;
            drop(ledger);
            tracing::info!("auto-login cleared; restarting userspace");
        }
        autologin::restart_userspace(self.host.as_ref())
    }

    /// Makes the VM user the one the login window logs in: the password,
    /// `/etc/kcpassword` and `autoLoginUser`, then the record last, so a failure before
    /// it leaves the old record guarding its lease user. Returns the VM user's name,
    /// or `None` when the Mac already rests there with its GUI session up.
    fn rest_at_vm(&self) -> Result<Option<String>, String> {
        let role = self.vm_role()?;
        let paths = &self.settings.session;
        autologin::require_filevault_off(self.host.as_ref())?;
        if self.resting_at_vm_with_gui(&role)? {
            return Ok(None);
        }
        // The VM user's password is set once, by `kbf-mac-provision`, and never changed
        // here: a reset would strand its login keychain.
        let password = autologin::read_vm_password(paths)?;
        ensure_kcpassword(&paths.kcpassword, password.as_bytes())
            .map_err(|why| format!("writing {}: {why}", paths.kcpassword.display()))?;
        autologin::set_auto_login(self.host.as_ref(), paths, role.name)?;
        write_intent(&paths.intent, Intent::Vm)
            .map_err(|why| format!("writing {}: {why}", paths.intent.display()))?;
        Ok(Some(role.name.to_owned()))
    }

    /// Whether the record is `vm`, `autoLoginUser` is the VM user and `gui/<uid>` exists.
    fn resting_at_vm_with_gui(&self, role: &Role<'_>) -> Result<bool, String> {
        let paths = &self.settings.session;
        if read_intent(&paths.intent)? != Some(Intent::Vm) {
            return Ok(false);
        }
        let named = autologin::read_auto_login(self.host.as_ref(), paths)?;
        Ok(named.as_deref() == Some(role.name) && self.vm_gui(role)? == Gui::Up)
    }

    /// Whether `autoLoginUser` is the VM user; anything unreadable is "no".
    fn auto_login_is_vm(&self, role: &Role<'_>) -> bool {
        match autologin::read_auto_login(self.host.as_ref(), &self.settings.session) {
            Ok(named) => named.as_deref() == Some(role.name),
            Err(why) => {
                tracing::warn!("auto-login is unreadable, so not the VM user's: {why}");
                false
            }
        }
    }

    /// Whether the boot reset keeps the Mac resting at the VM user: one is configured
    /// with an account the helper acts on, `autoLoginUser` is it, and the record names
    /// it or there is no record (the record is written then). A `vm` record over any
    /// other auto-login is a half-made switch and is cleared.
    fn keeps_vm_rest(&self, intent: Option<Intent>) -> bool {
        let Ok(role) = self.vm_role() else {
            return false;
        };
        match intent {
            Some(Intent::Vm) => self.auto_login_is_vm(&role),
            Some(Intent::Lease(_)) => false,
            None => {
                let paths = &self.settings.session;
                self.auto_login_is_vm(&role)
                    && write_intent(&paths.intent, Intent::Vm)
                        .map_err(|why| tracing::warn!("writing {}: {why}", paths.intent.display()))
                        .is_ok()
            }
        }
    }

    /// Clears auto-login and `/etc/kcpassword`, then removes the intent record.
    fn clear_session(&self) -> Result<(), String> {
        let paths = &self.settings.session;
        autologin::clear_auto_login(self.host.as_ref(), paths)?;
        remove(&paths.intent)
            .map(drop)
            .map_err(|why| format!("removing {}: {why}", paths.intent.display()))
    }

    /// `boot-reset [held]`: see the module documentation.
    ///
    /// # Errors
    /// It ran before, a held lease id does not parse, clearing auto-login failed, or an
    /// account that no lease of the ledger names could not be deleted and shown gone (each
    /// failure is named; the rest still ran). A lease of the ledger whose account is not
    /// shown gone is no error: it is in [`Reset::held`].
    pub fn boot_reset(&self, held: &[String]) -> Result<Reset, String> {
        let held: BTreeSet<LeaseId> = held
            .iter()
            .map(|lease| parse_lease(lease))
            .collect::<Result<_, _>>()?;
        let mut ledger = self.lock();
        if self.reset_done() {
            return Err(super::RESET_ALREADY.to_owned());
        }
        let intent = match read_intent(&self.settings.session.intent) {
            Ok(intent) => intent,
            Err(why) => {
                tracing::warn!("an unreadable intent record is stale: {why}");
                None
            }
        };
        let kept = match intent {
            Some(Intent::Lease(lease))
                if held.contains(&lease) && self.live_uid(&ledger, lease).is_ok() =>
            {
                Some(lease)
            }
            _ => None,
        };
        if kept.is_none() && !self.keeps_vm_rest(intent) {
            self.clear_session()?;
        }
        let kept_uid = kept
            .and_then(|lease| ledger.get(lease))
            .map(|entry| entry.uid);
        let mut errors = Vec::new();
        if self.full_disk_access() == FullDiskAccess::Granted {
            errors.extend(self.end_unknown_accounts(&mut ledger, kept_uid));
        }
        let mut deleted = Vec::new();
        let mut holding = Vec::new();
        for lease in ledger.live_leases() {
            if Some(lease) == kept {
                continue;
            }
            match self.remove_leftover(&mut ledger, lease) {
                Ok(()) => deleted.push(lease),
                Err(why) => {
                    tracing::warn!(lease = %Dotted(lease), "the account is not shown gone, and its uid stays held: {why}");
                    holding.push(lease);
                }
            }
        }
        if !errors.is_empty() {
            return Err(format!(
                "the boot reset is incomplete: {}",
                errors.join("; ")
            ));
        }
        self.reset_done.store(true, Ordering::Release);
        tracing::info!(kept = ?kept, deleted = deleted.len(), held = holding.len(), "boot reset done");
        Ok(Reset {
            kept,
            deleted,
            held: holding,
        })
    }

    /// Ends a lease left from before this start: its processes killed, its account deleted
    /// and shown gone, the lease recorded over.
    fn remove_leftover(&self, ledger: &mut Ledger, lease: LeaseId) -> Result<(), String> {
        self.kill_locked(ledger, lease)?;
        Ok(self.end_locked(ledger, lease).map(drop)?)
    }

    /// The boot reset's account step (see the module documentation): finds every
    /// `kbf-lease-*` account of the lease range, records those named for a lease the ledger
    /// has not seen, and deletes by name those no live lease of the ledger names. `kept_uid`
    /// is the uid of the lease whose session is kept: its account is never touched. What
    /// failed, account by account.
    fn end_unknown_accounts(&self, ledger: &mut Ledger, kept_uid: Option<u32>) -> Vec<String> {
        let accounts = match self.host.lease_accounts() {
            Ok(accounts) => accounts,
            Err(why) => return vec![format!("listing the lease accounts: {why}")],
        };
        let mut errors = Vec::new();
        let mut pool_file_removed = false;
        for (name, uid) in accounts {
            if !self.settings.range.contains(uid) || kept_uid == Some(uid) {
                continue;
            }
            let pool = match leftover(ledger, &name, uid) {
                Leftover::Held => continue,
                Leftover::Unknown(lease) => {
                    match ledger.record_created(lease, uid) {
                        Ok(()) => tracing::warn!(
                            lease = %Dotted(lease), uid,
                            "the ledger did not know {name}; it is recorded and ended"
                        ),
                        Err(why) => errors.push(format!("{name}: recording it: {why}")),
                    }
                    continue;
                }
                Leftover::ByName { pool } => pool,
            };
            if pool && !pool_file_removed {
                // Before the first of the pool's accounts goes, so a rollback to the release
                // with the pool finds no pool and makes its own afresh.
                match ledger.remove_pool_file() {
                    Ok(_) => pool_file_removed = true,
                    Err(why) => {
                        errors.push(format!("{name}: removing the pool file: {why}"));
                        continue;
                    }
                }
            }
            match self.delete_leftover(&name, uid) {
                Ok(()) => tracing::warn!(uid, "deleted {name}, which no lease of the ledger names"),
                Err(why) => errors.push(format!("{name}: {why}")),
            }
        }
        errors
    }
}

/// What the boot reset does with the account `name`, with `uid`, which no one asked about:
/// see [`Leftover`].
fn leftover(ledger: &Ledger, name: &str, uid: u32) -> Leftover {
    if parse_pool_user(name).is_some() {
        return Leftover::ByName { pool: true };
    }
    match parse_lease_user(name) {
        Some(lease) => match ledger.get(lease) {
            Some(Entry {
                uid: held,
                deleted: false,
            }) if held == uid => Leftover::Held,
            None => Leftover::Unknown(lease),
            Some(_) => Leftover::ByName { pool: false },
        },
        None => Leftover::ByName { pool: false },
    }
}

#[cfg(test)]
mod tests;
