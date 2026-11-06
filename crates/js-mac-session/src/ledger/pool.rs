//! The pool's file: the state of each slot of the pool of lease users.
//!
//! One record per line, as the lease ledger's (see the parent module):
//!
//! ```text
//! made <slot> <uid>               the slot's user is being made: resetting
//! leased <slot> <term>.<seq>      the slot's user is the lease's
//! leased <slot> <term>.<seq> admin   ... made an administrator for the lease
//! resetting <slot>                the lease is over; the user is being reset (or replaced)
//! free <slot>                     the user is clean: the next lease may take it
//! broken <slot>                   the user could be neither reset nor replaced
//! ```
//!
//! Each record must follow from the slot's state before it: `made` once per slot, with a
//! uid no other slot has; `leased` only from free, and only for a lease no slot is
//! leased to; `resetting` only from leased; `free` only from resetting or broken (a
//! broken slot whose user was replaced later); `broken` only from resetting. A line that
//! does not stops the helper from starting, as in the lease ledger.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;

use js_types::LeaseId;

use super::{bad_line, open_records, refused, write_record};
use crate::lease::{Dotted, Slot, parse_lease};

/// What a slot's user is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// Clean, and taken by the next lease.
    Free,
    /// The user of `lease`, an administrator when `admin`.
    Leased {
        /// The lease the user is lent to.
        lease: LeaseId,
        /// Whether the user was made an administrator for the lease (with a gate grant).
        admin: bool,
    },
    /// Being made, reset after a lease, or replaced: lent to no one until it is free.
    Resetting,
    /// Neither the reset nor the replacement of its user succeeded: lent to no one, until
    /// a `pool-create` (or the boot reset) replaces the user.
    Broken,
}

/// One slot's record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotEntry {
    /// The uid of the slot's user, for good.
    pub uid: u32,
    /// What the user is doing.
    pub state: SlotState,
}

/// One line of the pool's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Record {
    Made(Slot, u32),
    Leased(Slot, LeaseId, bool),
    Resetting(Slot),
    Free(Slot),
    Broken(Slot),
}

impl Record {
    fn parse(line: &str) -> Result<Self, String> {
        let words: Vec<&str> = line.split(' ').collect();
        let slot = |text: &str| text.parse::<Slot>();
        match words.as_slice() {
            ["made", n, uid] => Ok(Self::Made(
                slot(n)?,
                uid.parse().map_err(|_| format!("bad uid {uid:?}"))?,
            )),
            ["leased", n, lease] => Ok(Self::Leased(slot(n)?, parse_lease(lease)?, false)),
            ["leased", n, lease, "admin"] => Ok(Self::Leased(slot(n)?, parse_lease(lease)?, true)),
            ["resetting", n] => Ok(Self::Resetting(slot(n)?)),
            ["free", n] => Ok(Self::Free(slot(n)?)),
            ["broken", n] => Ok(Self::Broken(slot(n)?)),
            _ => Err(format!("unknown record {line:?}")),
        }
    }
}

impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Made(slot, uid) => write!(f, "made {slot} {uid}"),
            Self::Leased(slot, lease, false) => write!(f, "leased {slot} {}", Dotted(*lease)),
            Self::Leased(slot, lease, true) => write!(f, "leased {slot} {} admin", Dotted(*lease)),
            Self::Resetting(slot) => write!(f, "resetting {slot}"),
            Self::Free(slot) => write!(f, "free {slot}"),
            Self::Broken(slot) => write!(f, "broken {slot}"),
        }
    }
}

/// The pool's file and what it says.
#[derive(Debug)]
pub(super) struct Pool {
    file: File,
    slots: BTreeMap<Slot, SlotEntry>,
    /// How many more records may be written before every write fails.
    #[cfg(test)]
    writes_left: Option<usize>,
}

impl Pool {
    /// Opens (creating, mode 0600) and reads the pool's file at `path`.
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let (file, text) = open_records(path)?;
        let mut pool = Self {
            file,
            slots: BTreeMap::new(),
            #[cfg(test)]
            writes_left: None,
        };
        for (number, line) in text.lines().enumerate() {
            let record = Record::parse(line)
                .and_then(|record| pool.admit(record).map(|()| record))
                .map_err(|why| bad_line(path, number, &why))?;
            pool.commit(record);
        }
        Ok(pool)
    }

    /// Whether `record` may follow what the file holds.
    fn admit(&self, record: Record) -> Result<(), String> {
        let state = |slot: Slot| {
            self.slots
                .get(&slot)
                .map(|entry| entry.state)
                .ok_or_else(|| format!("slot {slot} was never made"))
        };
        match record {
            Record::Made(slot, _) if self.slots.contains_key(&slot) => {
                Err(format!("slot {slot} made twice"))
            }
            Record::Made(slot, uid) => match self.slots.iter().find(|(_, e)| e.uid == uid) {
                Some((other, _)) => Err(format!("slot {slot}'s uid {uid} is slot {other}'s")),
                None => Ok(()),
            },
            Record::Leased(slot, lease, _) => match (state(slot)?, self.leased_to(lease)) {
                (_, Some(other)) => Err(format!(
                    "lease {} holds slot {other} already",
                    Dotted(lease)
                )),
                (SlotState::Free, None) => Ok(()),
                _ => Err(format!("slot {slot} is not free")),
            },
            Record::Resetting(slot) => match state(slot)? {
                SlotState::Leased { .. } => Ok(()),
                _ => Err(format!("slot {slot} is not leased")),
            },
            Record::Free(slot) => match state(slot)? {
                SlotState::Resetting | SlotState::Broken => Ok(()),
                _ => Err(format!("slot {slot} is neither resetting nor broken")),
            },
            Record::Broken(slot) => match state(slot)? {
                SlotState::Resetting => Ok(()),
                _ => Err(format!("slot {slot} is not resetting")),
            },
        }
    }

    fn commit(&mut self, record: Record) {
        let (slot, state) = match record {
            Record::Made(slot, uid) => {
                let state = SlotState::Resetting;
                self.slots.insert(slot, SlotEntry { uid, state });
                return;
            }
            Record::Leased(slot, lease, admin) => (slot, SlotState::Leased { lease, admin }),
            Record::Resetting(slot) => (slot, SlotState::Resetting),
            Record::Free(slot) => (slot, SlotState::Free),
            Record::Broken(slot) => (slot, SlotState::Broken),
        };
        // Admitted, so the slot was made.
        self.slots
            .entry(slot)
            .and_modify(|entry| entry.state = state);
    }

    /// Writes `record` in one `write`, `fsync`s, then applies it.
    pub(super) fn append(&mut self, record: Record) -> io::Result<()> {
        self.admit(record).map_err(refused)?;
        #[cfg(test)]
        if let Some(left) = &mut self.writes_left {
            *left = left
                .checked_sub(1)
                .ok_or_else(|| io::Error::other("the disk is full"))?;
        }
        write_record(&mut self.file, &record.to_string())?;
        self.commit(record);
        Ok(())
    }

    /// Whether a `resetting` for `slot` may be written: the slot is leased (to `lease`,
    /// when one is named).
    pub(super) fn admit_reset(&self, slot: Slot, lease: Option<LeaseId>) -> Result<(), String> {
        match (self.slots.get(&slot).map(|entry| entry.state), lease) {
            (Some(SlotState::Leased { lease: holder, .. }), Some(lease)) if holder != lease => {
                Err(format!(
                    "slot {slot} is leased to lease {}, not {}",
                    Dotted(holder),
                    Dotted(lease)
                ))
            }
            _ => self.admit(Record::Resetting(slot)),
        }
    }

    /// The slot's record.
    pub(super) fn get(&self, slot: Slot) -> Option<SlotEntry> {
        self.slots.get(&slot).copied()
    }

    /// Every slot made, in order.
    pub(super) fn all(&self) -> Vec<(Slot, SlotEntry)> {
        self.slots.iter().map(|(s, e)| (*s, *e)).collect()
    }

    /// The slot leased to `lease`.
    pub(super) fn leased_to(&self, lease: LeaseId) -> Option<Slot> {
        self.slots
            .iter()
            .find(|(_, entry)| matches!(entry.state, SlotState::Leased { lease: l, .. } if l == lease))
            .map(|(slot, _)| *slot)
    }

    /// Makes every later write fail, as a full or failing disk would.
    #[cfg(test)]
    pub(super) fn fail_writes(&mut self, path: &Path) {
        self.file = File::open(path).unwrap();
    }

    /// Makes every write after the next `writes` fail.
    #[cfg(test)]
    pub(super) fn fail_after(&mut self, writes: usize) {
        self.writes_left = Some(writes);
    }
}

#[cfg(test)]
mod tests;
