//! What the provider remembers of the instances it holds, for the calls that answer with
//! no request to the cloud: how many it supplies and could add, which of them a first
//! hello comes from and with which image, and the architectures and accelerator models its
//! catalog has carried. It is kept from what the provider last listed, launched, released
//! and read by id, so it can lag the account; it never decides a launch (the launch guard
//! reads a fresh list of the acquire for that).
//!
//! Every instance is kept by its id, so one instance is counted once whichever source
//! named it. A list is the account's word: it replaces what it shows, and keeps the
//! instances this provider launched that it does not show yet (a read after a write can
//! lag it). A read that shows only some instances, such as the lookup of one acquire,
//! only adds and updates.

use std::collections::{BTreeMap, BTreeSet};

use js_elastic::{Capacity, Claim, Held, HostName, HostRef, HostStatus, Observed, Offer, Released};
use js_types::Model;

use crate::launch::ImageId;

/// The instances the provider knows of.
#[derive(Default)]
pub(super) struct Records {
    /// Ours, by id, as the latest list, launch answer, release or read left them.
    held: BTreeMap<HostRef, Held>,
    /// The image each held instance was launched from, by id.
    images: BTreeMap<HostRef, ImageId>,
    /// The instances a launch answered with that no read has shown yet: a list that does
    /// not show them keeps them.
    pending: BTreeSet<HostRef>,
    /// The instances with some of our marks, not all, by id, with their status. They count
    /// toward `max_machines` until they are terminated or gone.
    suspects: BTreeMap<HostRef, HostStatus>,
    /// The acquire ids of launches sent with no answer.
    unresolved: BTreeSet<String>,
    /// The architectures the catalog has carried.
    arches: BTreeSet<String>,
    /// The accelerator models the catalog has carried.
    models: BTreeSet<Model>,
}

impl Records {
    /// A read showed `rows`, each with the claim its marks make; it may show only some of
    /// the account's instances, so nothing it does not show is dropped.
    pub(super) fn seen(&mut self, rows: impl IntoIterator<Item = (Claim, Held, ImageId)>) {
        for (claim, row, image) in rows {
            self.pending.remove(&row.id);
            match claim {
                Claim::Ours => {
                    self.suspects.remove(&row.id);
                    self.images.insert(row.id.clone(), image);
                    self.held.insert(row.id.clone(), row);
                }
                Claim::Suspect => {
                    self.held.remove(&row.id);
                    self.suspects.insert(row.id, row.status);
                }
                Claim::Foreign => {}
            }
        }
    }

    /// A list worked and showed `rows`: they replace what is held, except the instances a
    /// launch answered with that the list does not show yet.
    pub(super) fn listed(&mut self, rows: impl IntoIterator<Item = (Claim, Held, ImageId)>) {
        let kept: Vec<Held> = self
            .pending
            .iter()
            .filter_map(|id| self.held.get(id))
            .cloned()
            .collect();
        self.held.clear();
        self.suspects.clear();
        self.seen(rows);
        let unlisted: Vec<Held> = kept
            .into_iter()
            .filter(|row| self.pending.contains(&row.id))
            .collect();
        for row in unlisted {
            self.held.entry(row.id.clone()).or_insert(row);
        }
    }

    /// A launch answered with the instance `held`, made from `image`.
    pub(super) fn launched(&mut self, held: Held, image: ImageId) {
        self.pending.insert(held.id.clone());
        self.images.insert(held.id.clone(), image);
        self.held.insert(held.id.clone(), held);
    }

    /// The launches sent with no answer are those of `acquires`.
    pub(super) fn unresolved(&mut self, acquires: BTreeSet<String>) {
        self.unresolved = acquires;
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
        self.models.extend(
            offers
                .iter()
                .filter_map(|offer| offer.accelerator.as_ref())
                .map(|accelerator| accelerator.model.clone()),
        );
    }

    /// The architectures the catalog has carried.
    pub(super) fn arches(&self) -> BTreeSet<String> {
        self.arches.clone()
    }

    /// The accelerator models the catalog has carried.
    pub(super) fn models(&self) -> BTreeSet<Model> {
        self.models.clone()
    }

    fn set_status(&mut self, id: &HostRef, status: &HostStatus) {
        let held = self.held.get_mut(id).into_iter();
        held.for_each(|row| row.status = status.clone());
        let suspect = self.suspects.get_mut(id).into_iter();
        suspect.for_each(|known| *known = status.clone());
    }

    fn forget(&mut self, id: &HostRef) {
        self.held.remove(id);
        self.images.remove(id);
        self.pending.remove(id);
        self.suspects.remove(id);
    }

    /// How many instances count toward `max_machines`, each once: ours and not
    /// terminated, the suspects that are not terminated, and the launches with no answer
    /// that no held instance of their acquire id accounts for.
    pub(super) fn counted(&self) -> usize {
        let live = self
            .held
            .values()
            .filter(|row| row.status != HostStatus::Terminated)
            .count();
        let suspects = self
            .suspects
            .values()
            .filter(|status| **status != HostStatus::Terminated)
            .count();
        let unanswered = self
            .unresolved
            .iter()
            .filter(|acquire| !self.held.values().any(|row| row.acquire == **acquire))
            .count();
        live + suspects + unanswered
    }

    /// The instances supplied (held and not ending) and how many more `max_machines`
    /// allows once every instance it counts is counted once.
    pub(super) fn capacity(&self, max_machines: usize) -> Capacity {
        let supplied = self
            .held
            .values()
            .filter(|row| !ending(&row.status))
            .count();
        Capacity {
            supplied: count(supplied),
            can_add: Some(count(max_machines.saturating_sub(self.counted()))),
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

    /// The image the held instance `id` was launched from.
    pub(super) fn image_of(&self, id: &HostRef) -> Option<&ImageId> {
        self.images.get(id)
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
