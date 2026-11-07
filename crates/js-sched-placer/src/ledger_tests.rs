//! What the engine hands the registry's reservation ledger and takes back from it, driven
//! as the registry drives it: a round per pass, the start of each request, a failed prepare,
//! and a restart's records put back before any request is presented again. The clock is the
//! test's: farm milliseconds passed by hand.

use std::collections::BTreeMap;
use std::time::Duration;

use js_lease::{ManualClock, Timestamp};
use js_provider::{Candidate, Held, StartSpec};
use js_registry::{
    Exclusion, MarkedTails, NotReady, Placer, PrepareFailure, Standing, WaitingWhole, WholeHost,
    WholeKept, WholeNodeConfig, WholeRound,
};
use js_sched::Bar;
use js_types::sched::FarmTime;
use js_types::{
    HostId, LeaseId, Request as HostRequest, RequestId, ResourceClass, Resources, Shape,
};

use crate::engine::{Engine, Settings};
use crate::map;
use crate::place::SchedPlacer;

/// `reserve_backoff` of the rounds.
const BACKOFF: u64 = 60_000;
/// `drain_after` of the rounds.
const DRAIN_AFTER: u64 = 10_000;
/// The drain deadline of the rounds.
const DEADLINE: u64 = 60_000;
/// The wall time of every request.
const WALL: u64 = 600_000;

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

/// An engine built at zero over `hosts`, each with the leases given.
fn engine(hosts: &[(&str, &[u64])]) -> Engine {
    let every: Vec<Candidate> = hosts.iter().map(|(n, s)| candidate(n, s)).collect();
    Engine::build(Settings::default(), &every, at(0))
}

/// The settings of the rounds: a request waits ten seconds before it marks a busy host, a
/// mark is given up after a minute and the host is then backed off for one.
fn settings() -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms: DRAIN_AFTER,
        drain_deadline_ms: Some(DEADLINE),
        reserve_backoff_ms: BACKOFF,
        ..WholeNodeConfig::dedicated(4)
    }
}

/// Request `name`, first presented at `since` ms, that may run ten minutes.
fn waiting(name: &str, since: u64) -> WaitingWhole {
    WaitingWhole {
        id: id(name),
        max_wall: Duration::from_millis(WALL),
        since: stamp(since),
        give_ups: 0,
        exclusions: BTreeMap::new(),
        mark: None,
    }
}

/// A round of `waiting` over the live hosts `names`, at `now`, each heard from then.
fn pass(e: &mut Engine, waiting: Vec<WaitingWhole>, names: &[&str], now: u64) {
    for name in names {
        e.heard(&host(name), None, at(now));
    }
    let hosts = names.iter().map(|name| WholeHost {
        id: host(name),
        standing: Standing::Live,
        open: true,
    });
    let round = WholeRound {
        settings: settings(),
        waiting,
        hosts: hosts.collect(),
        outside: MarkedTails::new(),
        lives: BTreeMap::new(),
    };
    e.whole_round(round, at(now));
}

/// The start of whole-node request `name` as lease `1-<seq>`.
fn whole_spec(seq: u64, name: &str) -> StartSpec {
    let class = ResourceClass::whole_node();
    let request = HostRequest {
        class: class.id().clone(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_millis(WALL),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    let spec = StartSpec::new(lease(seq), request, &[class]).unwrap();
    spec.with_request_id(id(name))
}

/// A failed prepare that keeps the host off the request for life.
fn for_life() -> PrepareFailure {
    PrepareFailure {
        exclusion: Some(Exclusion::ForLife),
        not_ready: NotReady {
            code: "unsupported".into(),
            detail: "no hook".into(),
        },
    }
}

/// When the hold of `r` reaches its deadline: a minute after it was made.
const GIVE_UP: u64 = DRAIN_AFTER + DEADLINE;

/// An engine over busy `a` where request `r` held `a` from `DRAIN_AFTER` and gave the hold
/// up at its deadline, `GIVE_UP`.
fn gave_up() -> Engine {
    let mut e = engine(&[("a", &[1])]);
    pass(&mut e, vec![waiting("r", 0)], &["a"], DRAIN_AFTER);
    assert!(e.sched.whole_mark(e.whole[&id("r")]).is_some());
    // The host is heard and the queue passed every five seconds, as the registry does.
    for now in (DRAIN_AFTER + 5_000..=GIVE_UP).step_by(5_000) {
        pass(&mut e, vec![waiting("r", 0)], &["a"], now);
    }
    assert_eq!(e.sched.whole_give_ups(e.whole[&id("r")]), 1);
    e
}

/// An engine over quiet `b` where request `s` was committed on `b` at 1 s and its prepare
/// failed there for life.
fn failed() -> Engine {
    let mut e = engine(&[("b", &[])]);
    pass(&mut e, vec![waiting("s", 1_000)], &["b"], 1_000);
    let open = [candidate("b", &[])];
    let committed = e.commit_whole(&whole_spec(7, "s"), &open, at(1_000));
    assert_eq!(committed, Some(Some(0)));
    // Committed and preparing, with nothing counted: nothing to keep.
    assert!(e.whole_ledger(at(1_000)).is_empty());
    assert!(e.prepare_failed(&lease(7), &for_life(), at(1_000)));
    e
}

/// Catches: a ledger without the give-up the engine counted, without its back-off, or
/// without the failed prepare and the bar of a request the engine committed (a restart under
/// js-sched would then reset `max_reserve_attempts`, lift a back-off and give the request its
/// tries again). Each request is written with its counts and its host; a back-off that has
/// ended is no longer written, and the count stays.
#[test]
fn the_ledger_holds_each_requests_give_ups_back_off_failures_and_bars() {
    let e = gave_up();
    let ledger = e.whole_ledger(at(GIVE_UP));
    let r = &ledger[&id("r")];
    assert_eq!((r.give_ups, r.prepare_failures), (1, 0));
    let back_off = Exclusion::Until(stamp(GIVE_UP + BACKOFF));
    assert_eq!(r.exclusions, BTreeMap::from([(host("a"), back_off)]));
    let later = e.whole_ledger(at(GIVE_UP + 10 * BACKOFF));
    assert_eq!(later[&id("r")].give_ups, 1);
    assert!(later[&id("r")].exclusions.is_empty());

    let e = failed();
    let barred = WholeKept {
        give_ups: 0,
        prepare_failures: 1,
        exclusions: BTreeMap::from([(host("b"), Exclusion::ForLife)]),
    };
    assert_eq!(
        e.whole_ledger(at(1_000)),
        BTreeMap::from([(id("s"), barred.clone())])
    );
    let later = e.whole_ledger(at(GIVE_UP + 10 * BACKOFF));
    assert_eq!(later, BTreeMap::from([(id("s"), barred)]));
}

/// Catches: a restored record that the engine does not keep until its request is presented,
/// one queued without its give-ups or its back-off, a back-off put back as a hard bar (the
/// request could then never take the host it backed off, even with no other), and a record
/// with failed prepares that is not answered until it is queued: the terminal request is
/// refused at its first start, once, with both its bars, and the record without failures
/// shows none.
#[test]
fn a_restored_request_is_queued_with_its_counts_and_a_terminal_one_is_refused() {
    let mut e = engine(&[("a", &[1])]);
    let backoff_end = stamp(5 * BACKOFF);
    let gave_up = WholeKept {
        give_ups: 1,
        prepare_failures: 0,
        exclusions: BTreeMap::from([(host("a"), Exclusion::Until(backoff_end))]),
    };
    e.whole_restore(&id("r"), &gave_up, (false, stamp(0)), at(1_000));
    let used = WholeKept {
        give_ups: 0,
        prepare_failures: 2,
        exclusions: BTreeMap::from([
            (host("a"), Exclusion::Until(backoff_end)),
            (host("b"), Exclusion::ForLife),
        ]),
    };
    e.whole_restore(&id("t"), &used, (true, stamp(0)), at(1_000));
    assert_eq!(e.whole_record(&id("r")), None);
    let terminal = e.whole_record(&id("t")).unwrap();
    assert_eq!((terminal.prepare_failures, terminal.terminal), (2, true));
    assert_eq!(terminal.last, None);
    assert_eq!(terminal.exclusions, used.exclusions);
    let ledger = e.whole_ledger(at(1_000));
    assert_eq!(ledger[&id("r")], gave_up);
    assert_eq!(ledger[&id("t")], used);
    assert!(e.whole_start(&id("t"), at(2_000)));
    assert!(!e.whole_start(&id("t"), at(2_000)));

    pass(&mut e, vec![waiting("r", 0)], &["a"], 2_000);
    let operation = e.whole[&id("r")];
    assert_eq!(e.sched.whole_give_ups(operation), 1);
    let worker = map::worker(&host("a"));
    let bars = e.sched.whole_barred(operation, at(2_000));
    let soft = Bar::Backoff(FarmTime::from_millis(backoff_end.millis()));
    assert_eq!(bars, BTreeMap::from([(worker, soft)]));
    assert_eq!(e.whole_ledger(at(2_000))[&id("r")], gave_up);
}

/// Catches: what a restart put back kept for ever when its request never comes back (it
/// would be written to every ledger after), or dropped a tick before the idle horizon; a
/// cancel that keeps it; and a rebuild of the engine that loses it.
#[test]
fn what_a_restart_put_back_and_nobody_presented_goes_at_the_idle_horizon() {
    let horizon = Duration::from_millis(4 * BACKOFF);
    let kept = WholeKept {
        give_ups: 2,
        ..WholeKept::default()
    };
    let mut e = engine(&[("a", &[])]);
    e.whole_restore(&id("r"), &kept, (false, stamp(0)), at(1_000));
    e.whole_restore(&id("c"), &kept, (false, stamp(0)), at(1_000));
    e.whole_forget(&id("c"));
    let rebuilt = e.rebuild(Settings::default(), &[candidate("a", &[])], at(2_000));
    assert_eq!(
        rebuilt.whole_ledger(at(2_000)),
        BTreeMap::from([(id("r"), kept.clone())])
    );
    let mut e = rebuilt;
    e.whole_sweep(horizon, at(1_000 + 4 * BACKOFF - 1));
    assert_eq!(e.whole_ledger(at(1_000)).len(), 1);
    e.whole_sweep(horizon, at(1_000 + 4 * BACKOFF));
    assert!(e.whole_ledger(at(1_000)).is_empty());
}

/// Catches: a placer that does not hand the ledger calls to its engine, or that says it does
/// not keep a restored record (the registry would then keep the counts and the engine would
/// queue the request without them).
#[test]
fn the_placer_hands_the_ledger_calls_to_its_engine() {
    let placer = SchedPlacer::new(Settings::default(), ManualClock::new(stamp(0)));
    assert_eq!(placer.whole_reservations(), Some(Vec::new()));
    assert!(placer.whole_ledger().is_empty());
    let kept = WholeKept {
        give_ups: 1,
        ..WholeKept::default()
    };
    assert!(placer.whole_restore(&id("r"), &kept, false, stamp(0)));
    assert_eq!(placer.whole_ledger(), BTreeMap::from([(id("r"), kept)]));
}
