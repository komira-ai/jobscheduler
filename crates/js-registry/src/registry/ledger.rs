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
//! holds, a place past the wait list's size cap ([`MAX_WAITING_REQUESTS`], the live path's
//! own), and exclusions past the pool's machine limit (the first hosts by id are kept).
//! Nothing it drops counts as a give-up.
//!
//! # Under a placer that keeps the queue
//!
//! Under `--placement js-sched` the engine keeps the marks of the requests it queues, their
//! give-ups and back-offs, and the failed prepares and bars of requests it committed
//! ([`crate::Placer::whole_round`] and its siblings). The ledger is then written from the
//! engine: a request's mark is the engine's ([`crate::Placer::whole_reservations`]) and its
//! counts and exclusions are the engine's ([`crate::Placer::whole_ledger`]); the place and
//! the first-presentation instant are its wait list's ([`crate::Placer::whole_places`]). The
//! format does not change: a file written under one placement is read under the other.
//!
//! A restore checks the records as above and hands what stands of each request's counts and
//! exclusions to the engine ([`crate::Placer::whole_restore`]), which queues the request with
//! them when it is presented again (`js_sched::Scheduler::restore_whole`) and answers a
//! terminal request at its next start. A timed exclusion of a record with no failed prepare
//! can only be a give-up's back-off, and is put back as one; any other is a failed
//! prepare's bar. The place goes back on the engine's wait list
//! ([`crate::Placer::whole_restore_place`]) and the mark to the engine
//! ([`crate::Placer::whole_revive`]), which holds the host for the request outside its queue
//! until the request is presented again and its host has said hello, or the mark is dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_lease::{Exclusion as Barred, ExclusionEnd, Held, Queued, ReservationRecord, Timestamp};
use js_types::{HostId, RequestId};

use super::{Registry, State};
use crate::placer::{Placer, WholeKept};
use crate::whole::queue::MAX_WAITING_REQUESTS;
use crate::whole::requests::{Exclusion, RequestKey, RequestRecord, whole_settings};
use crate::whole::reserve::Reservation;
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
        let state = self.lock();
        let placer = self.shared.placer.get().map(|placer| &**placer);
        let marks = self.marks_now(&state);
        let kept = placer.map(Placer::whole_ledger).unwrap_or_default();
        // The wait list's places, and the registry's own records, are the placer's when it
        // keeps them.
        let (places, own) = match self.keeper() {
            Some(keeper) => (keeper.whole_places(), Vec::new()),
            None => {
                let places = state.waiting.queue();
                let places = places.map(|(id, since)| (id.clone(), since));
                let own = state.requests.recorded().map(|(id, record)| {
                    let kept = WholeKept {
                        give_ups: record.give_ups,
                        prepare_failures: record.prepare_failures,
                        exclusions: record.exclusions.clone(),
                    };
                    (id.clone(), kept)
                });
                (places.collect(), own.collect())
            }
        };
        export(&places, &marks, own.into_iter().chain(kept))
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
            max_exclusions: self.shared.config.max_machines(),
            placer: self.shared.placer.get().map(|placer| &**placer),
            keeper: self.keeper(),
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

/// The ledger as records: the wait list's places (`places`, front first), the hosts set
/// aside (`marks`), and the counts and exclusions of the registry's records and of what a
/// placer that keeps the queue keeps (`kept`), the larger of each count and the longer of
/// each exclusion.
fn export(
    places: &[(RequestId, Timestamp)],
    marks: &[Reservation],
    kept: impl Iterator<Item = (RequestId, WholeKept)>,
) -> Vec<ReservationRecord> {
    let mut rows: BTreeMap<RequestId, ReservationRecord> = BTreeMap::new();
    for (place, (id, since)) in places.iter().enumerate() {
        let mark = marks.iter().find(|mark| mark.request == *id);
        let held = mark.map(|mark| Held {
            host: mark.host.clone(),
            reserved_at: mark.at,
            deadline: mark.deadline,
            assigned: mark.assigned,
            tail_end: mark.tail_end,
        });
        let queued = Queued {
            place: u32::try_from(place).unwrap_or(u32::MAX),
            since: *since,
        };
        let row = ReservationRecord {
            held,
            queued: Some(queued),
            ..empty(id)
        };
        rows.insert(id.clone(), row);
    }
    for (id, kept) in kept {
        let row = rows.entry(id.clone()).or_insert_with(|| empty(&id));
        row.give_ups = row.give_ups.max(kept.give_ups);
        row.prepare_failures = row.prepare_failures.max(kept.prepare_failures);
        let mut exclusions: BTreeMap<HostId, Exclusion> =
            row.exclusions.iter().map(exclusion).collect();
        for (host, until) in kept.exclusions {
            // The longer of the two: `None` is less than any exclusion.
            let had = exclusions.get(&host).copied();
            exclusions.insert(host, had.max(Some(until)).unwrap_or(until));
        }
        row.exclusions = exclusions.iter().map(barred).collect();
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
struct Standing<'a> {
    now: Timestamp,
    claim_by: Timestamp,
    max_replace: u32,
    max_reserve_attempts: u32,
    /// The most hosts one request is kept off: a request is kept off a host at most once,
    /// so the pool's machine limit bounds it.
    max_exclusions: usize,
    /// The placer, which keeps the counts and exclusions when it keeps the queue
    /// ([`Placer::whole_restore`]).
    placer: Option<&'a dyn Placer>,
    /// The placer when it keeps the wait list as well ([`Placer::keeps_wait_list`]): it
    /// takes the places and the marks back too.
    keeper: Option<&'a dyn Placer>,
}

/// Puts back one record, if it can stand; what came of it. `seen` holds the request ids
/// already put back, so a request is restored once.
fn revive(
    state: &mut State,
    record: &ReservationRecord,
    ctx: &Standing<'_>,
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
    let mut dropped = 0;
    if counted {
        let failures = record.prepare_failures;
        let used_up = [failures > 0, failures >= ctx.max_replace];
        let mut exclusions: BTreeMap<HostId, Exclusion> =
            record.exclusions.iter().map(exclusion).collect();
        let over = exclusions.len() > ctx.max_exclusions;
        while exclusions.len() > ctx.max_exclusions {
            exclusions.pop_last();
        }
        dropped += usize::from(over);
        let terminal = used_up.into_iter().all(|used| used);
        let since = record.queued.map_or(ctx.now, |queued| queued.since);
        let counts = WholeKept {
            give_ups: record.give_ups,
            prepare_failures: failures,
            exclusions,
        };
        let engine = ctx
            .placer
            .is_some_and(|placer| placer.whole_restore(&id, &counts, terminal, since));
        // A placer that keeps the request keeps all of it; the registry keeps the give-up
        // count it shows and hands the placer at each pass.
        let kept = match engine {
            true => RequestRecord {
                give_ups: record.give_ups,
                ..RequestRecord::default()
            },
            false => RequestRecord {
                prepare_failures: failures,
                give_ups: record.give_ups,
                exclusions: counts.exclusions,
                last: None,
                terminal,
            },
        };
        let key = RequestKey::Id(id.clone());
        let registry = ctx.keeper.is_none();
        if [kept.give_ups > 0, !engine].into_iter().any(|keep| keep) & registry {
            state.requests.restore(key, kept, ctx.now);
        }
    }
    let wants_place = record.queued;
    let placed = wants_place.is_some_and(|queued| match ctx.keeper {
        Some(keeper) => keeper.whole_restore_place(&id, queued.since),
        None => {
            let room = state.waiting.len() < MAX_WAITING_REQUESTS;
            room.then(|| state.waiting.restore(id.clone(), queued.since, ctx.now))
                .is_some()
        }
    });
    dropped += usize::from(wants_place.is_some() & !placed);
    let mut marked = false;
    if let Some(held) = &record.held {
        let leaving = state.ending.contains(&held.host);
        let standing = [placed, !leaving].into_iter().all(|ok| ok);
        marked = standing
            && match ctx.keeper {
                Some(keeper) => {
                    let mark = Reservation {
                        request: id,
                        host: held.host.clone(),
                        at: held.reserved_at,
                        deadline: held.deadline,
                        assigned: held.assigned,
                        tail_end: held.tail_end,
                    };
                    keeper.whole_revive(&mark, ctx.claim_by)
                }
                None => state.marks.revive(Revived {
                    request: id,
                    host: held.host.clone(),
                    at: held.reserved_at,
                    deadline: held.deadline,
                    assigned: held.assigned,
                    capped: record.give_ups >= ctx.max_reserve_attempts,
                    tail_end: held.tail_end,
                    claim_by: ctx.claim_by,
                }),
            };
        dropped += usize::from(!marked);
    }
    let resumed = [counted, placed, marked].into_iter().any(|kept| kept);
    Restored {
        resumed: usize::from(resumed),
        dropped,
    }
}
