//! What the provider remembers of the instances it holds, for the calls that answer with
//! no request to the API: how many it supplies and could add, and which of them a first
//! hello comes from. It is kept from what the provider last listed, launched, released and
//! read by id, so it can lag the account; it never decides a launch (the launch guard
//! reads a fresh list for that).

use std::collections::BTreeMap;

use js_elastic::{Capacity, Claim, Held, HostName, HostRef, HostStatus, Observed, Released};

/// The instances the provider knows of, by what told it.
#[derive(Default)]
pub(super) struct Records {
    /// Ours, as the last list showed them.
    listed: BTreeMap<HostRef, Held>,
    /// The instances a launch was answered with that no list had shown as ours when the
    /// guard last settled: the launch guard's own memory, copied.
    answered: BTreeMap<HostRef, Held>,
    /// The rows of the last list with some of our marks, not all, that are not terminated.
    /// They count toward `max_machines`.
    suspects: usize,
    /// What else counts toward `max_machines` with no instance of ours to show for it:
    /// launches sent with no answer yet, and the extra instances a launch started.
    pending: usize,
}

impl Records {
    /// Every instance held, by id: the answered ones, then the listed ones over them (a
    /// list is the later word on the status).
    fn machines(&self) -> BTreeMap<&HostRef, &Held> {
        let mut all: BTreeMap<&HostRef, &Held> = self.answered.iter().collect();
        all.extend(&self.listed);
        all
    }

    /// A list worked and showed `rows`, each with the claim its marks make.
    pub(super) fn listed(&mut self, rows: impl IntoIterator<Item = (Claim, Held)>) {
        self.listed.clear();
        self.suspects = 0;
        for (claim, held) in rows {
            match claim {
                Claim::Ours => {
                    self.listed.insert(held.id.clone(), held);
                }
                Claim::Suspect => {
                    self.suspects += usize::from(held.status != HostStatus::Terminated);
                }
                Claim::Foreign => {}
            }
        }
    }

    /// The launch guard holds `answered` (the instances a launch answered with that it is
    /// the one to name) and `pending` more that count with nothing to show for them.
    pub(super) fn guarded(&mut self, answered: impl IntoIterator<Item = Held>, pending: usize) {
        self.answered = answered
            .into_iter()
            .map(|held| (held.id.clone(), held))
            .collect();
        self.pending = pending;
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

    fn set_status(&mut self, id: &HostRef, status: &HostStatus) {
        let known = self.listed.get_mut(id).into_iter();
        for held in known.chain(self.answered.get_mut(id)) {
            held.status = status.clone();
        }
    }

    fn forget(&mut self, id: &HostRef) {
        self.listed.remove(id);
        self.answered.remove(id);
    }

    /// The instances supplied (held and not ending) and how many more `max_machines`
    /// allows once every instance it counts is counted: held and not terminated, the
    /// suspects, and the pending launches.
    pub(super) fn capacity(&self, max_machines: usize) -> Capacity {
        let machines = self.machines();
        let supplied = machines.values().filter(|held| !ending(&held.status));
        let live = machines
            .values()
            .filter(|held| held.status != HostStatus::Terminated);
        let counted = live.count() + self.suspects + self.pending;
        Capacity {
            supplied: count(supplied.count()),
            can_add: Some(count(max_machines.saturating_sub(counted))),
        }
    }

    /// The held instance named `name` and, when `instance` is given, with that id.
    pub(super) fn verify(&self, name: &HostName, instance: Option<&str>) -> Option<HostRef> {
        let named = Some(name.as_str());
        let found = self.machines().into_iter().find(|(id, held)| {
            let id_fits = instance.is_none_or(|instance| instance == id.as_str());
            (held.name.as_deref() == named) & id_fits
        });
        found.map(|(id, _)| id.clone())
    }
}

/// Whether a machine in `status` is on its way out, so no longer supplied.
fn ending(status: &HostStatus) -> bool {
    matches!(
        status,
        HostStatus::Terminating | HostStatus::Terminated | HostStatus::Preempted
    )
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
