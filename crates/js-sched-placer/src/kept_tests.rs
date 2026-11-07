//! What the engine keeps of each lease: the start and bound of a lease put back from the
//! lease file, those of every lease over a rebuild, and the whole-node standing of a
//! whole-node lease, read through the marks of the whole-node queue. The clock is the
//! test's: farm milliseconds passed by hand.

use std::collections::BTreeMap;
use std::time::Duration;

use js_registry::{
    LeaseTimes, MarkedTails, Reservation, SeatedLease, Standing, WaitingWhole, WholeHost,
    WholeNodeConfig, WholeRound,
};
use js_types::Timestamp;
use js_types::sched::FarmTime;
use js_types::{
    Admission, ClassId, HostId, LeaseId, Request as HostRequest, RequestId, ResourceClass,
    Resources, Shape,
};
use js_types::{Candidate, Held, StartSpec};

use super::Kept;
use crate::engine::{Engine, Settings};
use crate::map;

fn at(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn shape() -> Shape {
    let caps = [("arch".to_owned(), "x86_64".to_owned())]
        .into_iter()
        .collect();
    Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps)
}

/// What a lease that books the whole host books.
fn whole_host() -> Resources {
    shape().resources().clone()
}

/// `name`, carrying `leases`, each booking the whole host.
fn candidate(name: &str, leases: &[u64]) -> Candidate {
    let leases = leases.iter().map(|seq| Held {
        lease: LeaseId::new(1, *seq),
        resources: whole_host(),
        whole: true,
    });
    Candidate {
        host: host(name),
        shape: shape(),
        leases: leases.collect(),
    }
}

/// An engine built at zero over `names`, each said hello and carrying nothing.
fn engine(names: &[&str]) -> Engine {
    let every: Vec<Candidate> = names.iter().map(|name| candidate(name, &[])).collect();
    Engine::build(Settings::default(), &every, at(0))
}

/// A class that books a whole host without being the whole-node class.
fn book_all() -> ResourceClass {
    ResourceClass::new(ClassId::new("book").unwrap(), Admission::BookAll, 0)
}

/// The start of lease `1-<seq>` of `class`, bound to `wall`.
fn spec(seq: u64, class: &ResourceClass, wall: Duration) -> StartSpec {
    let request = HostRequest {
        class: class.id().clone(),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: wall,
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    StartSpec::new(LeaseId::new(1, seq), request, std::slice::from_ref(class)).unwrap()
}

/// Places lease `1-<seq>` of `class`, bound to `wall`, on `on` (offered alone) at `now`.
fn place(engine: &mut Engine, seq: u64, class: &ResourceClass, wall: Duration, on: &str, now: u64) {
    let open = [candidate(on, &[])];
    let nodes = Engine::nodes(&open).unwrap();
    let start = spec(seq, class, wall);
    let placed = engine.grant(&start, (&open, nodes), &MarkedTails::new(), at(now));
    assert_eq!(placed, Ok(Some(0)), "lease {seq} on {on}");
}

/// Lease `1-<seq>`, booking the whole host `on`, put back with `times` (start and end in
/// ms) as the lease file records them; a whole-node lease when `whole_node`.
fn seated(seq: u64, on: &str, times: Option<(u64, u64)>, whole_node: bool) -> SeatedLease {
    SeatedLease {
        host: host(on),
        held: Held {
            lease: LeaseId::new(1, seq),
            resources: whole_host(),
            whole: true,
        },
        times: times.map(|(start, end)| LeaseTimes {
            started_at: Timestamp::from_millis(start),
            ends_at: Timestamp::from_millis(end),
        }),
        whole_node,
    }
}

/// A round with request `r`, first presented at zero, over `names`, all live; a request
/// waits ten seconds before it marks a busy host.
fn round(names: &[&str]) -> WholeRound {
    let hosts = names.iter().map(|name| WholeHost {
        id: host(name),
        standing: Standing::Live,
        open: true,
    });
    let waiting = WaitingWhole {
        id: RequestId::new("r").unwrap(),
        max_wall: Duration::from_secs(600),
        since: Timestamp::from_millis(0),
        give_ups: 0,
        exclusions: BTreeMap::new(),
        mark: None,
    };
    WholeRound {
        settings: WholeNodeConfig {
            drain_after_ms: 10_000,
            drain_deadline_ms: Some(3_600_000),
            ..WholeNodeConfig::dedicated(4)
        },
        waiting: vec![waiting],
        hosts: hosts.collect(),
        outside: MarkedTails::new(),
        lives: BTreeMap::new(),
    }
}

/// The marks of `r` at `now`, each as its host and the end of its tail.
fn marks(engine: &mut Engine, names: &[&str], now: u64) -> Vec<(String, Option<u64>)> {
    let marks: Vec<Reservation> = engine.whole_round(round(names), at(now));
    let marks = marks.iter().map(|mark| {
        let tail = mark.tail_end;
        (mark.host.to_string(), tail)
    });
    marks.collect()
}

/// Every host of `names` heard from at `now`, as their heartbeats are.
fn beat(engine: &mut Engine, names: &[&str], now: u64) {
    for name in names {
        engine.heard(&host(name), None, at(now));
    }
}

fn minutes(n: u64) -> Duration {
    Duration::from_secs(60 * n)
}

/// Catches: a lease put back from the lease file costed as started at the put-back, or with
/// no bound (it would read as ending at the latest time there is, and its host would drain
/// last). Lease 1 is put back on `a` as started at zero and bound to end at 20 minutes; at
/// five minutes lease 2 is placed on `b` for ten. The request marks `a`, whose lease ends
/// first, with that end as its tail: counted from the put-back, or from no bound, `b` would
/// be marked.
#[test]
fn a_seated_lease_keeps_its_recorded_start_and_bound() {
    let mut e = engine(&["a", "b"]);
    beat(&mut e, &["a", "b"], 300_000);
    e.seat(&seated(1, "a", Some((0, 1_200_000)), false));
    let kept = e.kept(&LeaseId::new(1, 1));
    assert_eq!(kept, Some(Kept::started(at(0), minutes(20), false)));
    place(&mut e, 2, &book_all(), minutes(10), "b", 300_000);
    // `a`'s lease ends at 20 minutes, `b`'s at fifteen: `b` is marked.
    assert_eq!(
        marks(&mut e, &["a", "b"], 310_000),
        [("b".into(), Some(900_000))]
    );
}

/// Catches: an end bound before the start read as a wall time that wraps (the lease would
/// read as never ending). Lease 1 is put back bound to end before it started: it has no
/// time left, and its host `a` is marked with no wait.
#[test]
fn a_seated_lease_bound_before_its_start_has_no_time_left() {
    let mut e = engine(&["a", "b"]);
    e.seat(&seated(1, "a", Some((60_000, 0)), false));
    let kept = e.kept(&LeaseId::new(1, 1));
    assert_eq!(kept, Some(Kept::started(at(60_000), Duration::ZERO, false)));
    place(&mut e, 2, &book_all(), minutes(10), "b", 0);
    assert_eq!(
        marks(&mut e, &["a", "b"], 10_000),
        [("a".into(), Some(60_000))]
    );
}

/// Catches: a lease put back with no times (a caller that records none) given a start it
/// does not have, or read as never ending (its host would drain last). It counts as ending
/// at the put-back, as the registry counts it: its host `a` is marked ahead of `b`, whose
/// lease is bound, with no wait.
#[test]
fn a_seated_lease_without_times_ends_at_its_put_back() {
    let mut e = engine(&["a", "b"]);
    beat(&mut e, &["a", "b"], 5_000);
    e.seat(&seated(1, "a", None, false));
    let now = Kept {
        start: None,
        max_wall: Duration::ZERO,
        whole_node: false,
    };
    assert_eq!(e.kept(&LeaseId::new(1, 1)), Some(now));
    place(&mut e, 2, &book_all(), minutes(10), "b", 5_000);
    assert_eq!(
        marks(&mut e, &["a", "b"], 10_000),
        [("a".into(), Some(5_000))]
    );
}

/// Catches: a lease put back twice (or one the engine already holds) taken up twice, which
/// would book its host twice. The second put-back changes nothing, the first times kept.
#[test]
fn a_lease_seated_twice_is_taken_up_once() {
    let mut e = engine(&["a"]);
    e.seat(&seated(1, "a", Some((0, 600_000)), false));
    e.seat(&seated(1, "a", Some((0, 1_200_000)), true));
    let worker = map::worker(&host("a"));
    assert_eq!(e.sched().leases_on(&worker).len(), 1);
    let kept = e.kept(&LeaseId::new(1, 1));
    assert_eq!(kept, Some(Kept::started(at(0), minutes(10), false)));
}

/// Catches: a rebuild that drops the wall times and starts of the leases the engine placed
/// (each would read as starting at the rebuild with no bound, an unknown tail). Lease 1 is
/// placed on `a` for ten minutes at zero and lease 2 on `b` for an hour; the engine is built
/// again at five minutes from the registry's books. The request marks `a` with its tail
/// ending at ten minutes, counted from the placement, not the rebuild.
#[test]
fn a_rebuild_keeps_each_leases_start_and_wall_time() {
    let mut e = engine(&["a", "b"]);
    place(&mut e, 1, &book_all(), minutes(10), "a", 0);
    place(&mut e, 2, &book_all(), minutes(60), "b", 0);
    let every = [candidate("a", &[1]), candidate("b", &[2])];
    let mut rebuilt = e.rebuild(Settings::default(), &every, at(300_000));
    let kept = rebuilt.kept(&LeaseId::new(1, 1));
    assert_eq!(kept, Some(Kept::started(at(0), minutes(10), false)));
    assert_eq!(
        marks(&mut rebuilt, &["a", "b"], 310_000),
        [("a".into(), Some(600_000))]
    );
}

/// Catches: a rebuild that gives a lease the engine never held a start or a wall time it
/// does not have (or fails on it). Lease 7 is on `a` in the registry's books only: the
/// rebuilt engine holds it, keeps nothing of it, and still keeps lease 1's times.
#[test]
fn a_rebuild_keeps_nothing_of_a_lease_the_old_engine_never_held() {
    let mut e = engine(&["a", "b"]);
    place(&mut e, 1, &book_all(), minutes(10), "b", 0);
    let every = [candidate("a", &[7]), candidate("b", &[1])];
    let rebuilt = e.rebuild(Settings::default(), &every, at(1_000));
    let worker = map::worker(&host("a"));
    assert_eq!(rebuilt.sched().leases_on(&worker).len(), 1);
    assert_eq!(rebuilt.kept(&LeaseId::new(1, 7)), None);
    let kept = rebuilt.kept(&LeaseId::new(1, 1));
    assert_eq!(kept, Some(Kept::started(at(0), minutes(10), false)));
}

/// Catches: a lease the registry stopped still kept, so a rebuild would give its times to
/// nothing, or a later lease of the same id. Once lease 1 ends the engine keeps nothing of
/// it, and keeps lease 2.
#[test]
fn an_ended_lease_is_no_longer_kept() {
    let mut e = engine(&["a", "b"]);
    place(&mut e, 1, &book_all(), minutes(10), "a", 0);
    place(&mut e, 2, &book_all(), minutes(10), "b", 0);
    e.ended(LeaseId::new(1, 1));
    assert_eq!(e.kept(&LeaseId::new(1, 1)), None);
    assert!(e.kept(&LeaseId::new(1, 2)).is_some());
}

/// Catches: a lease the registry booked without the engine placing it (the fallback of a
/// start the engine cannot describe) taken up with no bound and no whole-node standing. It is
/// kept as started at the take-up with the request's wall time, a whole-node lease when its
/// class is the whole-node class.
#[test]
fn an_adopted_lease_keeps_its_wall_time_and_whole_node_standing() {
    let mut e = engine(&["a", "b"]);
    e.feed(at(5_000), js_sched::Event::Tick);
    e.adopt(&candidate("a", &[]), &spec(1, &book_all(), minutes(10)));
    let whole = ResourceClass::whole_node();
    e.adopt(&candidate("b", &[]), &spec(2, &whole, minutes(10)));
    let book = e.kept(&LeaseId::new(1, 1));
    assert_eq!(book, Some(Kept::started(at(5_000), minutes(10), false)));
    let node = e.kept(&LeaseId::new(1, 2));
    assert_eq!(node, Some(Kept::started(at(5_000), minutes(10), true)));
}

/// Catches: a host held by a whole-node lease placed without a request id taken for a host
/// a whole-node request may drain (it would wait behind another request's whole node), and a
/// lease of a class that only books the whole host taken for one (no host held that way
/// could ever be drained). `a`'s whole-node lease ends after one minute, `b`'s whole-host
/// lease after ten: the request marks `b`, never `a`.
#[test]
fn a_host_a_whole_node_lease_holds_is_never_marked() {
    let mut e = engine(&["a", "b"]);
    place(&mut e, 1, &ResourceClass::whole_node(), minutes(1), "a", 0);
    place(&mut e, 2, &book_all(), minutes(10), "b", 0);
    assert_eq!(
        marks(&mut e, &["a", "b"], 10_000),
        [("b".into(), Some(600_000))]
    );
}

/// Catches: the whole-node standing lost where a lease is taken up rather than placed: at a
/// put-back from the lease file, and over a rebuild. With whole-node leases on both hosts
/// (one put back, one placed) nothing is marked, before and after the engine is built
/// again.
#[test]
fn whole_node_leases_seated_or_rebuilt_still_hold_their_hosts() {
    let mut e = engine(&["a", "b"]);
    e.seat(&seated(1, "a", Some((0, 60_000)), true));
    place(&mut e, 2, &ResourceClass::whole_node(), minutes(10), "b", 0);
    assert_eq!(marks(&mut e, &["a", "b"], 10_000), []);
    let every = [candidate("a", &[1]), candidate("b", &[2])];
    let mut rebuilt = e.rebuild(Settings::default(), &every, at(20_000));
    assert_eq!(marks(&mut rebuilt, &["a", "b"], 30_000), []);
}
