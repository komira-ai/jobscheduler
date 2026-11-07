//! What the registry answers of a whole-node request's record ([`Registry::request_record`],
//! [`Registry::not_ready`]): the placer's, which keeps the request
//! ([`crate::Placer::whole_record`]).

use super::Registry;
use crate::whole::requests::{NotReady, RequestKey, RequestRecord};

impl Registry {
    /// The record of a whole-node request: its failed prepares, the hosts left out of its
    /// placements and the last failure. `None` for a request that has failed none, or
    /// was placed since. Ask by the request id (`&RequestId`); a request whose start
    /// carried no id is kept under its content (`&Request`), so equal requests share a
    /// record. The record is the placer's, which counts the failures.
    pub fn request_record(&self, key: impl Into<RequestKey>) -> Option<RequestRecord> {
        let key = key.into();
        // Locked first: the placer drops its idle records.
        let _state = self.lock();
        self.placer().whole_record(&key)
    }

    /// Why a request will not be placed: the last code and detail reported, once it has
    /// used up `max_replace` prepare failures; `None` before, and after the refusal has
    /// been delivered to a start (the record is then removed) or the record has gone idle. A
    /// start of such a request answers `Refused(ExclusiveNotReady)`, which the consumer sees
    /// as the typed `exclusive-not-ready` refusal (wire 409). The placer writes its counts to
    /// the ledger and takes them back at a start ([`Registry::restore_reservations`]), but
    /// not this text, so `max_replace` holds across restarts and a restored terminal request
    /// reads `None` here.
    /// Ask as [`Registry::request_record`] does.
    pub fn not_ready(&self, key: impl Into<RequestKey>) -> Option<NotReady> {
        let record = self.request_record(key);
        record
            .filter(|record| record.terminal)
            .and_then(|record| record.last)
    }
}
