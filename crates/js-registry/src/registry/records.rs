//! What the registry answers of a whole-node request's record ([`Registry::request_record`],
//! [`Registry::not_ready`]): its own, or the placer's when the placer keeps the request
//! ([`crate::Placer::whole_record`], the engine of `--placement js-sched`).

use super::Registry;
use crate::whole::requests::{NotReady, RequestKey, RequestRecord};

impl Registry {
    /// The record of a whole-node request: its failed prepares, the hosts left out of its
    /// placements and the last failure. `None` for a request that has failed none, or
    /// was placed since. Ask by the request id (`&RequestId`); a request whose start
    /// carried no id is kept under its content (`&Request`), so equal requests share a
    /// record. A request with an id that a placer keeps (the engine of `--placement
    /// js-sched`) reads the placer's record, which it counts in place of the registry's.
    pub fn request_record(&self, key: impl Into<RequestKey>) -> Option<RequestRecord> {
        self.record_of(key.into())
    }

    /// [`Registry::request_record`], over the key itself, so that every kind of key runs
    /// the same code.
    fn record_of(&self, key: RequestKey) -> Option<RequestRecord> {
        // Locked first: the idle records are dropped, the placer's as well.
        let state = self.lock();
        let id = match &key {
            RequestKey::Id(id) => Some(id),
            RequestKey::Content(_) => None,
        };
        // A placer that keeps the records answers for every request; another one only for
        // a request with an id it keeps.
        if let Some(keeper) = self.keeper() {
            return keeper.whole_record(&key);
        }
        let placer = self.shared.placer.get();
        let kept = id
            .zip(placer)
            .and_then(|(_, placer)| placer.whole_record(&key));
        kept.or_else(|| state.requests.record(&key).cloned())
    }

    /// Why a request will not be placed: the last code and detail reported, once it has
    /// used up `max_replace` prepare failures; `None` before, and after the refusal has
    /// been delivered to a start (the record is then removed, see the `requests` module)
    /// or the record has gone idle. A start of such a request answers
    /// `Refused(ExclusiveNotReady)`, which the consumer sees as the typed
    /// `exclusive-not-ready` refusal (wire 409). Under the registry's own placement a restart
    /// puts the counts back from the ledger ([`Registry::restore_reservations`]) but not this
    /// text, so `max_replace` holds across restarts and a restored terminal request reads
    /// `None` here; a placer that keeps the request (`--placement js-sched`) writes its counts
    /// to the same ledger and takes them back, without the text, so the same holds there.
    /// Ask as [`Registry::request_record`] does.
    pub fn not_ready(&self, key: impl Into<RequestKey>) -> Option<NotReady> {
        let record = self.request_record(key);
        record
            .filter(|record| record.terminal)
            .and_then(|record| record.last)
    }
}
