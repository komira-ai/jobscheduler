//! The prepare of the whole-node leases the engine commits, and the records of the requests
//! whose prepare failed, driven as the registry drives them: a round per pass, the start of
//! each request, and the registry's calls for the deadline, the ready report, a hello, a
//! failure, a start and its idle horizon. The clock is the test's: farm milliseconds passed
//! by hand.

use std::collections::BTreeMap;
use std::time::Duration;

use js_lease::{ManualClock, Timestamp};
use js_provider::{Candidate, Held, StartSpec};
use js_registry::{
    Exclusion, MarkedTails, NotReady, Placer, PrepareFailure, Reservation, Standing, WaitingWhole,
    WholeHost, WholeNodeConfig, WholeRound,
};
use js_types::sched::FarmTime;
use js_types::{
    HostId, LeaseId, Request as HostRequest, RequestId, ResourceClass, Resources, Shape,
};

use crate::engine::{Engine, Settings};
use crate::place::SchedPlacer;

/// `prepare_timeout` of the rounds.
const TIMEOUT: u64 = 120_000;
/// `reserve_backoff` of the rounds.
const BACKOFF: u64 = 60_000;
/// `drain_after` of the rounds.
const DRAIN_AFTER: u64 = 10_000;

fn at(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

fn stamp(ms: u64) -> Timestamp {
    Timestamp::from_millis(ms)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn id(name: &str) -> RequestId {
    RequestId::new(name).unwrap()
}

fn lease(seq: u64) -> LeaseId {
    LeaseId::new(1, seq)
}

/// A host of 8 cores and 16 GiB named `name`, carrying the one-core leases `1-<seq>` of
/// `seqs`.
fn candidate(name: &str, seqs: &[u64]) -> Candidate {
    let caps = [("arch".to_owned(), "x86_64".to_owned())]
        .into_iter()
        .collect();
    let shape = Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps);
    let leases = seqs.iter().map(|seq| Held {
        lease: lease(*seq),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        whole: false,
    });
    Candidate {
        host: host(name),
        shape,
        leases: leases.collect(),
    }
}

/// The hosts of `hosts`, each with the leases given.
fn every(hosts: &[(&str, &[u64])]) -> Vec<Candidate> {
    let every = hosts.iter().map(|(name, seqs)| candidate(name, seqs));
    every.collect()
}

/// An engine built at zero over `hosts`, each with the leases given.
fn engine(hosts: &[(&str, &[u64])]) -> Engine {
    Engine::build(Settings::default(), &every(hosts), at(0))
}

/// The settings of the rounds: a two-minute prepare, a one-minute back-off, two tries, and
/// a request that waits ten seconds before it marks a busy host.
fn settings() -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms: DRAIN_AFTER,
        drain_deadline_ms: Some(600_000),
        prepare_timeout_ms: TIMEOUT,
        reserve_backoff_ms: BACKOFF,
        max_replace: 2,
        ..WholeNodeConfig::dedicated(4)
    }
}

/// Request `name`, first presented at `since` ms, that may run ten minutes.
fn waiting(name: &str, since: u64) -> WaitingWhole {
    WaitingWhole {
        id: id(name),
        max_wall: Duration::from_secs(600),
        since: stamp(since),
        give_ups: 0,
        exclusions: BTreeMap::new(),
        mark: None,
    }
}

/// A round of `waiting` over the live hosts `names`.
fn round(waiting: Vec<WaitingWhole>, names: &[&str]) -> WholeRound {
    let hosts = names.iter().map(|name| WholeHost {
        id: host(name),
        standing: Standing::Live,
        open: true,
    });
    WholeRound {
        settings: settings(),
        waiting,
        hosts: hosts.collect(),
        outside: MarkedTails::new(),
        lives: BTreeMap::new(),
    }
}

/// The start of whole-node request `name` as lease `1-<seq>`.
fn whole_spec(seq: u64, name: &str) -> StartSpec {
    let class = ResourceClass::whole_node();
    let request = HostRequest {
        class: class.id().clone(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    let spec = StartSpec::new(lease(seq), request, &[class]).unwrap();
    spec.with_request_id(id(name))
}

/// Each of `marks` as `<request>@<host>`, ending in `*` when it is assigned.
fn held(marks: &[Reservation]) -> Vec<String> {
    let held = marks.iter().map(|mark| {
        let assigned = if mark.assigned { "*" } else { "" };
        format!("{}@{}{assigned}", mark.request, mark.host)
    });
    held.collect()
}

/// Request `name`, presented at `since`, is granted the quiet host of `hosts` in a round at
/// `now` (each test leaves one quiet host it is not barred from) and its start `1-<seq>`
/// commits there at `now`: the host it committed on.
fn commit(
    e: &mut Engine,
    (name, since, seq): (&str, u64, u64),
    hosts: &[&str],
    now: u64,
) -> HostId {
    for on in hosts {
        e.heard(&host(on), None, at(now));
    }
    e.whole_round(round(vec![waiting(name, since)], hosts), at(now));
    let open: Vec<Candidate> = hosts.iter().map(|name| candidate(name, &[])).collect();
    let index = e.commit_whole(&whole_spec(seq, name), &open, at(now));
    open[index.flatten().unwrap()].host.clone()
}

/// A failure that sets `exclusion`, named `code`.
fn failure(exclusion: Option<Exclusion>, code: &str) -> PrepareFailure {
    PrepareFailure {
        exclusion,
        not_ready: NotReady {
            code: code.into(),
            detail: format!("{code} detail"),
        },
    }
}

/// Catches: a deadline counted from anything but the commit, or one that ends a tick early or
/// late; and a lease the engine did not commit answered as overdue (the registry would then
/// not keep the deadline of a lease put back after a restart). The deadline is two minutes
/// after the commit, exactly.
#[test]
fn a_committed_lease_is_overdue_at_its_deadline_and_not_before() {
    let mut e = engine(&[("a", &[])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    assert_eq!(
        e.prepare_overdue(&lease(7), at(1_000 + TIMEOUT - 1)),
        Some(false)
    );
    assert_eq!(
        e.prepare_overdue(&lease(7), at(1_000 + TIMEOUT)),
        Some(true)
    );
    assert_eq!(e.prepare_overdue(&lease(8), at(1_000 + TIMEOUT)), None);
}

/// Catches: a ready report that leaves the deadline running (a ready lease would then fail),
/// a record that survives its request's placement, or a report for a lease the engine did
/// not commit that changes anything. `r` fails once on `a`, is placed on `b` and reported
/// ready there: its record goes and the lease is never overdue.
#[test]
fn a_ready_report_ends_the_deadline_and_the_requests_record() {
    let mut e = engine(&[("a", &[]), ("b", &[1])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    let until = Exclusion::Until(stamp(2_000 + BACKOFF));
    assert!(e.prepare_failed(&lease(7), &failure(Some(until), "not-quiet"), at(2_000)));
    assert!(e.whole_record(&id("r")).is_some());
    e.ended(lease(1));
    let on = commit(&mut e, ("r", 3_000, 8), &["a", "b"], 3_000);
    assert_eq!(on, host("b"));
    e.prepare_ready(&lease(9));
    assert!(e.whole_record(&id("r")).is_some());
    e.prepare_ready(&lease(8));
    assert_eq!(e.whole_record(&id("r")), None);
    assert_eq!(e.prepare_overdue(&lease(8), at(10 * TIMEOUT)), Some(false));
}

/// Catches: a hello that postpones the deadline of a lease not yet ready (an agent that keeps
/// restarting would hold the host for ever), and one that leaves a ready lease with no
/// deadline (the as-built hello rule: the agent must report ready again within a whole
/// `prepare_timeout` of its hello). A hello for a lease the engine did not commit changes
/// nothing.
#[test]
fn a_hello_restarts_the_prepare_only_of_a_ready_lease() {
    let mut e = engine(&[("a", &[])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    e.prepare_again(&lease(7), at(50_000));
    e.prepare_again(&lease(8), at(50_000));
    assert_eq!(
        e.prepare_overdue(&lease(7), at(1_000 + TIMEOUT)),
        Some(true)
    );
    e.prepare_ready(&lease(7));
    e.prepare_again(&lease(7), at(200_000));
    assert_eq!(
        e.prepare_overdue(&lease(7), at(200_000 + TIMEOUT - 1)),
        Some(false)
    );
    assert_eq!(
        e.prepare_overdue(&lease(7), at(200_000 + TIMEOUT)),
        Some(true)
    );
    assert_eq!(e.prepare_overdue(&lease(8), at(200_000 + TIMEOUT)), None);
}

/// Catches: a failure that is not counted, a host not barred for `reserve_backoff`, a lease
/// left held, or a request queued again from its new presentation (it would wait out
/// `drain_after` again). `r` commits on quiet `a` and fails there; presented again, `a` is
/// barred and `b` is busy: `r` marks `b` at once, as it has waited `drain_after` since it
/// was first queued. A request queued from its new presentation would mark nothing yet.
#[test]
fn a_failed_prepare_counts_bars_the_host_and_requeues_from_the_first_queue_time() {
    let mut e = engine(&[("a", &[]), ("b", &[1])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    let until = Exclusion::Until(stamp(12_000 + BACKOFF));
    assert!(e.prepare_failed(&lease(7), &failure(Some(until), "setup-failed"), at(12_000)));
    let still_held = e.on.get(&host("a")).and_then(|on| on.get(&lease(7)));
    assert!(still_held.is_none());
    assert!(!e.prepare_failed(&lease(7), &failure(Some(until), "setup-failed"), at(12_000)));
    let record = e.whole_record(&id("r")).unwrap();
    assert_eq!((record.prepare_failures, record.terminal), (1, false));
    assert_eq!(record.exclusions, BTreeMap::from([(host("a"), until)]));
    assert_eq!(record.last.unwrap().code, "setup-failed");
    for name in ["a", "b"] {
        e.heard(&host(name), None, at(13_000));
    }
    let again = round(vec![waiting("r", 13_000)], &["a", "b"]);
    let marks = e.whole_round(again, at(13_000));
    assert_eq!(held(&marks), ["r@b"]);
    let mut fresh = engine(&[("a", &[]), ("b", &[1])]);
    let mut barred = waiting("r", 13_000);
    barred.exclusions = BTreeMap::from([(host("a"), until)]);
    let marks = fresh.whole_round(round(vec![barred], &["a", "b"]), at(13_000));
    assert!(marks.is_empty());
}

/// Catches: a cause mapped to another effect on the host. A refusal for life bars the host
/// for good and is shown as such; one that taints the host bars nothing for the request (the
/// taint keeps the host from every request); and a later, stronger bar on a host the request
/// was already barred from replaces the weaker one.
#[test]
fn each_exclusion_bars_the_host_as_its_row_says() {
    let mut e = engine(&[("a", &[]), ("b", &[1]), ("c", &[2])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    assert!(e.prepare_failed(&lease(7), &failure(None, "host-unsafe"), at(2_000)));
    assert!(e.whole_record(&id("r")).unwrap().exclusions.is_empty());
    commit(&mut e, ("s", 0, 8), &["a"], 3_000);
    let until = Exclusion::Until(stamp(3_000 + BACKOFF));
    assert!(e.prepare_failed(&lease(8), &failure(Some(until), "not-quiet"), at(3_000)));
    let after = 3_000 + BACKOFF;
    commit(&mut e, ("s", after, 9), &["a"], after);
    let life = failure(Some(Exclusion::ForLife), "unsupported");
    assert!(e.prepare_failed(&lease(9), &life, at(after)));
    let record = e.whole_record(&id("s")).unwrap();
    assert_eq!(
        record.exclusions,
        BTreeMap::from([(host("a"), Exclusion::ForLife)])
    );
    e.ended(lease(1));
    let hosts = ["a", "b", "c"];
    for name in hosts {
        e.heard(&host(name), None, at(after));
    }
    let marks = e.whole_round(round(vec![waiting("s", after)], &hosts), at(after));
    assert_eq!(held(&marks), ["s@b*"]);
}

/// Catches: a terminal request refused more than once, or one whose record and bars survive
/// the refusal (the start after it would not be fresh), or a request refused before it has
/// used up `max_replace`. Two failures make `r` terminal; its next start is refused once and
/// the start after it finds no record.
#[test]
fn max_replace_refuses_the_next_start_once_and_then_starts_afresh() {
    let mut e = engine(&[("a", &[]), ("b", &[1])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    let life = failure(Some(Exclusion::ForLife), "unsupported");
    assert!(e.prepare_failed(&lease(7), &life, at(2_000)));
    assert!(!e.whole_start(&id("r"), at(2_500)));
    assert!(!e.whole_record(&id("r")).unwrap().terminal);
    e.ended(lease(1));
    commit(&mut e, ("r", 3_000, 8), &["a", "b"], 3_000);
    assert!(e.prepare_failed(&lease(8), &failure(None, "host-unsafe"), at(4_000)));
    let record = e.whole_record(&id("r")).unwrap();
    assert_eq!((record.prepare_failures, record.terminal), (2, true));
    assert_eq!(record.last.unwrap().code, "host-unsafe");
    assert!(e.whole_start(&id("r"), at(5_000)));
    assert_eq!(e.whole_record(&id("r")), None);
    assert!(!e.whole_start(&id("r"), at(6_000)));
}

/// Catches: a record that outlives its consumer (its bars for life would never go), one
/// dropped a tick early, one dropped while its request holds a committed lease, and a start
/// that does not restart the idle time.
#[test]
fn an_idle_record_is_dropped_after_the_horizon() {
    let horizon = Duration::from_millis(4 * BACKOFF);
    let life = failure(Some(Exclusion::ForLife), "unsupported");
    let mut e = engine(&[("a", &[]), ("b", &[1])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    assert!(e.prepare_failed(&lease(7), &life, at(2_000)));
    commit(&mut e, ("s", 0, 8), &["a"], 3_000);
    assert!(e.prepare_failed(&lease(8), &life, at(3_000)));
    e.ended(lease(1));
    commit(&mut e, ("s", 4_000, 9), &["a", "b"], 4_000);
    assert!(!e.whole_start(&id("r"), at(10_000)));
    e.whole_sweep(horizon, at(10_000 + 4 * BACKOFF - 1));
    assert!(e.whole_record(&id("r")).is_some());
    e.whole_sweep(horizon, at(10_000 + 4 * BACKOFF));
    assert_eq!(e.whole_record(&id("r")), None);
    assert!(e.whole_record(&id("s")).is_some());
}

/// Catches: a lease that ends without a failure (released, expired) counted as one, or its
/// deadline kept (the registry would end a lease that is gone).
#[test]
fn a_lease_that_ends_counts_nothing_and_has_no_deadline() {
    let mut e = engine(&[("a", &[])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    e.ended(lease(7));
    assert_eq!(e.prepare_overdue(&lease(7), at(1_000 + TIMEOUT)), None);
    assert!(!e.prepare_failed(&lease(7), &failure(None, "host-unsafe"), at(2_000)));
    assert_eq!(e.whole_record(&id("r")), None);
}

/// Catches: a rebuild of the engine that drops the deadlines of the committed leases or the
/// records of the failed requests (a restart of the engine would forget `max_replace`).
#[test]
fn a_rebuild_keeps_the_prepares_and_the_records() {
    let mut e = engine(&[("a", &[]), ("b", &[1])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    e.ended(lease(1));
    commit(&mut e, ("s", 0, 8), &["b"], 1_000);
    let until = Exclusion::Until(stamp(2_000 + BACKOFF));
    assert!(e.prepare_failed(
        &lease(8),
        &failure(Some(until), "noise-over-bound"),
        at(2_000)
    ));
    let listed = every(&[("a", &[7]), ("b", &[])]);
    let rebuilt = e.rebuild(Settings::default(), &listed, at(3_000));
    assert_eq!(
        rebuilt.prepare_overdue(&lease(7), at(1_000 + TIMEOUT)),
        Some(true)
    );
    assert_eq!(rebuilt.whole_record(&id("s")), e.whole_record(&id("s")));
    assert!(rebuilt.whole_record(&id("s")).is_some());
}

/// Catches: the placer not handing the registry's calls to its engine. A placer that
/// committed nothing keeps no deadline, takes no report or failure, refuses no start and
/// keeps no record.
#[test]
fn the_placer_hands_the_prepare_calls_to_its_engine() {
    let placer = SchedPlacer::new(Settings::default(), ManualClock::new(stamp(0)));
    assert_eq!(placer.prepare_overdue(&lease(7)), None);
    placer.prepare_ready(&lease(7));
    placer.prepare_again(&lease(7));
    assert!(!placer.prepare_failed(&lease(7), &failure(None, "host-unsafe")));
    assert!(!placer.whole_start(&id("r")));
    assert_eq!(placer.whole_record(&id("r")), None);
    placer.whole_forget(&id("r"));
    placer.whole_sweep(Duration::from_secs(60));
    assert_eq!(placer.whole_record(&id("r")), None);
}

/// Catches: a cancelled request that keeps its failures, its bars or its queued operation
/// (the id would not name a fresh request when used again). `s`, barred from the one host for
/// life and queued, is cancelled: its operation goes at once, and presented again it is
/// granted the host.
#[test]
fn a_cancelled_request_forgets_its_record() {
    let mut e = engine(&[("a", &[])]);
    commit(&mut e, ("r", 0, 7), &["a"], 1_000);
    let life = failure(Some(Exclusion::ForLife), "unsupported");
    assert!(e.prepare_failed(&lease(7), &life, at(2_000)));
    e.whole_forget(&id("r"));
    assert_eq!(e.whole_record(&id("r")), None);
    commit(&mut e, ("s", 0, 8), &["a"], 3_000);
    assert!(e.prepare_failed(&lease(8), &life, at(4_000)));
    e.heard(&host("a"), None, at(5_000));
    let marks = e.whole_round(round(vec![waiting("s", 5_000)], &["a"]), at(5_000));
    assert!(marks.is_empty());
    let op = e.whole[&id("s")];
    e.whole_forget(&id("s"));
    assert!(!e.whole.contains_key(&id("s")));
    assert_eq!(e.sched.state(op), None);
    let marks = e.whole_round(round(vec![waiting("s", 6_000)], &["a"]), at(6_000));
    assert_eq!(held(&marks), ["s@a*"]);
}
