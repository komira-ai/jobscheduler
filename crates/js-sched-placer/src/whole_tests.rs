//! The whole-node queue of the engine, driven as the registry drives it: a round per pass
//! with the registry's wait list, standing of the hosts and lives, and the start of each
//! request. The clock is the test's: farm milliseconds passed by hand.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_lease::Timestamp;
use js_provider::{Candidate, Held, StartSpec};
use js_registry::{
    Exclusion, MarkedTails, Reservation, Standing, WaitingWhole, WholeHost, WholeNodeConfig,
    WholeRound,
};
use js_sched::{Event, NodePhase, OpState};
use js_types::sched::{FarmTime, LeaseId as SchedLeaseId, WorkerId};
use js_types::{
    Admission, ClassId, HostId, LeaseId, Request as HostRequest, RequestId, ResourceClass,
    Resources, Shape,
};

use super::WHOLE_TERM;
use crate::engine::{Engine, Settings};
use crate::map;

fn at(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn id(name: &str) -> RequestId {
    RequestId::new(name).unwrap()
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

/// The settings of the rounds: a request waits ten seconds before it marks a busy host,
/// and a mark is given up after a minute.
fn settings() -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms: 10_000,
        drain_deadline_ms: Some(60_000),
        ..WholeNodeConfig::dedicated(4)
    }
}

/// Request `name`, first presented at `since` ms, that may run ten minutes.
fn waiting(name: &str, since: u64) -> WaitingWhole {
    WaitingWhole {
        id: id(name),
        max_wall: Duration::from_secs(600),
        since: Timestamp::from_millis(since),
        give_ups: 0,
        exclusions: BTreeMap::new(),
        mark: None,
    }
}

/// A round of `waiting` over `hosts`, standing as given; a live host is one the registry
/// would book (a busy one too: the engine reads only whether an idle host is bookable).
fn round(waiting: Vec<WaitingWhole>, hosts: &[(&str, Standing)]) -> WholeRound {
    let hosts = hosts.iter().map(|(name, standing)| WholeHost {
        id: host(name),
        standing: *standing,
        open: *standing == Standing::Live,
    });
    WholeRound {
        settings: settings(),
        waiting,
        hosts: hosts.collect(),
        outside: MarkedTails::new(),
        lives: BTreeMap::new(),
    }
}

/// Every host of `names` live.
fn live(names: &[&'static str]) -> Vec<(&'static str, Standing)> {
    names.iter().map(|name| (*name, Standing::Live)).collect()
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
    let spec = StartSpec::new(LeaseId::new(1, seq), request, &[class]).unwrap();
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

/// Every host of `names` heard from at `now`, as their heartbeats are.
fn beat(engine: &mut Engine, names: &[&str], now: u64) {
    for name in names {
        engine.heard(&host(name), None, at(now));
    }
}

/// Catches: a quiet host left unmarked until `drain_after`, a pending grant shown as a
/// draining mark (the hosts API would show a deadline), or the request's start booking
/// nothing or another host; and a committed lease left held when the registry stops it.
/// `b` is quiet: the request is granted it at once, shown assigned; its start books `b`
/// as the registry's lease, which the engine then holds and lets go when it ends.
#[test]
fn a_quiet_host_is_granted_at_once_and_the_start_commits_there() {
    let mut e = engine(&[("a", &[1]), ("b", &[])]);
    let first = e.whole_round(round(vec![waiting("r", 0)], &live(&["a", "b"])), at(1_000));
    let assigned = Reservation {
        request: id("r"),
        host: host("b"),
        at: Timestamp::from_millis(1_000),
        deadline: Timestamp::from_millis(1_000),
        assigned: true,
        tail_end: None,
    };
    assert_eq!(first, [assigned]);
    let b = map::worker(&host("b"));
    let pending = e.pending_on(&b);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].term, WHOLE_TERM);
    // Another start is no start of the request: the engine answers nothing for it.
    let other = whole_spec(8, "s");
    assert_eq!(
        e.commit_whole(&other, &[candidate("b", &[])], at(1_000)),
        None
    );
    let open = [candidate("a", &[1]), candidate("b", &[])];
    let committed = e.commit_whole(&whole_spec(7, "r"), &open, at(2_000));
    assert_eq!(committed, Some(Some(1)));
    assert!(e.whole.is_empty());
    assert_eq!(e.on[&host("b")][&LeaseId::new(1, 7)].lease, pending[0]);
    // The request has left the wait list: the round lets go of nothing it committed.
    assert!(
        e.whole_round(round(Vec::new(), &live(&["a", "b"])), at(3_000))
            .is_empty()
    );
    assert_eq!(e.sched.leases_on(&b), pending);
    e.ended(LeaseId::new(1, 7));
    assert!(e.sched.leases_on(&b).is_empty());
}

/// Catches: the drain cost read without the registry's lives (the engine would drain the
/// host the registry's own pass would not), or a mark made before `drain_after`. Both
/// hosts are busy with leases of unknown end. Without lives the mark goes to the first by
/// name, `a`, with no known tail; with `b`'s lease expected to last a minute from its
/// start, the mark goes to `b`, its tail ending then. Before `drain_after` nothing is
/// marked, and a start of the request books nothing.
#[test]
fn a_busy_pool_is_marked_after_drain_after_by_the_registrys_lives() {
    let hosts = live(&["a", "b"]);
    let mut plain = engine(&[("a", &[1]), ("b", &[2])]);
    let early = plain.whole_round(round(vec![waiting("r", 0)], &hosts), at(1_000));
    assert!(early.is_empty());
    let open = [candidate("a", &[1])];
    assert_eq!(
        plain.commit_whole(&whole_spec(7, "r"), &open, at(1_000)),
        Some(None)
    );
    let marks = plain.whole_round(round(vec![waiting("r", 0)], &hosts), at(11_000));
    assert_eq!(held(&marks), ["r@a"]);
    assert_eq!(marks[0].tail_end, None);
    assert_eq!(marks[0].deadline, Timestamp::from_millis(71_000));
    let mut known = engine(&[("a", &[1]), ("b", &[2])]);
    let mut lived = round(vec![waiting("r", 0)], &hosts);
    lived.lives = BTreeMap::from([(LeaseId::new(1, 2), 1)]);
    known.whole_round(lived.clone(), at(1_000));
    let marks = known.whole_round(lived, at(11_000));
    assert_eq!(held(&marks), ["r@b"]);
    assert_eq!(marks[0].tail_end, Some(60_000));
}

/// Catches: a mark that moves to a host that has become cheaper to drain while its
/// request waits (the mark ruling: it stays until its request is done), and a mark that
/// outlives its request. `r` marks `a`; then `b`'s lease is learned to end soon, and the
/// mark stays on `a`. Once `r` leaves the wait list the mark is gone in the same round.
#[test]
fn a_mark_stays_on_its_host_and_goes_with_its_request() {
    let hosts = live(&["a", "b"]);
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let marks = e.whole_round(round(vec![waiting("r", 0)], &hosts), at(11_000));
    assert_eq!(held(&marks), ["r@a"]);
    let op = e.whole[&id("r")];
    let mut cheaper = round(vec![waiting("r", 0)], &hosts);
    cheaper.lives = BTreeMap::from([(LeaseId::new(1, 2), 1)]);
    let marks = e.whole_round(cheaper, at(20_000));
    assert_eq!(held(&marks), ["r@a"]);
    let gone = e.whole_round(round(Vec::new(), &hosts), at(21_000));
    assert!(gone.is_empty());
    assert!(e.whole.is_empty());
    assert_eq!(e.sched.whole_mark(op), None);
    assert_eq!(e.sched.state(op), None);
}

/// Catches: the engine's own queue order used in place of the registry's wait list (the
/// requests would be served by name). `zeta` was presented before `alpha`: the one quiet
/// host goes to `zeta`.
#[test]
fn the_queue_order_is_the_wait_lists() {
    let hosts = live(&["a", "b"]);
    let mut e = engine(&[("a", &[]), ("b", &[2])]);
    let queue = vec![waiting("zeta", 0), waiting("alpha", 5)];
    let marks = e.whole_round(round(queue, &hosts), at(6_000));
    assert_eq!(held(&marks), ["zeta@a*"]);
}

/// Catches: a lost, tainted, leaving or cordoned host still granted or marked (a request
/// would wait on a host that cannot take it), a pending grant left on such a host, or a
/// host the engine cordoned left cordoned once the registry stands it live again. `r` is
/// granted `a`; `a` goes lost and `r` is granted `b` at once; then `a` is live again while
/// `b` is tainted, `c` leaving and `d` cordoned, and `r` is granted `a` again.
#[test]
fn the_registrys_standing_moves_a_grant_at_once() {
    let names = ["a", "b", "c", "d"];
    let mut e = engine(&[("a", &[]), ("b", &[]), ("c", &[]), ("d", &[])]);
    let marks = e.whole_round(round(vec![waiting("r", 0)], &live(&names)), at(1_000));
    assert_eq!(held(&marks), ["r@a*"]);
    let lost = [
        ("a", Standing::Lost),
        ("b", Standing::Live),
        ("c", Standing::Live),
        ("d", Standing::Live),
    ];
    let marks = e.whole_round(round(vec![waiting("r", 0)], &lost), at(2_000));
    assert_eq!(held(&marks), ["r@b*"]);
    let a = map::worker(&host("a"));
    assert!(e.sched.cordon(&a).is_some());
    let others = [
        ("a", Standing::Live),
        ("b", Standing::Tainted),
        ("c", Standing::Leaving),
        ("d", Standing::Cordoned),
    ];
    let marks = e.whole_round(round(vec![waiting("r", 0)], &others), at(3_000));
    assert_eq!(held(&marks), ["r@a*"]);
    assert!(e.sched.cordon(&a).is_none());
    let d = map::worker(&host("d"));
    assert!(e.sched.cordon(&d).is_some());
    // The leaving `c` and the cordoned `d` carry nothing the registry would book.
    assert_eq!(e.cordoned, BTreeSet::from([host("c"), host("d")]));
    // Once the queue is empty the engine lets go of the cordons and phases it set.
    e.whole_round(round(Vec::new(), &others), at(4_000));
    e.whole_round(round(Vec::new(), &others), at(5_000));
    assert!(e.sched.cordon(&d).is_none());
    let b = map::worker(&host("b"));
    assert_eq!(e.sched.phase(&b), Some(&NodePhase::Serving));
    assert!(e.cordoned.is_empty() & e.phased.is_empty());
}

/// Catches: a host the registry would not book now (restoring after a whole-node lease, a
/// run still listed) granted to a request while another it would book is free (the
/// request would wait on it, and on a host whose prepares fail, fail again), or kept from
/// every request when no other is free (a request could then hold no host while it waits,
/// and the hosts listing would show none set aside for it). `a` is restoring and `b` is
/// bookable: `r` is granted `b`; then, with no bookable host free, `s` is granted `a`.
#[test]
fn a_host_the_registry_would_not_book_waits_for_the_free_ones() {
    let mut e = engine(&[("a", &[]), ("b", &[]), ("c", &[1])]);
    let mut hosts = round(
        vec![waiting("r", 0), waiting("s", 0)],
        &live(&["a", "b", "c"]),
    );
    hosts.hosts[0].open = false;
    hosts.hosts[2].open = false;
    assert_eq!(held(&e.whole_round(hosts.clone(), at(1_000))), ["r@b*"]);
    let a = map::worker(&host("a"));
    assert!(e.sched.cordon(&a).is_some());
    let marks = e.whole_round(hosts, at(2_000));
    assert_eq!(held(&marks), ["r@b*", "s@a*"]);
    assert!(e.sched.cordon(&a).is_none() & e.cordoned.is_empty());
}

/// Catches: the hosts a request's failed prepares left out offered to it (as a mark or a
/// grant), or an exclusion read as lasting longer than it does. `a` is left out for life
/// and `b` until 5 s: `r` is granted `c`. A second request left out of `a` until 5 s, with
/// only `a` quiet after `c` is taken, is granted nothing before 5 s and `a` after.
#[test]
fn the_hosts_a_failed_prepare_left_out_are_never_offered() {
    let hosts = live(&["a", "b", "c"]);
    let mut e = engine(&[("a", &[]), ("b", &[]), ("c", &[])]);
    let mut r = waiting("r", 0);
    r.exclusions = BTreeMap::from([
        (host("a"), Exclusion::ForLife),
        (host("b"), Exclusion::Until(Timestamp::from_millis(5_000))),
    ]);
    let marks = e.whole_round(round(vec![r.clone()], &hosts), at(1_000));
    assert_eq!(held(&marks), ["r@c*"]);
    let mut s = waiting("s", 1_000);
    s.exclusions = BTreeMap::from([
        (host("a"), Exclusion::Until(Timestamp::from_millis(5_000))),
        (host("b"), Exclusion::ForLife),
    ]);
    let marks = e.whole_round(round(vec![r.clone(), s.clone()], &hosts), at(2_000));
    assert_eq!(held(&marks), ["r@c*"]);
    let marks = e.whole_round(round(vec![r, s], &hosts), at(5_000));
    assert_eq!(held(&marks), ["r@c*", "s@a*"]);
}

/// Catches: a mark the registry holds for a request (put back from the ledger, or left by
/// the last round of an engine since rebuilt) not taken when the engine first queues the
/// request: it would mark afresh, with a new deadline and tail. The mark on `a` is kept
/// as it was; a mark on a host the engine does not know is let go, and the request marks
/// a host it knows; a pending grant shown by the registry is not taken as a mark.
#[test]
fn a_mark_the_registry_holds_is_taken_when_the_request_is_queued() {
    let hosts = live(&["a", "b"]);
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    let kept = Reservation {
        request: id("r"),
        host: host("a"),
        at: Timestamp::from_millis(500),
        deadline: Timestamp::from_millis(90_000),
        assigned: false,
        tail_end: Some(70_000),
    };
    let mut r = waiting("r", 0);
    r.mark = Some(kept.clone());
    let marks = e.whole_round(round(vec![r], &hosts), at(11_000));
    assert_eq!(marks, [kept]);
    let mut stray = engine(&[("a", &[1]), ("b", &[2])]);
    let mut s = waiting("s", 0);
    s.mark = Some(Reservation {
        request: id("s"),
        host: host("gone"),
        at: Timestamp::from_millis(500),
        deadline: Timestamp::from_millis(90_000),
        assigned: false,
        tail_end: None,
    });
    let marks = stray.whole_round(round(vec![s.clone()], &hosts), at(11_000));
    assert_eq!(held(&marks), ["s@a"]);
    let mut pending = engine(&[("a", &[1]), ("b", &[2])]);
    s.mark = Some(Reservation {
        host: host("b"),
        assigned: true,
        ..s.mark.clone().unwrap()
    });
    let marks = pending.whole_round(round(vec![s], &hosts), at(1_000));
    assert!(marks.is_empty());
}

/// Catches: a rebuild that drops the whole-node queue (the requests would lose their
/// marks, their give-ups and their places) or numbers its leases afresh. `r` gives up its
/// mark on `a` at the deadline and marks `b`; the rebuilt engine queues it with the same
/// mark and one give-up, and an engine with no queue rebuilds with none.
#[test]
fn a_rebuild_keeps_the_queue_its_marks_and_give_ups() {
    let hosts = live(&["a", "b"]);
    let every = [candidate("a", &[1]), candidate("b", &[2])];
    let mut e = Engine::build(Settings::default(), &every, at(0));
    let empty = e.rebuild(Settings::default(), &every, at(0));
    assert!(empty.whole.is_empty() & empty.round.is_none());
    for now in (11_000..=72_000).step_by(5_000) {
        beat(&mut e, &["a", "b"], now);
        e.whole_round(round(vec![waiting("r", 0)], &hosts), at(now));
    }
    let op = e.whole[&id("r")];
    assert_eq!(e.sched.whole_give_ups(op), 1);
    let marks = e.whole_round(round(vec![waiting("r", 0)], &hosts), at(72_000));
    assert_eq!(held(&marks), ["r@b"]);
    let mut rebuilt = e.rebuild(Settings::default(), &every, at(72_000));
    let again = rebuilt.whole[&id("r")];
    assert_eq!(rebuilt.sched.whole_give_ups(again), 1);
    assert_eq!(rebuilt.sched.whole_mark(again), e.sched.whole_mark(op));
    assert_eq!(rebuilt.whole_seq, e.whole_seq);
    let marks = rebuilt.whole_round(round(vec![waiting("r", 0)], &hosts), at(73_000));
    assert_eq!(held(&marks), ["r@b"]);
}

/// Catches: a whole-node request granted by a round that is not its own keeping that
/// grant, numbered as the registry's next lease or the next job (the start or job placed
/// in that round would then be numbered after it). `r` marks `a`; `a`'s lease ends and a
/// plain tick grants `a` to `r` in the registry's term: it is put back at once, and the
/// next whole round grants it in the whole-node term.
#[test]
fn a_grant_outside_the_whole_round_is_put_back() {
    let hosts = live(&["a", "b"]);
    let mut e = engine(&[("a", &[1]), ("b", &[2])]);
    e.whole_round(round(vec![waiting("r", 0)], &hosts), at(11_000));
    let op = e.whole[&id("r")];
    e.ended(LeaseId::new(1, 1));
    e.feed(at(12_000), Event::Tick);
    assert_eq!(e.sched.state(op), Some(&OpState::Queued));
    let marks = e.whole_round(round(vec![waiting("r", 0)], &hosts), at(13_000));
    assert_eq!(held(&marks), ["r@a*"]);
    let a = map::worker(&host("a"));
    assert_eq!(e.pending_on(&a)[0].term, WHOLE_TERM);
}

/// Catches: a pending grant given up because the host's heartbeats do not list it (the
/// request would lose its quiet host every few seconds), or kept on a host the registry
/// has forgotten. `r` is granted `b`, which beats for two minutes and keeps it; once `b` is
/// forgotten, `r` waits again and `b` is no worker.
#[test]
fn a_pending_grant_is_kept_by_the_heartbeats_and_let_go_with_its_host() {
    let hosts = live(&["a", "b"]);
    let mut e = engine(&[("a", &[1]), ("b", &[])]);
    e.whole_round(round(vec![waiting("r", 0)], &hosts), at(1_000));
    let b = map::worker(&host("b"));
    for now in (5_000..=125_000).step_by(5_000) {
        beat(&mut e, &["a", "b"], now);
        e.feed(at(now), Event::Tick);
    }
    assert_eq!(e.pending_on(&b).len(), 1);
    e.host_gone(&host("b"));
    let op = e.whole[&id("r")];
    assert_eq!(e.sched.state(op), Some(&OpState::Queued));
    assert!(e.sched.capacity(&b).is_none());
}

/// Catches: a host held for a request not presented since a restart taken by another, or
/// the engine holding a host its own request holds outside itself (the request could then
/// not take it). `a` is held outside: `r` is granted `b`. Told the marks the registry
/// shows (both), the engine holds `a` outside and not `b`.
#[test]
fn a_host_held_outside_is_left_and_the_engines_own_is_not_held_outside() {
    let mut e = engine(&[("a", &[]), ("b", &[])]);
    let mut first = round(vec![waiting("r", 0)], &live(&["a", "b"]));
    first.outside = MarkedTails::from([(host("a"), Some(Duration::from_secs(600)))]);
    let marks = e.whole_round(first, at(1_000));
    assert_eq!(held(&marks), ["r@b*"]);
    let shown = MarkedTails::from([(host("a"), None), (host("b"), None)]);
    e.mark(&shown);
    assert_eq!(e.outside, BTreeSet::from([host("a")]));
}

/// Catches: a request whose operation the scheduler finished (refused as unservable)
/// left without one while it still waits. Its operation is gone: the next round queues it
/// again under a new one, and it is granted the quiet host.
#[test]
fn a_request_the_scheduler_dropped_is_queued_again() {
    let hosts = live(&["a", "b"]);
    let mut e = engine(&[("a", &[]), ("b", &[1])]);
    e.whole_round(round(vec![waiting("r", 0)], &hosts), at(1_000));
    let op = e.whole[&id("r")];
    let _ = e.sched.cancel_whole(op);
    let marks = e.whole_round(round(vec![waiting("r", 0)], &hosts), at(2_000));
    assert_ne!(e.whole[&id("r")], op);
    assert_eq!(held(&marks), ["r@a*"]);
}

/// Catches: the probe of a host the engine cordons for the registry uncordoning it, so a
/// whole-node request could be granted a host the registry stands lost or cordoned.
#[test]
fn a_probe_leaves_a_host_the_engine_cordons_cordoned() {
    let mut e = engine(&[("a", &[]), ("b", &[1]), ("c", &[])]);
    let hosts = [
        ("a", Standing::Cordoned),
        ("b", Standing::Live),
        ("c", Standing::Live),
    ];
    e.whole_round(round(vec![waiting("r", 0)], &hosts), at(1_000));
    let id = ClassId::new("share").unwrap();
    let share = ResourceClass::new(id, Admission::Book(Default::default()), 0);
    let request = HostRequest {
        class: share.id().clone(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: Some(host("a")),
    };
    let probe = StartSpec::new(LeaseId::new(1, 9), request, &[share]).unwrap();
    let open = [candidate("a", &[])];
    let nodes = Engine::nodes(&open).unwrap();
    let placed = e.grant(&probe, (&open, nodes), &MarkedTails::new(), at(2_000));
    assert_eq!(placed, Ok(Some(0)));
    let a: WorkerId = map::worker(&host("a"));
    assert!(e.sched.cordon(&a).is_some());
    let leases: Vec<SchedLeaseId> = e.sched.leases_on(&a);
    assert_eq!(leases.len(), 1);
}
