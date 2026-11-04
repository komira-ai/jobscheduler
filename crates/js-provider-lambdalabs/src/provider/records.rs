//! What the provider remembers of the instances it holds, for the calls that answer with
//! no request to the API: how many it supplies and could add, which of them a first hello
//! comes from, and the architectures its catalog has carried. It is kept from what the
//! provider last listed, launched, released and read by id, so it can lag the account; it
//! never decides a launch (the launch guard reads a fresh list for that).
//!
//! Every instance is kept by its id, so one instance is counted once whichever source
//! named it. A launch answer only adds an instance the records do not know: it never
//! brings back an older status over a later release or read, and never brings back an
//! instance a read or a release found gone. A list is the account's word: it replaces
//! what it shows (and clears a gone mark for an instance it shows), and keeps the
//! instances the launch guard holds that it does not show yet.

use std::collections::{BTreeMap, BTreeSet};

use js_elastic::{
    Capacity, Claim, Extra, Held, HostName, HostRef, HostStatus, Observed, Offer, Released,
};

/// The instances the provider knows of.
#[derive(Default)]
pub(super) struct Records {
    /// Ours, by id, as the latest list, launch answer, release or read by id left them.
    held: BTreeMap<HostRef, Held>,
    /// The ids a read by id or a release found gone.
    gone: BTreeSet<HostRef>,
    /// The instances the launch guard holds that no list had shown as ours when it last
    /// settled: a list that does not show them yet keeps them.
    answered: BTreeSet<HostRef>,
    /// The instances with some of our marks, not all, by id, with their status. They count
    /// toward `max_machines` until they are terminated or gone.
    suspects: BTreeMap<HostRef, HostStatus>,
    /// The acquire ids of launches sent with no answer yet.
    unresolved: BTreeSet<String>,
    /// The architectures the catalog has carried.
    arches: BTreeSet<String>,
}

impl Records {
    /// A list worked and showed `rows`, each with the claim its marks make.
    pub(super) fn listed(&mut self, rows: impl IntoIterator<Item = (Claim, Held)>) {
        let mut held = BTreeMap::new();
        let mut suspects = BTreeMap::new();
        for (claim, row) in rows {
            match claim {
                Claim::Ours => {
                    self.gone.remove(&row.id);
                    held.insert(row.id.clone(), row);
                }
                Claim::Suspect => {
                    suspects.insert(row.id.clone(), row.status);
                }
                Claim::Foreign => {}
            }
        }
        let unlisted = self.answered.iter().filter_map(|id| self.held.get(id));
        for row in unlisted {
            held.entry(row.id.clone()).or_insert_with(|| row.clone());
        }
        self.held = held;
        self.suspects = suspects;
    }

    /// After an acquire the launch guard holds `answered` (every instance a launch answered
    /// with that no list has shown as ours) and `unresolved` (the acquire ids of launches
    /// with no answer); `extras` are the instances the acquire's own answer released.
    pub(super) fn guarded(
        &mut self,
        answered: impl IntoIterator<Item = Held>,
        unresolved: impl IntoIterator<Item = String>,
        extras: &[Extra],
    ) {
        self.answered.clear();
        for row in answered {
            self.answered.insert(row.id.clone());
            let known = self.gone.contains(&row.id);
            let new = (!known).then_some(row);
            new.into_iter().for_each(|row| {
                self.held.entry(row.id.clone()).or_insert(row);
            });
        }
        self.unresolved = unresolved.into_iter().collect();
        for extra in extras {
            self.released(&extra.id, &extra.released);
        }
    }

    /// A release of `id` read back `released`.
    pub(super) fn released(&mut self, id: &HostRef, released: &Released) {
        match released {
            Released::Terminating(status) => self.set_status(id, status),
            Released::Gone => self.forget(id),
            Released::Refused(_) | Released::Unknown => {}
        }
    }

    /// A read of `id` by id found `observed`.
    pub(super) fn observed(&mut self, id: &HostRef, observed: &Observed) {
        match observed {
            Observed::Present(status) => self.set_status(id, status),
            Observed::Absent => self.forget(id),
            Observed::Unknown => {}
        }
    }

    /// The catalog carried `offers`.
    pub(super) fn offered(&mut self, offers: &[Offer]) {
        self.arches
            .extend(offers.iter().map(|offer| offer.arch.clone()));
    }

    /// The architectures the catalog has carried.
    pub(super) fn arches(&self) -> BTreeSet<String> {
        self.arches.clone()
    }

    fn set_status(&mut self, id: &HostRef, status: &HostStatus) {
        let held = self.held.get_mut(id).into_iter();
        held.for_each(|row| row.status = status.clone());
        let suspect = self.suspects.get_mut(id).into_iter();
        suspect.for_each(|known| *known = status.clone());
    }

    fn forget(&mut self, id: &HostRef) {
        self.held.remove(id);
        self.suspects.remove(id);
        self.gone.insert(id.clone());
    }

    /// The instances supplied (held and not ending) and how many more `max_machines`
    /// allows once every instance it counts is counted once: ours and not terminated, the
    /// suspects that are not terminated, and the launches with no answer that no held
    /// instance carries the acquire id of.
    pub(super) fn capacity(&self, max_machines: usize) -> Capacity {
        let rows = self.held.values();
        let supplied = rows.clone().filter(|row| !ending(&row.status)).count();
        let live = rows
            .clone()
            .filter(|row| row.status != HostStatus::Terminated)
            .count();
        let suspects = self.suspects.values();
        let suspects = suspects.filter(|status| **status != HostStatus::Terminated);
        let acquires: BTreeSet<&str> = rows.map(|row| row.acquire.as_str()).collect();
        let pending = self.unresolved.iter();
        let unanswered = pending.filter(|acquire| !acquires.contains(acquire.as_str()));
        let counted = live + suspects.count() + unanswered.count();
        Capacity {
            supplied: count(supplied),
            can_add: Some(count(max_machines.saturating_sub(counted))),
        }
    }

    /// The instance a hello named `name` comes from. With `instance`, the held instance of
    /// that name and id. Without it, the one held instance of that name that is not ending;
    /// `None` when there is none, or more than one (a hello by name alone cannot tell them
    /// apart).
    pub(super) fn verify(&self, name: &HostName, instance: Option<&str>) -> Option<HostRef> {
        let named = Some(name.as_str());
        let mut rows = self
            .held
            .values()
            .filter(|row| row.name.as_deref() == named);
        let found = match instance {
            Some(instance) => rows.find(|row| row.id.as_str() == instance),
            None => {
                let live: Vec<&Held> = rows.filter(|row| !ending(&row.status)).collect();
                live.first().filter(|_| live.len() == 1).copied()
            }
        };
        found.map(|row| row.id.clone())
    }
}

/// Whether a machine in `status` is on its way out, so no longer supplied and not one a
/// first hello comes from.
fn ending(status: &HostStatus) -> bool {
    matches!(
        status,
        HostStatus::Terminating | HostStatus::Terminated | HostStatus::Preempted
    )
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
