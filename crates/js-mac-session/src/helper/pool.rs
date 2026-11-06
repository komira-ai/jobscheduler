//! The pool of lease users: `pool-create` and `pool-status`, a lease taking a free pool
//! user (`user-create`), and the end of a lease (`user-delete`), which resets the user,
//! or replaces it when the reset is not clean.
//!
//! - `pool-create` makes each slot of the pool (`--pool-size`, slot `n` with the uid
//!   `<first uid of the range> + n` and the name `kbf-lease-pool-<n>`) that the ledger
//!   does not hold, and replaces the user of each broken slot. A slot the ledger holds is
//!   adopted as it is: running it again makes nothing new. A slot is made only when no
//!   live lease holds its uid, no user record has the uid and no process runs as it: a
//!   user the ledger does not hold is never adopted. The slot is recorded first (as
//!   resetting), then its home and its record are made, then it is recorded free; a
//!   failure leaves it broken. The boot reset runs it last, so the daemon's own start
//!   makes the pool; a failed slot is named and the rest still made.
//! - `user-create <lease>` takes the free slot with the lowest index and records it
//!   leased to the lease, then the lease with the slot's uid. No user is made or deleted
//!   for it. With no free slot it fails with [`PoolExhausted`], which the caller can
//!   retry once a lease ends; it never makes a user to make up for one. With a valid gate
//!   grant, the slot's user is replaced by an administrator for the lease.
//! - `user-delete <lease>` records the lease over (then the slot resetting) and resets the
//!   user ([`Helper::reset`]): the uid's processes killed and its launchd domains booted
//!   out (the teardown of `kill-uid`), its crontab and `at` jobs removed, its System V IPC
//!   objects removed, its files in the places `user-delete` sweeps removed, its home
//!   removed whole and made again empty, and its `DARWIN_USER_DIR` entry emptied (its
//!   `0`, `C` and `T` kept). Each step is judged by its outcome, as the teardown's are;
//!   only a reset with every step clean returns the slot to free.
//! - A reset that is not clean replaces the user ([`Helper::replace`]): the uid's
//!   processes killed again, the user deleted as `user-delete` deleted a lease's user
//!   (its `DARWIN_USER_DIR` entry and its home removed whole), then a new user made in the
//!   slot, with the same name and uid. The slot is free only once the new user exists.
//!   A replacement that fails too leaves the slot broken: no lease gets it, `pool-status`
//!   and an exhausted pool name it, and the next `pool-create` (or boot reset) replaces it
//!   again. The lease of an administrator is always replaced, never reset.
//! - The boot reset ends every live lease but the one whose session it keeps (as above),
//!   and then resets every slot that is resetting or leased to a lease that is not live
//!   (a crash between two records, or during a reset), before `pool-create`.

use js_types::LeaseId;

use super::Helper;
use super::teardown::TeardownError;
use crate::lease::{Dotted, Slot, pool_user_name, user_name};
use crate::ledger::Ledger;
use crate::ledger::pool::{SlotEntry, SlotState};
use crate::sweep::{SweepPlan, empty_keeping, sweep};

/// What every [`PoolExhausted`] says first: a caller can tell the refusal by it.
pub const POOL_EXHAUSTED: &str = "the pool has no free user";

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

/// How a slot's use ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// Reset the user, and replace it if the reset is not clean.
    Reset,
    /// Replace the user (an administrator's lease ended).
    Replace,
    /// Replace the user of a broken slot; it stays broken if that fails.
    Repair,
}

/// How the end of a slot's use came out.
#[derive(Debug)]
pub(super) enum Settled {
    /// The user is clean (or new): the slot is free.
    Free,
    /// Neither the reset nor the replacement succeeded: the slot is broken, and why.
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
                Some(SlotState::Resetting) => status.resetting.push(slot),
                Some(SlotState::Broken) => status.broken.push(slot),
            }
        }
        status
    }

    /// Makes each slot the ledger does not hold and replaces each broken slot's user;
    /// what failed, slot by slot.
    pub(super) fn make_pool(&self, ledger: &mut Ledger) -> Vec<String> {
        let mut errors = Vec::new();
        for slot in self.settings.pool.slots() {
            let result = match ledger.slot(slot).map(|entry| entry.state) {
                None => self.make_slot(ledger, slot),
                Some(SlotState::Broken) => match self.settle(ledger, slot, End::Repair) {
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

    /// Makes the user of `slot`, which the ledger does not hold.
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
            return Err(format!(
                "a user record the ledger does not hold has uid {uid}; it is not adopted"
            ));
        }
        let left = self.live(uid)?;
        if !left.is_empty() {
            return Err(format!("{} processes run as uid {uid}", left.len()));
        }
        ledger
            .record_slot(slot, uid)
            .map_err(|why| format!("recording slot {slot}: {why}"))?;
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
            self.replace(&name, uid, true).map_err(|why| {
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
    /// and the slot's user is reset (replaced, for an administrator's lease).
    pub(super) fn release(
        &self,
        ledger: &mut Ledger,
        lease: LeaseId,
        slot: Slot,
    ) -> Result<Settled, String> {
        let end = match ledger.slot(slot) {
            Some(SlotEntry {
                state: SlotState::Leased { admin: true, .. },
                ..
            }) => End::Replace,
            _ => End::Reset,
        };
        ledger
            .record_released(slot, lease)
            .map_err(|why| format!("recording the end of lease {}: {why}", Dotted(lease)))?;
        tracing::info!(lease = %Dotted(lease), %slot, "lease over; resetting its pool user");
        self.settle(ledger, slot, end)
    }

    /// Resets every slot that is resetting, or leased to a lease that is not live; what
    /// failed, slot by slot.
    pub(super) fn reset_orphans(&self, ledger: &mut Ledger) -> Vec<String> {
        let mut errors = Vec::new();
        for (slot, entry) in ledger.slots() {
            let result = match entry.state {
                SlotState::Resetting => self.settle(ledger, slot, End::Reset),
                SlotState::Leased { lease, admin }
                    if ledger.get(lease).is_none_or(|entry| entry.deleted) =>
                {
                    let end = match admin {
                        true => End::Replace,
                        false => End::Reset,
                    };
                    ledger
                        .record_orphan_reset(slot)
                        .map_err(|why| format!("recording slot {slot} resetting: {why}"))
                        .and_then(|()| self.settle(ledger, slot, end))
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

    /// Ends the use of the resetting (or, for [`End::Repair`], broken) `slot`, as `end`
    /// says. Records the slot free, or broken.
    ///
    /// # Errors
    /// The ledger could not record the outcome.
    fn settle(&self, ledger: &mut Ledger, slot: Slot, end: End) -> Result<Settled, String> {
        let name = pool_user_name(slot);
        // A slot is made with the uid of its index, and keeps it.
        let uid = ledger.slot(slot).map_or(0, |entry| entry.uid);
        let outcome = match end {
            End::Reset => self.reset(&name, uid).or_else(|why| {
                tracing::warn!(%slot, uid, "the reset of {name} is not clean ({why}); replacing it");
                self.replace(&name, uid, false)
            }),
            End::Replace | End::Repair => self.replace(&name, uid, false),
        };
        match outcome {
            Ok(()) => {
                ledger
                    .record_free(slot)
                    .map_err(|why| format!("recording slot {slot} free: {why}"))?;
                tracing::info!(%slot, uid, "pool user {name} is free");
                Ok(Settled::Free)
            }
            Err(why) => {
                if end != End::Repair {
                    ledger
                        .record_broken(slot)
                        .map_err(|e| format!("{why}; and recording slot {slot} broken: {e}"))?;
                }
                Ok(Settled::Broken(format!(
                    "slot {slot}'s user {name} could be neither reset nor replaced, so the slot \
                     is broken and lent to no one until pool-create replaces it: {why}"
                )))
            }
        }
    }

    /// Resets the pool user `name`, with `uid`, for the next lease: see the module
    /// documentation. Each step is judged by its outcome; the first that does not hold
    /// fails the reset.
    pub(super) fn reset(&self, name: &str, uid: u32) -> Result<(), TeardownError> {
        self.kill_account(name, uid, false)?;
        if self
            .host
            .account(name)
            .map_err(|source| TeardownError::Lookup {
                name: name.to_owned(),
                source,
            })?
            .is_none()
        {
            return Err(TeardownError::Missing {
                name: name.to_owned(),
            });
        }
        let entries = self.user_entries(name, uid)?;
        self.settled(uid)?;
        Self::swept(sweep(&self.settings.schedules, name, uid), name)?;
        self.none_left(uid)?;
        self.remove_ipc(name, uid)?;
        Self::swept(sweep(&self.with_home(), name, uid), name)?;
        self.make_home(name, uid)?;
        for entry in &entries {
            Self::swept(empty_keeping(entry, uid, &KEPT_FOLDERS), name)?;
        }
        // The kill discharged any GUI domain owed; this last look judges the domains again.
        self.settled(uid)?;
        Ok(())
    }

    /// Replaces the pool user `name`, with `uid`: kills its processes, deletes it (and
    /// what it leaves, as `user-delete` did a lease's user), and makes a new one with the
    /// same name and uid, an administrator when `admin`.
    pub(super) fn replace(&self, name: &str, uid: u32, admin: bool) -> Result<(), TeardownError> {
        self.kill_account(name, uid, false)?;
        self.remove_account(name, uid)?;
        self.create_account(name, uid, admin)
    }

    /// What `user-delete` sweeps, with the home folder of each user.
    pub(super) fn with_home(&self) -> SweepPlan {
        let mut plan = self.settings.sweep.clone();
        plan.named
            .push(format!("{}/{{user}}", self.settings.homes.display()));
        plan
    }

    /// Makes the home folder of `name`, with `uid`.
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
mod tests;
