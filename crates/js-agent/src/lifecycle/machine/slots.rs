//! The slots: the places a run can hold, each with a container user of its own.
//!
//! A slot is a number from 0. Its container uid and gid are the base ids of
//! `--container-user` plus the number (see
//! [`Settings::slot_ids`](crate::cli_runtime::Settings::slot_ids)), so two runs held at
//! once never share a host uid, and a host uid is used by one run at a time.
//!
//! A slot is **held** from the start of its run until the cleanup of that run has passed:
//! the container and the scratch directory are gone and the post-job check found no
//! process left (see the cleanup in the [module](super)). Only then is it free for another
//! run, which gets the same uid again. A slot whose check found something, or whose
//! container could not be removed, is **quarantined**: it is never free again in this
//! process. (The host is tainted then, so no run starts either way; the quarantine keeps
//! the rule true by itself.)
//!
//! Under a scratch quota a slot is also **parked** while its project id still holds usage
//! (or the usage cannot be read): no run takes it, and it is free again once the usage reads
//! zero. Unlike a quarantine this ends by itself.

use std::collections::BTreeSet;

/// The slots of one agent.
pub struct Slots {
    count: u32,
    held: BTreeSet<u32>,
    quarantined: BTreeSet<u32>,
    parked: BTreeSet<u32>,
}

impl Slots {
    /// `count` slots, all free.
    pub fn new(count: u32) -> Self {
        Self {
            count,
            held: BTreeSet::new(),
            quarantined: BTreeSet::new(),
            parked: BTreeSet::new(),
        }
    }

    /// The lowest slot that is free.
    fn lowest(&self) -> Option<u32> {
        (0..self.count).find(|slot| self.is_free(*slot))
    }

    /// Whether a slot is free to take.
    pub fn has_room(&self) -> bool {
        self.lowest().is_some()
    }

    /// Whether `slot` is one of the slots and neither held, quarantined nor parked.
    pub fn is_free(&self, slot: u32) -> bool {
        slot < self.count
            && !self.held.contains(&slot)
            && !self.quarantined.contains(&slot)
            && !self.parked.contains(&slot)
    }

    /// Takes the lowest free slot. Call it after [`Slots::has_room`] said there is one;
    /// with none free it returns the number one past the last slot, which no container
    /// user exists for.
    pub fn take(&mut self) -> u32 {
        let slot = self.lowest().unwrap_or(self.count);
        self.held.insert(slot);
        slot
    }

    /// Holds `slot` for a run taken over after a restart; `false` (and nothing held) when
    /// the slot is not free.
    pub fn hold(&mut self, slot: u32) -> bool {
        let free = self.is_free(slot);
        if free {
            self.held.insert(slot);
        }
        free
    }

    /// Frees `slot`: the cleanup of its run passed. A quarantined slot stays quarantined.
    pub fn release(&mut self, slot: u32) {
        self.held.remove(&slot);
    }

    /// Takes `slot` out of use for good, held or not.
    pub fn quarantine(&mut self, slot: u32) {
        self.held.remove(&slot);
        self.quarantined.insert(slot);
    }

    /// Parks `slot`: no run takes it until [`Slots::unpark`]. A quarantined slot stays
    /// quarantined.
    pub fn park(&mut self, slot: u32) {
        self.held.remove(&slot);
        self.parked.insert(slot);
    }

    /// Ends the parking of `slot`; it is free again unless it is held or quarantined.
    pub fn unpark(&mut self, slot: u32) {
        self.parked.remove(&slot);
    }

    /// The parked slots, lowest first.
    pub fn parked(&self) -> Vec<u32> {
        self.parked.iter().copied().collect()
    }

    /// The held slots, lowest first.
    pub fn held(&self) -> Vec<u32> {
        self.held.iter().copied().collect()
    }

    /// The quarantined slots, lowest first.
    pub fn quarantined(&self) -> Vec<u32> {
        self.quarantined.iter().copied().collect()
    }
}
