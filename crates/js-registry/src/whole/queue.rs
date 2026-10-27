//! The wait list of whole-node requests that found no free host: accepted, not refused,
//! and kept in the order they were first seen (design `whole-node.md` sections 2 and 3).
//!
//! It is the smallest queue the registry needs until a scheduler owns queue order, and
//! it keeps nothing but the order and two instants per request. It is in memory only: a
//! consumer presents the same request id again after a restart, as it presents it on
//! every poll. A poll is how the list knows the consumer is alive, so an entry that is
//! not presented again within `regain_grace` is dropped, and a cancel removes it at once.

use js_lease::Timestamp;
use js_types::{Request, RequestId};

/// One waiting request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    id: RequestId,
    /// What the request asks for, as first presented: the reservation reads its fit and
    /// its wall time from here.
    request: Request,
    /// The first presentation: how long the request has waited counts from here.
    since: Timestamp,
    /// The last time the consumer presented the request.
    seen: Timestamp,
    /// The first presentation at which no host could ever be given whole; `None` while
    /// some host could.
    unservable_since: Option<Timestamp>,
}

/// How the list answers a presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The request waits, at this place from the front (zero is next).
    Waiting(usize),
    /// The request has no host that could ever serve it and has waited out the grace
    /// since it was last seen to have one, or never had a place: it is refused and not
    /// kept.
    Unservable,
}

/// The requests that wait for a host, in first-seen order.
#[derive(Debug, Default)]
pub(crate) struct WaitList {
    entries: Vec<Entry>,
}

impl WaitList {
    /// Drops every entry not presented for `grace_ms` or more at `now`.
    pub(crate) fn expire(&mut self, now: Timestamp, grace_ms: u64) {
        self.entries
            .retain(|entry| now.millis().saturating_sub(entry.seen.millis()) < grace_ms);
    }

    /// The consumer presents request `id` at `now`. `capable` says whether some host
    /// could ever be given whole to it.
    ///
    /// A request not on the list enters at the back if `capable`, keeping `request` for
    /// the reservation, and is `Unservable` at once if not. A request already on the list keeps its place, whatever the
    /// number of presentations, and is seen again at `now`; while no host is capable it
    /// keeps waiting until `grace_ms` have passed since the first presentation that saw
    /// none, then it is `Unservable` and leaves the list.
    pub(crate) fn present(
        &mut self,
        id: &RequestId,
        request: &Request,
        capable: bool,
        now: Timestamp,
        grace_ms: u64,
    ) -> Verdict {
        let at = self.entries.iter().position(|entry| entry.id == *id);
        let Some(at) = at else {
            return match capable {
                true => {
                    self.entries.push(Entry {
                        id: id.clone(),
                        request: request.clone(),
                        since: now,
                        seen: now,
                        unservable_since: None,
                    });
                    Verdict::Waiting(self.entries.len() - 1)
                }
                false => Verdict::Unservable,
            };
        };
        let entry = &mut self.entries[at];
        entry.seen = now;
        entry.unservable_since = match capable {
            true => None,
            false => Some(entry.unservable_since.unwrap_or(now)),
        };
        let spent = entry
            .unservable_since
            .is_some_and(|since| now.millis().saturating_sub(since.millis()) >= grace_ms);
        match spent {
            true => {
                self.entries.remove(at);
                Verdict::Unservable
            }
            false => Verdict::Waiting(at),
        }
    }

    /// Removes request `id` at once; whether it was waiting.
    pub(crate) fn cancel(&mut self, id: &RequestId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != *id);
        self.entries.len() < before
    }

    /// The waiting requests, front first: the id, what it asks for and when it was first
    /// presented.
    pub(crate) fn waiting(&self) -> impl Iterator<Item = (&RequestId, &Request, Timestamp)> {
        let waiting = self.entries.iter();
        waiting.map(|entry| (&entry.id, &entry.request, entry.since))
    }

    /// Whether no request is waiting.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether request `id` is waiting.
    pub(crate) fn holds(&self, id: &RequestId) -> bool {
        self.position(id).is_some()
    }

    /// Where request `id` waits from the front, if it does.
    pub(crate) fn position(&self, id: &RequestId) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == *id)
    }
}
