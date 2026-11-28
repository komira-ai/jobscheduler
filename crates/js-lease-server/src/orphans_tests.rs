//! The orphans of a host and the guard, on the job tables alone: what [`held`] keeps of a
//! listing and [`judge`] decides from it, at each boundary of the guard and of the bound.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::StopReason;
use js_jobs::{Orphan, Retention};
use js_state::{Clock as _, ManualClock as StoreClock, StoreTime};
use js_types::HostId;

use super::{Judged, Listing, held, judge};
use crate::agent_api::job_rig::tables_on;
use crate::job_expiry::JobLimits;
use crate::metrics::MetricsSink;

const HOUR: Duration = Duration::from_secs(3600);

fn host() -> HostId {
    HostId::new("host-a").unwrap()
}

fn at(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(secs))
}

fn orphan(secs: u64, end_asked: bool) -> Orphan {
    Orphan {
        first_seen: at(secs),
        end_asked,
    }
}

fn orphans(items: &[(&str, Orphan)]) -> BTreeMap<String, Orphan> {
    items
        .iter()
        .map(|(job, orphan)| ((*job).to_owned(), *orphan))
        .collect()
}

/// The listed jobs the store holds no record of, each with whether it ended on the host.
fn unknown(items: &[(&str, bool)]) -> BTreeMap<String, bool> {
    items
        .iter()
        .map(|(job, ended)| ((*job).to_owned(), *ended))
        .collect()
}

/// A sink that records the orphan counts it is told of, and counts nothing else.
#[derive(Debug, Default)]
struct Told(Mutex<Vec<String>>);

impl Told {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl MetricsSink for Told {
    fn job_placed(&self, _: Duration) {}
    fn start_failed(&self, _: js_jobs::StartFailure) {}
    fn job_lost(&self) {}
    fn store_write_failed(&self) {}
    fn job_pass_took(&self, _: Duration) {}
    fn job_tables(&self, _: js_sched_placer::engine::TablesFrom) {}
    fn acquire_answered(&self, _: &str, _: &js_elastic::Acquired) {}
    fn unknown_jobs_found(&self, host: &HostId, count: usize) {
        self.0.lock().unwrap().push(format!("found {host} {count}"));
    }
    fn unknown_listing_guarded(&self, host: &HostId) {
        self.0.lock().unwrap().push(format!("guarded {host}"));
    }
}

fn limits(after: Duration) -> JobLimits {
    JobLimits {
        orphan_after: after,
        ..JobLimits::DEFAULT
    }
}

/// [`judge`] of a listing of `listed` jobs with `stopped` stopped for a record, over
/// `now_orphans` (the orphans after the listing) and `before` (the row's), at `now`.
fn judged(
    told: &Told,
    (listed, stopped): (usize, &[(&str, StopReason)]),
    (now_orphans, before): (&BTreeMap<String, Orphan>, &BTreeMap<String, Orphan>),
    now: Option<StoreTime>,
) -> Judged {
    let stopped: BTreeMap<String, StopReason> = stopped
        .iter()
        .map(|(job, why)| ((*job).to_owned(), *why))
        .collect();
    let listing = Listing {
        listed,
        stopped: &stopped,
    };
    judge(
        (&host(), told),
        &limits(HOUR),
        &listing,
        (now_orphans, before, now),
    )
}

fn stops(items: &[(&str, StopReason)]) -> BTreeMap<String, StopReason> {
    items
        .iter()
        .map(|(job, why)| ((*job).to_owned(), *why))
        .collect()
}

/// Asserts what the row keeps of a listing: no orphan and no read of the store's clock for a
/// listing whose unknown jobs all ended on the host; the store's time now for an orphan first
/// listed, the row's time and an operator's ask kept for one listed before, and an orphan no
/// longer listed (or now listed with its result) dropped. It catches an orphan whose age starts
/// again at every beat (it would never reach its bound), an ended job kept as an orphan (its
/// host would never be clean), and a clock read on every beat.
#[test]
fn the_row_keeps_each_running_unknown_job_with_the_time_it_was_first_listed() {
    let clock = Arc::new(StoreClock::default());
    clock.advance(Duration::from_secs(500));
    let (store, jobs) = tables_on(clock.clone(), Retention::DEFAULT);
    store.fail_clock(true);
    let ended_only = unknown(&[("done", true)]);
    assert_eq!(
        held(&jobs, &BTreeMap::new(), &ended_only).unwrap(),
        (BTreeMap::new(), None)
    );
    assert!(held(&jobs, &BTreeMap::new(), &unknown(&[("new", false)])).is_err());
    store.fail_clock(false);

    let before = orphans(&[
        ("kept", orphan(100, true)),
        ("ended-now", orphan(100, false)),
        ("gone", orphan(100, false)),
    ]);
    let listed = unknown(&[("kept", false), ("ended-now", true), ("new", false)]);
    let now = clock.now();
    let (after, read) = held(&jobs, &before, &listed).unwrap();
    assert_eq!(read, Some(now));
    assert_eq!(
        after,
        orphans(&[
            ("kept", orphan(100, true)),
            (
                "new",
                Orphan {
                    first_seen: now,
                    end_asked: false
                }
            ),
        ])
    );
}

/// Asserts the guard at its line: a listing of which exactly half is orphans is acted on (the
/// stops the store records are sent), one of which more than half is orphans stops nothing but
/// an operator's ended orphan, and a listing of one job that is an orphan is guarded. Each beat
/// the guard holds is counted, and the orphans a beat finds first are counted once. It catches
/// a guard that trips on a host with one unknown job among its own, one that never trips, and
/// one that holds an operator's end.
#[test]
fn more_than_half_of_a_listing_unknown_stops_nothing_but_an_operators_end() {
    let told = Told::default();
    let none = BTreeMap::new();
    let two = orphans(&[("o1", orphan(0, false)), ("o2", orphan(0, true))]);
    let cancelled = [("c1", StopReason::Cancelled), ("e1", StopReason::Ended)];

    let half = judged(&told, (4, &cancelled), (&two, &two), Some(at(1)));
    assert!(!half.guarded);
    assert_eq!(
        half.stops,
        stops(&[
            ("c1", StopReason::Cancelled),
            ("e1", StopReason::Ended),
            ("o2", StopReason::Operator),
        ])
    );
    assert!(told.take().is_empty());

    let more = judged(&told, (3, &cancelled[..1]), (&two, &none), Some(at(1)));
    assert!(more.guarded);
    assert_eq!(more.stops, stops(&[("o2", StopReason::Operator)]));
    assert_eq!(told.take(), ["found host-a 2", "guarded host-a"]);

    let one = orphans(&[("o1", orphan(0, false))]);
    let alone = judged(&told, (1, &[]), (&one, &one), Some(at(1)));
    assert_eq!(
        alone,
        Judged {
            stops: BTreeMap::new(),
            guarded: true
        }
    );
    assert_eq!(told.take(), ["guarded host-a"]);

    let nothing = judged(&told, (2, &[]), (&none, &none), None);
    assert_eq!(nothing, Judged::default());
    assert!(told.take().is_empty());
}

/// Asserts the bound: an orphan is not ended before it has been listed for the bound, the one
/// listed longest is ended at the bound (the lower id first among those first listed
/// together), only one at a time, never one an operator ended already, and none at all under
/// the guard. It catches a whole host's orphans ended in one beat, an orphan ended early, one
/// never ended, and a bound that bypasses the guard.
#[test]
fn only_the_orphan_listed_longest_is_ended_and_only_once_past_the_bound() {
    let told = Told::default();
    let three = orphans(&[
        ("o-b", orphan(10, false)),
        ("o-a", orphan(10, false)),
        ("o-c", orphan(20, false)),
    ]);
    let listing: (usize, &[(&str, StopReason)]) = (7, &[]);
    let early = judged(&told, listing, (&three, &three), Some(at(10 + 3599)));
    assert!(early.stops.is_empty());
    let due = judged(&told, listing, (&three, &three), Some(at(10 + 3600)));
    assert_eq!(due.stops, stops(&[("o-a", StopReason::OrphanExpired)]));
    let later = judged(&told, listing, (&three, &three), Some(at(20 + 7200)));
    assert_eq!(later.stops, stops(&[("o-a", StopReason::OrphanExpired)]));

    let asked = orphans(&[("o-a", orphan(10, true)), ("o-b", orphan(10, false))]);
    let past = judged(&told, (4, &[]), (&asked, &asked), Some(at(10 + 3600)));
    assert_eq!(
        past.stops,
        stops(&[
            ("o-a", StopReason::Operator),
            ("o-b", StopReason::OrphanExpired)
        ])
    );

    let guarded = judged(&told, (5, &[]), (&three, &three), Some(at(20 + 7200)));
    assert!(guarded.guarded & guarded.stops.is_empty());
}
