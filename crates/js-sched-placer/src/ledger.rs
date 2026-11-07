//! What the engine keeps of the registry's whole-node requests across a restart under
//! `--placement js-sched`: it hands the registry's reservation ledger (design
//! `whole-node.md`, D55) the give-ups, back-offs, failed prepares and bars of its requests
//! ([`Engine::whole_ledger`]) and the marks they hold ([`Engine::whole_reservations`]), and
//! takes them back from the ledger a restart read ([`Engine::whole_restore`]).
//!
//! - **Out.** A request the engine queues gives its give-ups and the bars the scheduler
//!   keeps for it (the back-offs of its give-ups and the bars of its failed prepares); a
//!   request whose prepare failed, or whose lease is committed and not yet ready, gives the
//!   failures, give-ups and bars [`super::prepare`] keeps; a request put back and not
//!   queued since gives what it was put back with. Bars that have ended are left out. Each
//!   count is the largest of these, and each worker keeps the longest bar.
//! - **In.** A record the registry put back is kept until its request is presented again:
//!   its give-ups and bars wait beside those a rebuild carries ([`Engine::carry_whole`]),
//!   and the request is queued with them, from when it was first presented, through
//!   [`js_sched::Scheduler::restore_whole`]. A timed exclusion of a record with no failed
//!   prepare is a give-up's back-off, a soft bar; any other is a failed prepare's hard bar.
//!   A record with failed prepares is a prepare record at once ([`super::prepare`]), so a
//!   terminal request is refused at its next start whether or not it is queued first. What
//!   is put back and not queued within the registry's idle horizon goes
//!   ([`Engine::whole_sweep`]), as the registry's own records do.
//!
//! The place of a request put back and its mark are not taken here: the wait list keeps them
//! ([`super::wait`]) until the request is presented again and its mark's host has said
//! hello, and hands the mark to the queue then with the request; until then the engine holds
//! the host outside its queue ([`js_registry::WholeRound::outside`]).

use std::collections::BTreeMap;

use js_lease::Timestamp;
use js_registry::{Exclusion, Reservation, WholeKept};
use js_sched::Bar;
use js_types::RequestId;
use js_types::sched::{FarmTime, WorkerId};

use super::Engine;
use super::prepare::{exclusion_of, stronger};
use super::whole::farm;
use crate::map;

/// The counts and bars of one request as the ledger is gathered: give-ups, failed prepares
/// and the bars by worker.
type Gathered = (u32, u32, BTreeMap<WorkerId, Bar>);

impl Engine {
    /// What the engine keeps of each whole-node request for the registry's ledger at `now`,
    /// those it keeps anything of (see the module documentation).
    pub fn whole_ledger(&self, now: FarmTime) -> BTreeMap<RequestId, WholeKept> {
        let mut gathered: BTreeMap<RequestId, Gathered> = BTreeMap::new();
        let mut take =
            |id: &RequestId, give_ups: u32, failures: u32, bars: &BTreeMap<WorkerId, Bar>| {
                let entry = gathered.entry(id.clone()).or_default();
                entry.0 = entry.0.max(give_ups);
                entry.1 = entry.1.max(failures);
                for (worker, bar) in bars {
                    let kept = entry.2.get(worker).copied();
                    entry.2.insert(worker.clone(), stronger(kept, *bar));
                }
            };
        for (id, operation) in &self.whole {
            let give_ups = self.sched.whole_give_ups(*operation);
            take(id, give_ups, 0, &self.sched.whole_barred(*operation, now));
        }
        for (id, (give_ups, bars)) in &self.carried {
            take(id, *give_ups, 0, bars);
        }
        for (id, failures, give_ups, bars) in self.prepare.ledger() {
            take(id, give_ups, failures, bars);
        }
        let kept = gathered
            .into_iter()
            .map(|(id, (give_ups, failures, bars))| {
                let standing = bars.into_iter().filter(|(_, bar)| holds(*bar, now));
                let exclusions = standing.filter_map(|(worker, bar)| {
                    Some((map::host_id(&worker).ok()?, exclusion_of(bar)))
                });
                let kept = WholeKept {
                    give_ups,
                    prepare_failures: failures,
                    exclusions: exclusions.collect(),
                };
                (id, kept)
            });
        kept.filter(|(_, kept)| !kept.is_empty()).collect()
    }

    /// The hosts the requests the engine queues hold at `now`, as the registry shows them:
    /// a pending grant as an assigned reservation, a mark with its deadline and tail.
    pub fn whole_reservations(&self, now: FarmTime) -> Vec<Reservation> {
        self.reservations(now)
    }

    /// Puts back at `now` what a restart's ledger kept of request `id` (see the module
    /// documentation): `kept`, whether its failed prepares made it terminal, and `since`,
    /// when it was first presented.
    pub fn whole_restore(
        &mut self,
        id: &RequestId,
        kept: &WholeKept,
        (terminal, since): (bool, Timestamp),
        now: FarmTime,
    ) {
        let failed = kept.prepare_failures > 0;
        let bars: BTreeMap<WorkerId, Bar> = kept
            .exclusions
            .iter()
            .map(|(host, exclusion)| (map::worker(host), bar_of(*exclusion, failed)))
            .collect();
        if failed {
            let counts = (kept.prepare_failures, kept.give_ups, bars.clone());
            self.prepare
                .restore(id, counts, (terminal, farm(since)), now);
        }
        self.carried.insert(id.clone(), (kept.give_ups, bars));
        self.restored.insert(id.clone(), now);
    }
}

/// The bar a restored `exclusion` is: a give-up's back-off when the record has no failed
/// prepare (`failed`) and the exclusion ends, else a hard bar.
fn bar_of(exclusion: Exclusion, failed: bool) -> Bar {
    let end = |at: Timestamp| FarmTime::from_millis(at.millis());
    match (exclusion, failed) {
        (Exclusion::Until(at), false) => Bar::Backoff(end(at)),
        (Exclusion::Until(at), true) => Bar::Until(end(at)),
        (Exclusion::ForLife, _) => Bar::ForLife,
    }
}

/// Whether `bar` still holds at `now`.
fn holds(bar: Bar, now: FarmTime) -> bool {
    match bar {
        Bar::Backoff(end) | Bar::Until(end) => now < end,
        Bar::ForLife => true,
    }
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
