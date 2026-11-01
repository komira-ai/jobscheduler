//! The reservation ledger: what the registry hands the lease file to keep so that a
//! restart does not lose the whole-node reservations in progress (design `whole-node.md`,
//! D55), and what it does with it at a start.
//!
//! The ledger is one [`ReservationRecord`] per request that carries an id, written from
//! the wait list, the marks and the request records:
//!
//! - the place on the wait list and the instant the request was first presented;
//! - the host set aside for it, when it was, its deadline, and whether the host was quiet
//!   (an assigned mark drains nothing);
//! - its give-up and failed-prepare counts, and the hosts it is kept off, each with the
//!   instant that ends it or for good.
//!
//! Nothing else is kept. What a host costs to drain, and every estimate of it, is read
//! from the table at each pass; the last failure's text is not kept; a request without an
//! id is told apart by its content and is forgotten by a restart, as a start without an
//! id never waits.
//!
//! [`Registry::restore_reservations`] runs once, before any host says hello, over the
//! records the lease table opened with. A record stands on its own claim: the request
//! keeps its place and its counts, and its mark its host and instants. The registry has no
//! hosts yet and the consumer has not polled yet, so both get until the pool's
//! `regain_grace` from the start: a request not presented again in that time is dropped
//! with its mark, a mark whose host does not say hello is dropped, and each is counted
//! ([`Registry::dropped_reservations`]). A record that cannot stand at once is dropped and
//! counted at the restore: a request id that is not one or is listed twice, a host that is
//! being given back, a mark with no place on the wait list or on a host another record
//! holds. Nothing it drops counts as a give-up.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_lease::{Exclusion as Barred, ExclusionEnd, Held, Queued, ReservationRecord, Timestamp};
use js_types::{HostId, RequestId};

use super::{Registry, State};
use crate::whole::requests::{Exclusion, RequestKey, RequestRecord, whole_settings};
use crate::whole::reserve::Revived;

/// What [`Registry::restore_reservations`] did with the records it was given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Restored {
    /// Records put back: a place on the wait list, counts, exclusions or a mark.
    pub resumed: usize,
    /// Records, or the marks of records, that could not stand and were dropped.
    pub dropped: usize,
}

impl Registry {
    /// The whole-node reservation state a restart must not lose, one record per request,
    /// in request id order. The lease server hands it to the lease table at each tick, to
    /// be written with the leases.
    pub fn reservation_ledger(&self) -> Vec<ReservationRecord> {
        export(&self.lock())
    }

    /// Puts back the state `records` hold, as the module documentation says. Called once
    /// at a start, before any host says hello and before the registry has seen any
    /// request. A deadline is taken as given: the lease table raises it to at least the
    /// boot plus its boot grace when it opens the file.
    pub fn restore_reservations(&self, records: &[ReservationRecord]) -> Restored {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let whole = whole_settings(&self.shared.config);
        let ctx = Standing {
            now,
            claim_by: now.plus(Duration::from_millis(whole.regain_grace_ms)),
            max_replace: whole.max_replace,
            max_reserve_attempts: whole.max_reserve_attempts,
        };
        let mut ordered: Vec<&ReservationRecord> = records.iter().collect();
        ordered.sort_by(|a, b| order(a).cmp(&order(b)));
        let mut seen = BTreeSet::new();
        let mut tally = Restored::default();
        for record in ordered {
            let one = revive(&mut guard, record, &ctx, &mut seen);
            tally.resumed += one.resumed;
            tally.dropped += one.dropped;
        }
        guard.dropped += u64::try_from(tally.dropped).unwrap_or(u64::MAX);
        tally
    }

    /// How many restored reservations were dropped since the start: a request not
    /// presented again within the regain grace, a mark whose host never said hello, and a
    /// record that could not stand at the restore.
    pub fn dropped_reservations(&self) -> u64 {
        self.lock().dropped
    }
}

/// The order records are put back in: those with a place, by place, then the rest, by
/// request id.
fn order(record: &ReservationRecord) -> (bool, Option<(u32, Timestamp)>, &str) {
    let place = record.queued.map(|queued| (queued.place, queued.since));
    (record.queued.is_none(), place, record.request_id.as_str())
}

/// The registry's side of the ledger, as records.
fn export(state: &State) -> Vec<ReservationRecord> {
    let marks = state.marks.reservations();
    let mut rows: BTreeMap<RequestId, ReservationRecord> = BTreeMap::new();
    for (place, (id, since)) in state.waiting.queue().enumerate() {
        let mark = marks.iter().find(|mark| mark.request == *id);
        let held = mark.map(|mark| Held {
            host: mark.host.clone(),
            reserved_at: mark.at,
            deadline: mark.deadline,
            assigned: mark.assigned,
        });
        let queued = Queued {
            place: u32::try_from(place).unwrap_or(u32::MAX),
            since,
        };
        let row = ReservationRecord {
            held,
            queued: Some(queued),
            ..empty(id)
        };
        rows.insert(id.clone(), row);
    }
    for (id, record) in state.requests.recorded() {
        let row = rows.entry(id.clone()).or_insert_with(|| empty(id));
        row.give_ups = record.give_ups;
        row.prepare_failures = record.prepare_failures;
        row.exclusions = record.exclusions.iter().map(barred).collect();
    }
    rows.into_values().collect()
}

fn empty(id: &RequestId) -> ReservationRecord {
    ReservationRecord {
        request_id: id.to_string(),
        held: None,
        queued: None,
        give_ups: 0,
        prepare_failures: 0,
        exclusions: Vec::new(),
        claim_by: None,
    }
}

fn barred((host, exclusion): (&HostId, &Exclusion)) -> Barred {
    let until = match exclusion {
        Exclusion::Until(at) => ExclusionEnd::At(*at),
        Exclusion::ForLife => ExclusionEnd::Forever,
    };
    Barred {
        host: host.clone(),
        until,
    }
}

fn exclusion(barred: &Barred) -> (HostId, Exclusion) {
    let exclusion = match barred.until {
        ExclusionEnd::At(at) => Exclusion::Until(at),
        ExclusionEnd::Forever => Exclusion::ForLife,
    };
    (barred.host.clone(), exclusion)
}

/// What a restore reads besides the records.
struct Standing {
    now: Timestamp,
    claim_by: Timestamp,
    max_replace: u32,
    max_reserve_attempts: u32,
}

/// Puts back one record, if it can stand; what came of it. `seen` holds the request ids
/// already put back, so a request is restored once.
fn revive(
    state: &mut State,
    record: &ReservationRecord,
    ctx: &Standing,
    seen: &mut BTreeSet<RequestId>,
) -> Restored {
    let id = RequestId::new(&record.request_id).ok();
    let id = id.filter(|id| seen.insert(id.clone()));
    let Some(id) = id else {
        return Restored {
            resumed: 0,
            dropped: 1,
        };
    };
    let counted = [
        record.give_ups > 0,
        record.prepare_failures > 0,
        !record.exclusions.is_empty(),
    ];
    let counted = counted.into_iter().any(|counted| counted);
    if counted {
        let failures = record.prepare_failures;
        let used_up = [failures > 0, failures >= ctx.max_replace];
        let kept = RequestRecord {
            prepare_failures: failures,
            give_ups: record.give_ups,
            exclusions: record.exclusions.iter().map(exclusion).collect(),
            last: None,
            terminal: used_up.into_iter().all(|used| used),
        };
        let key = RequestKey::Id(id.clone());
        state.requests.restore(key, kept, ctx.now);
    }
    let placed = record.queued.is_some();
    if let Some(queued) = record.queued {
        state.waiting.restore(id.clone(), queued.since, ctx.now);
    }
    let mut marked = false;
    let mut dropped = 0;
    if let Some(held) = &record.held {
        let leaving = state.ending.contains(&held.host);
        let standing = [placed, !leaving].into_iter().all(|ok| ok);
        let revived = Revived {
            request: id,
            host: held.host.clone(),
            at: held.reserved_at,
            deadline: held.deadline,
            assigned: held.assigned,
            capped: record.give_ups >= ctx.max_reserve_attempts,
            claim_by: ctx.claim_by,
        };
        marked = standing && state.marks.revive(revived);
        dropped = usize::from(!marked);
    }
    let resumed = [counted, placed, marked].into_iter().any(|kept| kept);
    Restored {
        resumed: usize::from(resumed),
        dropped,
    }
}
