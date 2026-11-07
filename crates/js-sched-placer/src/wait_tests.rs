//! The engine's own wait list and request records, driven as the registry drives them under
//! `--placement js-sched`: each presentation, cancel, stop and restart call, and a pass per
//! round over the registry's hosts. The clock is the test's: farm milliseconds passed by
//! hand.

use std::collections::BTreeMap;
use std::time::Duration;

use js_registry::{
    Exclusion, MAX_WAITING_REQUESTS, MarkedTails, NotReady, PrepareFailure, Presented, RequestKey,
    Reservation, Standing, WholeHost, WholeNodeConfig, WholeRound,
};
use js_types::Timestamp;
use js_types::{Candidate, Held};
use js_types::{ClassId, HostId, LeaseId, Request, RequestId, Resources, Shape};

use crate::engine::{Engine, Settings};
use js_types::sched::FarmTime;

/// The regain grace of every presentation: a minute.
const GRACE: Duration = Duration::from_secs(60);

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
        lease: LeaseId::new(1, *seq),
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
    let every: Vec<Candidate> = hosts
        .iter()
        .map(|(name, seqs)| candidate(name, seqs))
        .collect();
    Engine::build(Settings::default(), &every, at(0))
}

/// A whole-node request that may run ten minutes.
fn request() -> Request {
    Request {
        class: ClassId::new("whole").unwrap(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    }
}

/// `e` is presented request `name` under lease `1-<seq>` at `now`.
fn present(e: &mut Engine, name: &str, seq: u64, capable: bool, now: u64) -> Presented {
    let asked = request();
    e.whole_present((&id(name), lease(seq), &asked), capable, GRACE, at(now))
}

/// The pass of the engine over `hosts`, standing as given, at `now`: a request waits ten
/// seconds before it marks a busy host, a mark's deadline is a minute after it is made, and two failed
/// prepares make a request terminal.
fn pass(e: &mut Engine, hosts: &[(&str, Standing)], now: u64) {
    let hosts = hosts.iter().map(|(name, standing)| WholeHost {
        id: host(name),
        standing: *standing,
        open: *standing == Standing::Live,
    });
    let round = WholeRound {
        settings: WholeNodeConfig {
            drain_after_ms: 10_000,
            drain_deadline_ms: Some(60_000),
            max_replace: 2,
            ..WholeNodeConfig::dedicated(4)
        },
        waiting: Vec::new(),
        hosts: hosts.collect(),
        outside: MarkedTails::new(),
        lives: BTreeMap::new(),
    };
    e.whole_pass(round, at(now));
}

/// The ids of the list, front first.
fn places(e: &Engine) -> Vec<String> {
    e.whole_places()
        .into_iter()
        .map(|(id, _)| id.to_string())
        .collect()
}

/// Each host held at `now` as `<request>@<host>`, ending in `*` when it is assigned.
fn held(e: &Engine, now: u64) -> Vec<String> {
    let held = e.whole_held(at(now)).into_iter().map(|mark| {
        let assigned = if mark.assigned { "*" } else { "" };
        format!("{}@{}{assigned}", mark.request, mark.host)
    });
    held.collect()
}

/// Catches a list that orders by anything but the first presentation (a poll moving a
/// request back, a cancel leaving a gap), an instant taken from a later presentation, and a
/// front request that is not the first to be given a quiet host. `a`, `b` and `c` are
/// presented in turn and `a` polled again; the list is `a b c` with the first instants;
/// `a` cancelled leaves `b c`. The one quiet host goes to `b`, the front.
#[test]
fn the_list_is_first_seen_order_and_a_repeat_is_a_poll() {
    let mut e = engine(&[("h", &[])]);
    assert_eq!(present(&mut e, "a", 1, true, 100), Presented::Waiting(0));
    assert_eq!(present(&mut e, "b", 2, true, 200), Presented::Waiting(1));
    assert_eq!(present(&mut e, "c", 3, true, 300), Presented::Waiting(2));
    assert_eq!(present(&mut e, "a", 1, true, 400), Presented::Waiting(0));
    assert_eq!(places(&e), ["a", "b", "c"]);
    let since: Vec<u64> = e.whole_places().iter().map(|(_, at)| at.millis()).collect();
    assert_eq!(since, [100, 200, 300]);
    let rows = e.whole_waiting();
    assert_eq!((rows[2].id.as_str(), rows[2].since.millis()), ("c", 300));
    assert_eq!(rows[2].request, request());
    assert!(e.whole_cancel(&id("a")));
    assert!(!e.whole_cancel(&id("a")));
    assert_eq!(places(&e), ["b", "c"]);
    pass(&mut e, &[("h", Standing::Live)], 600);
    assert_eq!(held(&e, 600), ["b@h*"]);
    let waiter = e.whole_waiter(&id("b")).unwrap();
    assert_eq!(
        (waiter.since.millis(), waiter.give_ups, waiter.marked),
        (200, 0, true)
    );
    assert!(!e.whole_waiter(&id("c")).unwrap().marked);
    assert_eq!(e.whole_waiter(&id("a")), None);
}

/// Catches a request kept past `regain_grace` without a presentation, or dropped a
/// millisecond early; a drop of a presented request counted as a restored one; and a
/// restored request never presented again dropped uncounted. `r` presented at zero is
/// still there a millisecond before the grace and goes at it; `q`, put back by a restart
/// at zero, goes at the same instant and is counted.
#[test]
fn a_request_not_presented_for_regain_grace_goes_at_the_boundary() {
    let mut e = engine(&[("h", &[1])]);
    present(&mut e, "r", 1, true, 0);
    assert!(e.whole_restore_place(&id("q"), stamp(0), at(0)));
    let grace = 60_000;
    assert_eq!(e.whole_expire(GRACE, at(grace - 1)), 0);
    assert_eq!(places(&e), ["r", "q"]);
    present(&mut e, "r", 1, true, grace - 1);
    assert_eq!(e.whole_expire(GRACE, at(grace)), 1);
    assert_eq!(places(&e), ["r"]);
    assert_eq!(e.whole_expire(GRACE, at(2 * grace - 1)), 0);
    assert_eq!(e.whole_expire(GRACE, at(2 * grace - 1 + grace)), 0);
    assert!(places(&e).is_empty());
}

/// Catches a request no host could serve entered on the list, one kept for ever while no
/// host could serve it, and one refused before the grace since the presentation that first
/// saw none (or with that instant not reset by a capable presentation).
#[test]
fn an_unservable_request_waits_out_the_grace_then_leaves() {
    let mut e = engine(&[("h", &[1])]);
    assert_eq!(present(&mut e, "n", 1, false, 0), Presented::Unservable);
    assert!(places(&e).is_empty());
    present(&mut e, "r", 2, true, 0);
    assert_eq!(present(&mut e, "r", 2, false, 1_000), Presented::Waiting(0));
    assert_eq!(present(&mut e, "r", 2, true, 30_000), Presented::Waiting(0));
    assert_eq!(
        present(&mut e, "r", 2, false, 40_000),
        Presented::Waiting(0)
    );
    assert_eq!(
        present(&mut e, "r", 2, false, 99_999),
        Presented::Waiting(0)
    );
    assert_eq!(
        present(&mut e, "r", 2, false, 100_000),
        Presented::Unservable
    );
    assert!(places(&e).is_empty());
}

/// Catches a list past its cap, a newcomer putting out a request that waits, a restart's
/// place past the cap, and a cancel that frees no place.
#[test]
fn the_cap_refuses_a_new_request_and_keeps_those_waiting() {
    let mut e = engine(&[("h", &[1])]);
    for n in 0..MAX_WAITING_REQUESTS {
        let name = format!("r{n}");
        let seq = u64::try_from(n).unwrap();
        assert_eq!(present(&mut e, &name, seq, true, 0), Presented::Waiting(n));
    }
    assert_eq!(present(&mut e, "late", 5_000, true, 0), Presented::Full);
    assert_eq!(present(&mut e, "r7", 7, true, 0), Presented::Waiting(7));
    assert!(!e.whole_restore_place(&id("back"), stamp(0), at(0)));
    assert!(e.whole_cancel(&id("r0")));
    let last = MAX_WAITING_REQUESTS - 1;
    assert_eq!(
        present(&mut e, "late", 5_000, true, 0),
        Presented::Waiting(last)
    );
}

/// Catches a stop that takes off a request another lease presented since, a binding that
/// does not move the request to the lease, and a binding of a request that does not wait.
#[test]
fn a_stop_takes_off_the_request_its_lease_presented_last() {
    let mut e = engine(&[("h", &[9])]);
    present(&mut e, "r", 1, true, 0);
    present(&mut e, "r", 2, true, 10);
    assert_eq!(e.whole_cancel_lease(&lease(1)), None);
    assert_eq!(e.whole_waiter_of(&lease(2)).unwrap().id, id("r"));
    assert!(e.whole_bind(&id("r"), lease(3)));
    assert!(!e.whole_bind(&id("s"), lease(4)));
    assert_eq!(e.whole_waiter_of(&lease(2)), None);
    assert_eq!(e.whole_cancel_lease(&lease(3)), Some(id("r")));
    assert!(places(&e).is_empty());
}

/// Catches a mark put back that does not hold its host, that is dropped before its host's
/// claim lapses or kept after, that is dropped uncounted (or counted on a tainted host), one
/// handed to the queue before its host said hello, and two marks put back on one host.
/// `r`'s mark on `z` waits for `z` until the claim; `q`'s on the tainted `t` goes at once,
/// uncounted; `p`'s on `h` is taken by the queue once `p` is presented, with its instant.
#[test]
fn a_mark_put_back_waits_for_its_host_until_its_claim() {
    let mut e = engine(&[("h", &[1]), ("t", &[])]);
    let mark = |request: &str, on: &str| Reservation {
        request: id(request),
        host: host(on),
        at: stamp(50),
        deadline: stamp(90_000),
        assigned: false,
        tail_end: None,
    };
    for (request, on) in [("r", "z"), ("q", "t"), ("p", "h")] {
        assert!(e.whole_restore_place(&id(request), stamp(0), at(0)));
        assert!(e.whole_revive(&mark(request, on), stamp(10_000)));
    }
    assert!(!e.whole_revive(&mark("o", "z"), stamp(10_000)));
    assert_eq!(held(&e, 0), ["p@h", "q@t", "r@z"]);
    present(&mut e, "r", 1, true, 100);
    let hosts = [("h", Standing::Live), ("t", Standing::Tainted)];
    pass(&mut e, &hosts, 9_999);
    assert_eq!(held(&e, 9_999), ["p@h", "r@z"]);
    assert_eq!(e.whole_expire(GRACE, at(9_999)), 0);
    present(&mut e, "p", 2, true, 10_000);
    pass(&mut e, &hosts, 10_000);
    assert_eq!(held(&e, 10_000), ["p@h"]);
    assert_eq!(e.whole_held(at(10_000))[0].at, stamp(50));
    assert!(e.whole_waiter(&id("p")).unwrap().marked);
    assert_eq!(e.whole_expire(GRACE, at(10_000)), 1);
}

/// Catches a retired host that stays set aside until the next round, and a mark put back on
/// it that stays. `r` marks the busy `a`; `a` is retired, and is no longer held.
#[test]
fn a_retired_host_is_let_go_at_once() {
    let mut e = engine(&[("a", &[1])]);
    present(&mut e, "r", 2, true, 0);
    pass(&mut e, &[("a", Standing::Live)], 20_000);
    assert_eq!(held(&e, 20_000), ["r@a"]);
    e.whole_drop_host(&host("a"), at(20_001));
    assert!(held(&e, 20_001).is_empty());
    assert!(e.whole_restore_place(&id("q"), stamp(0), at(0)));
    let kept = Reservation {
        request: id("q"),
        host: host("a"),
        at: stamp(0),
        deadline: stamp(0),
        assigned: false,
        tail_end: None,
    };
    assert!(e.whole_revive(&kept, stamp(60_000)));
    e.whole_drop_host(&host("a"), at(20_002));
    assert!(held(&e, 20_002).is_empty());
}

/// Catches a request that leaves the list carrying anything over (a record of it, or a
/// count on its next presentation), and a cancel that leaves its mark held. `r` holds the
/// busy `a` past its deadline and keeps the mark; once cancelled it holds nothing and has
/// no record, and presented again it is a fresh unmarked waiter.
#[test]
fn a_request_that_leaves_carries_nothing_over() {
    let mut e = engine(&[("a", &[1])]);
    present(&mut e, "r", 2, true, 0);
    pass(&mut e, &[("a", Standing::Live)], 10_000);
    assert_eq!(held(&e, 10_000), ["r@a"]);
    pass(&mut e, &[("a", Standing::Live)], 70_000);
    assert_eq!(held(&e, 70_000), ["r@a"]);
    assert_eq!(e.whole_waiter(&id("r")).unwrap().give_ups, 0);
    assert!(e.whole_cancel(&id("r")));
    assert!(held(&e, 70_001).is_empty());
    let key = RequestKey::Id(id("r"));
    assert_eq!(e.whole_record(&key), None);
    present(&mut e, "r", 3, true, 70_002);
    let again = e.whole_waiter(&id("r")).unwrap();
    assert_eq!((again.give_ups, again.marked), (0, false));
}

/// Catches the failed prepare of a lease the registry committed outside the queue counted
/// nowhere, one counted for another request, a request held by its own lease, a terminal
/// start answered more than once, a ready report that keeps the record, and a record of a
/// request with no id dropped while its lease is committed or kept past the idle horizon.
#[test]
fn the_records_of_the_leases_the_registry_commits_are_kept_by_key() {
    let mut e = engine(&[("a", &[1])]);
    pass(&mut e, &[("a", Standing::Live)], 0);
    let content = RequestKey::Content(request());
    let named = RequestKey::Id(id("n"));
    let failure = PrepareFailure {
        exclusion: Some(Exclusion::Until(stamp(5_000))),
        not_ready: NotReady {
            code: "setup-failed".into(),
            detail: "no image".into(),
        },
    };
    e.whole_commit(lease(7), &named);
    assert!(e.whole_held_by_other(&id("n"), &lease(8)));
    assert!(!e.whole_held_by_other(&id("n"), &lease(7)));
    e.prepare_ready(&lease(7));
    e.ended(lease(7));
    assert!(!e.whole_held_by_other(&id("n"), &lease(8)));
    assert!(!e.prepare_failed(&lease(7), &failure, at(10)));
    // Two failures of `max_replace` two: terminal, refused once.
    for seq in [10, 11] {
        e.whole_commit(lease(seq), &content);
        assert!(!e.whole_start(&content, at(seq)));
        assert!(e.prepare_failed(&lease(seq), &failure, at(seq)));
    }
    let record = e.whole_record(&content).unwrap();
    assert_eq!((record.prepare_failures, record.terminal), (2, true));
    assert_eq!(record.last.unwrap().detail, "no image");
    assert!(e.whole_start(&content, at(20)));
    assert!(!e.whole_start(&content, at(20)));
    assert_eq!(e.whole_record(&content), None);
    // A record held by a committed lease stays past the horizon; a ready report lets it go.
    e.whole_commit(lease(12), &content);
    assert!(e.prepare_failed(&lease(12), &failure, at(30)));
    e.whole_commit(lease(13), &content);
    e.whole_sweep(Duration::from_secs(1), at(10_000));
    assert!(e.whole_record(&content).is_some());
    e.prepare_ready(&lease(13));
    assert_eq!(e.whole_record(&content), None);
    e.whole_commit(lease(14), &named);
    assert!(e.prepare_failed(&lease(14), &failure, at(10_000)));
    e.whole_sweep(Duration::from_secs(1), at(10_999));
    assert!(e.whole_record(&named).is_some());
    e.whole_sweep(Duration::from_secs(1), at(11_000));
    assert_eq!(e.whole_record(&named), None);
}

/// Catches a rebuild that queues again a request that left the wait list since the last
/// pass: the new engine would grant it a host nobody presents it for until the next pass.
/// `r` is granted the quiet `h` and cancelled; the engine is rebuilt and ticks before any
/// pass, and holds nothing.
#[test]
fn a_rebuild_does_not_queue_again_a_request_that_left() {
    let mut e = engine(&[("h", &[])]);
    present(&mut e, "r", 1, true, 0);
    pass(&mut e, &[("h", Standing::Live)], 100);
    assert_eq!(held(&e, 100), ["r@h*"]);
    assert!(e.whole_cancel(&id("r")));
    let mut rebuilt = e.rebuild(Settings::default(), &[candidate("h", &[])], at(300));
    // A host retired elsewhere ticks the queue of the new engine before any pass.
    rebuilt.whole_drop_host(&host("x"), at(300));
    assert!(held(&rebuilt, 300).is_empty());
    assert!(rebuilt.whole_places().is_empty());
}

/// Catches a failed prepare of a lease the registry committed outside the queue that bars
/// nothing on the host it held (the request would be placed there again), and a lease the
/// engine committed from its queue counted twice once the registry reports the commit
/// too. `n`'s lease `1-1` holds `a` and fails: `a` is barred to `n`, and quiet. `r` is
/// committed on `a` from the queue, the registry reports it, and its failure counts once.
#[test]
fn a_failed_lease_bars_its_host_and_a_queued_commit_counts_once() {
    let mut e = engine(&[("a", &[1]), ("b", &[])]);
    pass(&mut e, &[("a", Standing::Live), ("b", Standing::Live)], 0);
    let failure = PrepareFailure {
        exclusion: Some(Exclusion::Until(stamp(5_000))),
        not_ready: NotReady {
            code: "setup-failed".into(),
            detail: String::new(),
        },
    };
    let named = RequestKey::Id(id("n"));
    e.whole_commit(lease(1), &named);
    assert!(e.prepare_failed(&lease(1), &failure, at(10)));
    let record = e.whole_record(&named).unwrap();
    let barred = BTreeMap::from([(host("a"), Exclusion::Until(stamp(5_000)))]);
    assert_eq!(record.exclusions, barred);
    // `r` is granted the first quiet host, `a`, and its start commits there from the queue.
    present(&mut e, "r", 2, true, 20);
    pass(&mut e, &[("a", Standing::Live), ("b", Standing::Live)], 20);
    assert_eq!(held(&e, 20), ["r@a*"]);
    let class = js_types::ResourceClass::whole_node();
    let asked = Request {
        class: class.id().clone(),
        ..request()
    };
    let spec = js_types::StartSpec::new(lease(2), asked, &[class])
        .unwrap()
        .with_request_id(id("r"));
    let open = [candidate("a", &[])];
    assert_eq!(e.commit_whole(&spec, &open, at(30)), Some(Some(0)));
    let queued = RequestKey::Id(id("r"));
    e.whole_commit(lease(2), &queued);
    assert!(e.whole_held_by_other(&id("r"), &lease(3)));
    assert!(e.prepare_failed(&lease(2), &failure, at(40)));
    assert_eq!(e.whole_record(&queued).unwrap().prepare_failures, 1);
    assert!(!e.prepare_failed(&lease(2), &failure, at(41)));
}
