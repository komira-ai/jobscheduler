//! The pool of lease users: `pool-create` and `pool-status`, a lease taking a free pool
//! user (`user-create`), and the end of a lease (`user-delete`), which resets the user in
//! place.
//!
//! The pool's users are made once and are permanent: the helper never deletes a pool
//! user's record, its home or a folder the system placed for it (macOS 27 refuses root
//! all three, with System Integrity Protection on; everything can be emptied). A slot is
//! reused by being reset where it is.
//!
//! - `pool-create` makes each slot of the pool (`--pool-size`, slot `n` with the uid
//!   `<first uid of the range> + n` and the name `kbf-lease-pool-<n>`) that the ledger
//!   does not hold, and resets again the user of each broken slot. A slot the ledger
//!   holds is kept as it is: running it again makes nothing new. A slot is made only when
//!   no live lease holds its uid, and either no user record has the uid and no process
//!   runs as it (the user is made), or the record that has it is the pool's own: named
//!   `kbf-lease-pool-<n>`, with that uid, hidden, and the home `<homes>/kbf-lease-pool-<n>`
//!   (a first install on a Mac whose earlier install left its users, with or without the
//!   ledger). That record is adopted after a clean reset in place; a record that differs
//!   in any of those is refused, naming how, and the slot is left unmade. The slot is
//!   recorded first (as resetting), then its home and its record are made (or the
//!   adopted user is reset), then it is recorded free; a failure leaves it broken. The
//!   boot reset runs it last, so the daemon's own start makes the pool; a failed slot is
//!   named and the rest still made.
//! - `user-create <lease>` takes the free slot with the lowest index and records it
//!   leased to the lease, then the lease with the slot's uid. No user is made or deleted
//!   for it. With no free slot it fails with [`PoolExhausted`], which the caller can
//!   retry once a lease ends; it never makes a user to make up for one. With a valid gate
//!   grant, the slot's user is added to the `admin` group for the lease.
//! - `user-delete <lease>` records the lease over (then the slot resetting) and resets the
//!   user ([`Helper::reset`]): the uid's processes killed and its launchd domains booted
//!   out (the teardown of `kill-uid`), its crontab and `at` jobs removed, its System V IPC
//!   objects removed, its files in the places `user-delete` sweeps removed, its home
//!   emptied (every entry below it removed; the folder itself is kept; its owner, group,
//!   mode, flags, ACL and extended attributes given back), its `DARWIN_USER_DIR` entry
//!   emptied (its `0`, `C` and `T` kept), the attributes of its record that the pool owns
//!   set back where root may write them on an existing account (primary group, shell,
//!   hidden; its uid and home path are compared, not written, and a record whose uid or
//!   home is not the slot's leaves the slot broken), the user taken out of
//!   `admin`, and last the directory asked that the user is not in `admin`. Each step is
//!   judged by its outcome, as the teardown's are; only a reset with every step clean
//!   returns the slot to free.
//! - A reset that is not clean leaves the slot broken ([`Helper::reset_or_make`] is the
//!   same reset, which first makes a user that has no record): no lease gets it,
//!   `pool-status` and an exhausted pool name it, the refusal and the `pool-create` log
//!   line name the entry or step that blocked it, and the next `pool-create` (or boot
//!   reset) resets it again.
//! - The lease of an administrator ends as any other: it is not replaced, there being no
//!   deletion to replace it with. Its user was added to `admin` at the start; the reset
//!   takes it out and the directory is asked that it is out, so a slot whose user stays
//!   an administrator is broken, never free. A slot is not retired for having been an
//!   administrator's: a retired slot could never be reclaimed (its user cannot be
//!   deleted), so a few grants would end the pool, and the reset removes what a
//!   replacement would have (the uid's processes, files, domains, IPC objects, record
//!   attributes and membership), though not what an administrator may have put in root's
//!   own places, which a replacement never reached either. The `resetting <slot> replace`
//!   ledger record is still written and read (the ledger's format only grows), and is
//!   reset like any other.
//! - The boot reset ends every live lease but the one whose session it keeps (as above),
//!   and then resets every slot that is resetting or leased to a lease that is not live
//!   (a crash between two records, or during a reset), before `pool-create`.

use std::io;

use js_types::LeaseId;

use super::Helper;
use super::teardown::TeardownError;
use crate::lease::{Dotted, Slot, pool_user_name, user_name};
use crate::ledger::Ledger;
use crate::ledger::pool::{SlotEntry, SlotState};
use crate::sweep::{SweepPlan, empty, empty_keeping, sweep};

/// What every [`PoolExhausted`] says first: a caller can tell the refusal by it.
pub const POOL_EXHAUSTED: &str = "the pool has no free user";

/// The program that answers whether a user is in the `admin` group.
pub const DSEDITGROUP: &str = "/usr/sbin/dseditgroup";

/// The folders of a `DARWIN_USER_DIR` entry the reset empties and keeps.
const KEPT_FOLDERS: [&str; 3] = ["0", "C", "T"];

/// The state of the pool's slots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolStatus {
    /// How many slots the pool has (`--pool-size`).
    pub size: u32,
    /// The free slots.
    pub free: Vec<Slot>,
    /// The leased slots, each with its lease.
    pub leased: Vec<(Slot, LeaseId)>,
    /// The slots being made, reset or replaced.
    pub resetting: Vec<Slot>,
    /// The slots whose user could be neither reset nor replaced: lent to no one.
    pub broken: Vec<Slot>,
    /// The slots of the pool that were never made.
    pub missing: Vec<Slot>,
    /// What `pool-create` could not do, slot by slot; empty in `pool-status`.
    pub errors: Vec<String>,
}

/// `user-create` found no free slot: retry once a lease ends (or, for a broken or
/// missing slot, once `pool-create` has made it).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{POOL_EXHAUSTED}: of {} slots, {} leased, {} resetting, broken: [{}], not made: [{}]; retry when a lease ends",
    .0.size,
    .0.leased.len(),
    .0.resetting.len(),
    list(&.0.broken),
    list(&.0.missing)
)]
pub struct PoolExhausted(pub Box<PoolStatus>);

/// `slots`, comma-separated.
fn list(slots: &[Slot]) -> String {
    slots
        .iter()
        .map(Slot::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why `user-create` gave the lease no user.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    /// No slot is free; the caller may retry.
    #[error(transparent)]
    Exhausted(#[from] PoolExhausted),
    /// Refused or failed, and why.
    #[error("{0}")]
    Refused(String),
}

impl From<String> for LeaseError {
    fn from(why: String) -> Self {
        Self::Refused(why)
    }
}

impl From<&str> for LeaseError {
    fn from(why: &str) -> Self {
        Self::Refused(why.to_owned())
    }
}

/// How the end of a slot's use came out.
#[derive(Debug)]
pub(super) enum Settled {
    /// The user is clean: the slot is free.
    Free,
    /// The reset in place did not come out clean: the slot is broken, and what blocked it.
    Broken(String),
}

/// The user of a live lease: its name, its uid and, for a pool user, its slot.
#[derive(Debug)]
pub(super) struct LeaseUser {
    pub(super) name: String,
    pub(super) uid: u32,
    pub(super) slot: Option<Slot>,
}

impl Helper {
    /// The user of the live `lease`: its pool user, or (for a lease a release before
    /// the pool created) its own `kbf-lease-<term>-<seq>`.
    pub(super) fn lease_user(&self, ledger: &Ledger, lease: LeaseId) -> Result<LeaseUser, String> {
        let uid = self.live_uid(ledger, lease)?;
        Ok(match ledger.slot_of(lease) {
            Some(slot) => LeaseUser {
                name: pool_user_name(slot),
                uid,
                slot: Some(slot),
            },
            None => LeaseUser {
                name: user_name(lease),
                uid,
                slot: None,
            },
        })
    }

    /// `pool-create`: see the module documentation. Returns the pool's state, with what
    /// could not be done slot by slot in its `errors`.
    pub fn pool_create(&self) -> PoolStatus {
        let mut ledger = self.lock();
        let errors = self.make_pool(&mut ledger);
        PoolStatus {
            errors,
            ..self.status(&ledger)
        }
    }

    /// `pool-status`: the pool's state.
    #[must_use]
    pub fn pool_status(&self) -> PoolStatus {
        self.status(&self.lock())
    }

    /// The state of each slot of the pool.
    fn status(&self, ledger: &Ledger) -> PoolStatus {
        let mut status = PoolStatus {
            size: self.settings.pool.get(),
            ..PoolStatus::default()
        };
        for slot in self.settings.pool.slots() {
            match ledger.slot(slot).map(|entry| entry.state) {
                None => status.missing.push(slot),
                Some(SlotState::Free) => status.free.push(slot),
                Some(SlotState::Leased { lease, .. }) => status.leased.push((slot, lease)),
                Some(SlotState::Resetting { .. }) => status.resetting.push(slot),
                Some(SlotState::Broken) => status.broken.push(slot),
            }
        }
        status
    }

    /// Makes each slot the ledger does not hold and resets each broken slot's user again
    /// in place; what failed, slot by slot.
    pub(super) fn make_pool(&self, ledger: &mut Ledger) -> Vec<String> {
        let mut errors = Vec::new();
        for slot in self.settings.pool.slots() {
            let result = match ledger.slot(slot).map(|entry| entry.state) {
                None => self.make_slot(ledger, slot),
                Some(SlotState::Broken) => match self.settle(ledger, slot, true) {
                    Ok(Settled::Free) => Ok(()),
                    Ok(Settled::Broken(why)) | Err(why) => Err(why),
                },
                Some(_) => Ok(()),
            };
            if let Err(why) = result {
                tracing::warn!(%slot, "pool-create: {why}");
                errors.push(format!("slot {slot}: {why}"));
            }
        }
        tracing::info!(errors = errors.len(), "pool-create done");
        errors
    }

    /// Whether the record that has `uid` is the pool's own user of `slot`: named
    /// `kbf-lease-pool-<slot>`, with that uid, hidden, and the home `<homes>/<name>`.
    /// Whatever else it has (its group, shell, membership of `admin`) is set back by the
    /// reset that follows.
    ///
    /// # Errors
    /// Why it is not adopted: another record has the uid (the name, uid, hidden flag or
    /// home differ), or the directory could not be asked.
    fn adoptable(&self, slot: Slot, uid: u32) -> Result<(), String> {
        let name = pool_user_name(slot);
        let record = self
            .host
            .user_record(&name)
            .map_err(|why| format!("reading the record {name}: {why}"))?;
        let home = self.settings.homes.join(&name);
        let Some(record) = record else {
            return Err(format!(
                "a user record the ledger does not hold has uid {uid} and is not {name}; it is not adopted"
            ));
        };
        let mut differs = Vec::new();
        if record.uid != uid {
            differs.push(format!("its uid is {}", record.uid));
        }
        if !record.hidden {
            differs.push("it is not hidden".to_owned());
        }
        if record.home != home {
            differs.push(format!("its home is {}", record.home.display()));
        }
        match differs.is_empty() {
            true => Ok(()),
            false => Err(format!(
                "a user record the ledger does not hold has uid {uid} and is not the pool's own {name} ({}); it is not adopted",
                differs.join(", ")
            )),
        }
    }

    /// Makes the user of `slot`, which the ledger does not hold: a new user, or the
    /// pool's own record of it that is already there ([`Self::adoptable`]; a first
    /// install on a Mac whose earlier install left the users, which can never be deleted),
    /// reset in place before the slot is free.
    fn make_slot(&self, ledger: &mut Ledger, slot: Slot) -> Result<(), String> {
        let range = self.settings.range;
        let uid = range
            .uid(slot)
            .ok_or_else(|| format!("the uid range {range} has no uid for it"))?;
        if ledger.held_uids().contains(&uid) {
            return Err(format!("uid {uid} is held by a live lease"));
        }
        let taken = self
            .host
            .uid_taken(uid)
            .map_err(|why| format!("looking up uid {uid}: {why}"))?;
        if taken {
            self.adoptable(slot, uid)?;
        } else {
            let left = self.live(uid)?;
            if !left.is_empty() {
                return Err(format!("{} processes run as uid {uid}", left.len()));
            }
        }
        ledger
            .record_slot(slot, uid)
            .map_err(|why| format!("recording slot {slot}: {why}"))?;
        if taken {
            return match self.settle(ledger, slot, false)? {
                Settled::Free => {
                    tracing::info!(%slot, uid, "adopted pool user {}", pool_user_name(slot));
                    Ok(())
                }
                Settled::Broken(why) => Err(why),
            };
        }
        let name = pool_user_name(slot);
        match self.create_account(&name, uid, false) {
            Ok(()) => {
                ledger
                    .record_free(slot)
                    .map_err(|why| format!("recording slot {slot} free: {why}"))?;
                tracing::info!(%slot, uid, "made pool user {name}");
                Ok(())
            }
            Err(why) => {
                ledger
                    .record_broken(slot)
                    .map_err(|e| format!("{why}; and recording slot {slot} broken: {e}"))?;
                Err(why.to_string())
            }
        }
    }

    /// A free slot of the pool, the one with the lowest index, and its uid; `None` when
    /// none is.
    pub(super) fn free_slot(&self, ledger: &Ledger) -> Option<(Slot, u32)> {
        self.settings
            .pool
            .slots()
            .find_map(|slot| match ledger.slot(slot) {
                Some(SlotEntry {
                    uid,
                    state: SlotState::Free,
                }) if self.settings.range.contains(uid) => Some((slot, uid)),
                _ => None,
            })
    }

    /// `user-create`'s part for the pool: `lease` takes a free slot. Returns its uid.
    pub(super) fn take_slot(
        &self,
        ledger: &mut Ledger,
        lease: LeaseId,
        admin: bool,
    ) -> Result<u32, LeaseError> {
        let (slot, uid) = self
            .free_slot(ledger)
            .ok_or_else(|| PoolExhausted(Box::new(self.status(ledger))))?;
        self.first_lease_user()?;
        ledger
            .record_leased(slot, lease, admin)
            .map_err(|why| format!("recording lease {}: {why}", Dotted(lease)))?;
        let name = pool_user_name(slot);
        if admin {
            self.host.set_admin(&name, true).map_err(|why| {
                format!(
                    "making {name} an administrator for lease {}: {why}",
                    Dotted(lease)
                )
            })?;
        }
        tracing::info!(lease = %Dotted(lease), %slot, uid, admin, "lease takes pool user {name}");
        Ok(uid)
    }

    /// `user-delete`'s part for the pool: the live `lease`, which holds `slot`, is over,
    /// and the slot's user is reset in place (an administrator's too: its membership of
    /// `admin` is taken away by the reset).
    pub(super) fn release(
        &self,
        ledger: &mut Ledger,
        lease: LeaseId,
        slot: Slot,
    ) -> Result<Settled, String> {
        ledger
            .record_released(slot, lease)
            .map_err(|why| format!("recording the end of lease {}: {why}", Dotted(lease)))?;
        tracing::info!(lease = %Dotted(lease), %slot, "lease over; resetting its pool user");
        self.settle(ledger, slot, false)
    }

    /// Resets every slot that is resetting, or leased to a lease that is not live; what
    /// failed, slot by slot.
    pub(super) fn reset_orphans(&self, ledger: &mut Ledger) -> Vec<String> {
        let mut errors = Vec::new();
        for (slot, entry) in ledger.slots() {
            let result = match entry.state {
                SlotState::Resetting { .. } => self.settle(ledger, slot, false),
                SlotState::Leased { lease, .. }
                    if ledger.get(lease).is_none_or(|entry| entry.deleted) =>
                {
                    ledger
                        .record_orphan_reset(slot)
                        .map_err(|why| format!("recording slot {slot} resetting: {why}"))
                        .and_then(|()| self.settle(ledger, slot, false))
                }
                _ => continue,
            };
            match result {
                Ok(Settled::Free) => tracing::info!(%slot, "an orphaned slot is reset"),
                Ok(Settled::Broken(why)) => tracing::warn!(%slot, "{why}"),
                Err(why) => errors.push(format!("slot {slot}: {why}")),
            }
        }
        errors
    }

    /// Ends the use of the resetting (or, when `was_broken`, broken) `slot`: resets its
    /// user in place ([`Self::reset_or_make`]). Records the slot free, or broken (a slot
    /// that was broken stays so, as the ledger holds it).
    ///
    /// # Errors
    /// The ledger could not record the outcome.
    fn settle(&self, ledger: &mut Ledger, slot: Slot, was_broken: bool) -> Result<Settled, String> {
        let name = pool_user_name(slot);
        // A slot is made with the uid of its index, and keeps it.
        let uid = ledger.slot(slot).map_or(0, |entry| entry.uid);
        match self.reset_or_make(&name, uid) {
            Ok(()) => {
                ledger
                    .record_free(slot)
                    .map_err(|why| format!("recording slot {slot} free: {why}"))?;
                tracing::info!(%slot, uid, "pool user {name} is free");
                Ok(Settled::Free)
            }
            Err(why) => {
                if !was_broken {
                    ledger
                        .record_broken(slot)
                        .map_err(|e| format!("{why}; and recording slot {slot} broken: {e}"))?;
                }
                Ok(Settled::Broken(format!(
                    "slot {slot}'s user {name} could not be reset in place, so the slot \
                     is broken and lent to no one until pool-create resets it again: {why}"
                )))
            }
        }
    }

    /// Resets the pool user `name`, with `uid`, whose account exists, for the next lease:
    /// see the module documentation. Each step is judged by its outcome; the first that does not hold
    /// fails the reset.
    pub(super) fn reset(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        self.kill_pool_user(name, uid)?;
        // The account is there: [`Self::reset_or_make`], the one caller, looked it up and
        // made it when it was not.
        let entries = self.user_entries(name, uid)?;
        self.settled(uid)?;
        Self::swept(sweep(&self.settings.schedules, name, uid), name)?;
        self.none_left(uid)?;
        self.remove_ipc(name, uid)?;
        Self::swept(sweep(&self.settings.sweep, name, uid), name)?;
        self.empty_home(name, uid)?;
        for entry in &entries {
            Self::swept(empty_keeping(entry, uid, &KEPT_FOLDERS), name)?;
        }
        self.restore_record(name, uid)?;
        // The kill discharged any GUI domain owed; this last look judges the domains again.
        self.settled(uid)?;
        self.not_admin(name)
    }

    /// Resets the pool user `name`, with `uid`, in place; a user with no record (a slot
    /// whose making failed) is made first, as `pool-create` makes one. Nothing is ever
    /// deleted. The slot is free only when every step of the reset is clean.
    pub(super) fn reset_or_make(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        let made = self
            .host
            .account(name)
            .map_err(|source| TeardownError::Lookup {
                name: name.to_owned(),
                source,
            })?;
        if made.is_none() {
            self.create_account(name, uid, false)?;
        }
        self.reset(name, uid)
    }

    /// Sets back the attributes of the record of the pool user `name`, with `uid`, that
    /// the pool owns and macOS lets root write on an existing account (its primary group,
    /// shell and hidden flag), and takes the user out of `admin` (a lease's grant put it
    /// there); the directory is asked last, by [`Self::not_admin`]. The record's uid and
    /// home path are read and compared, never written: macOS 27 refuses root a write of an
    /// existing account's home attribute (as it refuses the account's deletion), so a
    /// record whose uid or home is not the slot's cannot be repaired, and its slot stays
    /// broken, naming what differs.
    fn restore_record(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        let failed = |source: io::Error| TeardownError::Restore {
            name: name.to_owned(),
            source,
        };
        let home = self.settings.homes.join(name);
        let record = self
            .host
            .user_record(name)
            .map_err(failed)?
            .ok_or_else(|| failed(io::Error::other("there is no record")))?;
        let mut differs = Vec::new();
        if record.uid != uid {
            differs.push(format!("its uid is {}, not {uid}", record.uid));
        }
        if record.home != home {
            differs.push(format!(
                "its home is {}, not {}",
                record.home.display(),
                home.display()
            ));
        }
        if !differs.is_empty() {
            return Err(failed(io::Error::other(format!(
                "{}; macOS lets no process rewrite these on an existing account, so the slot cannot be repaired",
                differs.join(", ")
            ))));
        }
        let user = super::NewUser {
            name: name.to_owned(),
            uid,
            gid: self.settings.gid,
            home,
            admin: false,
        };
        self.host
            .restore_user(&user)
            .map_err(|source| TeardownError::Restore {
                name: name.to_owned(),
                source,
            })?;
        self.host
            .set_admin(name, false)
            .map_err(|why| TeardownError::Admin {
                name: name.to_owned(),
                why: why.to_string(),
            })
    }

    /// Whether `name` is in no way a member of `admin`, as the directory answers
    /// (`dseditgroup -o checkmember`, which says `no ...` and exits non-zero for a user
    /// that is not): judged by the outcome, so a membership a deletion failed to remove,
    /// which a new user of the same name would inherit, fails the step.
    fn not_admin(&self, name: &str) -> Result<(), TeardownError> {
        let failed = |why: String| TeardownError::Admin {
            name: name.to_owned(),
            why,
        };
        let output = self
            .host
            .command(DSEDITGROUP, &["-o", "checkmember", "-m", name, "admin"])
            .map_err(|why| failed(why.to_string()))?;
        match (
            output.success,
            output.stdout.trim_start().starts_with("no "),
        ) {
            (false, true) => Ok(()),
            (true, _) => Err(failed("the directory says it is a member".to_owned())),
            (false, false) => Err(failed(format!(
                "dseditgroup failed: {}",
                output.stderr.trim()
            ))),
        }
    }

    /// [`Helper::kill_account`] of the pool user `name`, with `uid`: a uid whose
    /// processes or domains outlive it is a lease's, quarantined, as `kill-uid` words it.
    fn kill_pool_user(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        self.kill_account(name, uid, false)
            .map_err(|why| match why {
                TeardownError::Outlived {
                    uid, left, stayed, ..
                } => TeardownError::Outlived {
                    uid,
                    left,
                    stayed,
                    lease: true,
                },
                other => other,
            })
    }

    /// What `user-delete` sweeps, with the home folder of each user.
    pub(super) fn with_home(&self) -> SweepPlan {
        let mut plan = self.settings.sweep.clone();
        plan.named
            .push(format!("{}/{{user}}", self.settings.homes.display()));
        plan
    }

    /// Empties the home of the pool user `name`, with `uid`, and gives it back (owner,
    /// group, mode 0700, no flag, ACL or extended attribute): the folder itself is never
    /// removed or made again, which macOS refuses for a folder of `/Users`. Fails only
    /// if an entry below it cannot be removed (named) or the folder cannot be given back.
    /// An empty folder below it that the system refuses root the removal of (an account's
    /// `Library/Preferences`) is kept, not a failure.
    fn empty_home(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        Self::swept(empty(&self.settings.homes.join(name), uid), name)?;
        self.make_home(name, uid)
    }

    /// Makes the home folder of `name`, with `uid`, or takes the one that is there.
    fn make_home(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        self.host
            .make_home(&self.settings.homes, name, uid, self.settings.gid)
            .map_err(|source| TeardownError::MakeHome {
                path: self.settings.homes.join(name),
                source,
            })
    }

    /// Makes the user `name`, with `uid`, and its home folder.
    pub(super) fn create_account(
        &self,
        name: &str,
        uid: u32,
        admin: bool,
    ) -> Result<(), TeardownError> {
        self.make_home(name, uid)?;
        let user = super::NewUser {
            name: name.to_owned(),
            uid,
            gid: self.settings.gid,
            home: self.settings.homes.join(name),
            admin,
        };
        self.host
            .create_user(&user)
            .map_err(|source| TeardownError::Create {
                name: name.to_owned(),
                source,
            })
    }
}

#[cfg(test)]
mod adopt_tests;
#[cfg(test)]
mod home_tests;
#[cfg(test)]
mod reset_tests;
#[cfg(test)]
mod tests;
