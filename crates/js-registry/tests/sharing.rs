//! Leases that share a host, with a placer set: the class that books part of a host is
//! admitted, the placer picks among the hosts with room, a host that lists the runs of
//! its first lease takes a second, a restored lease keeps what it booked, and every
//! lease of a host counts toward its tail. Without a placer none of it changes.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_pool_static::Candidate;
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{Placer, PoolConfig};
use js_types::{Admission, ClassId, Dims, Request, ResourceClass, Resources};
use support::world::*;
use support::*;

fn book() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("book").unwrap(),
        Admission::Book(Dims::new()),
        0,
    )
}

fn sharing_spec(sequence: u64, wall_s: u64) -> StartSpec {
    let request = Request {
        class: book().id().clone(),
        max_wall: Duration::from_secs(wall_s),
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[book()]).unwrap()
}

/// A placer that books the last candidate it is shown, and remembers which hosts it was
/// shown at each start.
#[derive(Default)]
struct Last(Mutex<Vec<Vec<String>>>);

impl Placer for Last {
    fn pick(&self, _: &StartSpec, open: &[Candidate]) -> Option<usize> {
        let shown = open.iter().map(|c| c.host.to_string()).collect();
        self.0.lock().unwrap().push(shown);
        open.len().checked_sub(1)
    }
}

/// A placer that books the host named for the lease's sequence.
struct Named(Vec<(u64, &'static str)>);

impl Placer for Named {
    fn pick(&self, spec: &StartSpec, open: &[Candidate]) -> Option<usize> {
        let sequence = spec.lease().sequence();
        let (_, want) = self.0.iter().find(|(n, _)| *n == sequence)?;
        open.iter().position(|c| c.host.as_str() == *want)
    }
}

struct Pool {
    fx: Fixture,
    provider: js_registry::RegisteredProvider,
}

/// A pool of `book` leases (4 cores, 8 GiB per host; a lease asks for one core) with the
/// hosts `hosts` joined.
fn pool(hosts: &[&str]) -> Pool {
    let name = js_types::ProviderId::new("builders").unwrap();
    let fx = fixture_of(PoolConfig::new(name, book(), pool_shape(), 4));
    hosts.iter().for_each(|h| {
        fx.join(h, "t");
    });
    let provider = fx.registry.provider();
    Pool { fx, provider }
}

fn on(ack: &StartAck) -> Option<&str> {
    started_on(ack)
}

/// Catches: the flag-off state changing. With no placer the class that books part of a
/// host is refused `UnsupportedClass`, as it always was, and nothing is booked.
#[test]
fn without_a_placer_a_class_that_shares_is_refused() {
    let mut p = pool(&["a"]);
    let ack = p.provider.start(sharing_spec(1, 600));
    assert_eq!(ack, StartAck::Refused(RefusalCode::UnsupportedClass));
    assert!(p.provider.booked().is_empty());
}

/// Catches: a placer that does not open the class, or a second lease refused though the
/// host has room: two leases start on the one host, the capacity listing shows both in the
/// order they were booked, and the fifth, past the host's four cores, is `NoCapacity`.
#[test]
fn with_a_placer_a_second_lease_starts_beside_the_first() {
    let mut p = pool(&["a"]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let first = p.provider.start(sharing_spec(1, 600));
    let second = p.provider.start(sharing_spec(2, 600));
    assert_eq!((on(&first), on(&second)), (Some("a"), Some("a")));
    assert_eq!(p.provider.capacity()[0].leases, vec![lease(1), lease(2)]);
    for sequence in 3..=4 {
        p.provider.start(sharing_spec(sequence, 600));
    }
    assert_eq!(
        p.provider.start(sharing_spec(5, 600)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert_eq!(p.provider.capacity()[0].leases.len(), 4);
}

/// Catches: the placer not consulted, shown a host that has no room, or the registry
/// booking the first host by id: the placer takes the last candidate, so four leases fill
/// `b`; it is shown both hosts until `b` is full and then `a` alone, and the fifth lease
/// lands on `a`.
#[test]
fn the_placer_picks_among_the_hosts_with_room() {
    let mut p = pool(&["a", "b"]);
    let last = Arc::new(Last::default());
    p.fx.registry.set_placer(last.clone());
    let acks: Vec<StartAck> = (1..=5)
        .map(|n| p.provider.start(sharing_spec(n, 600)))
        .collect();
    let hosts: Vec<Option<&str>> = acks.iter().map(on).collect();
    assert_eq!(
        hosts,
        vec![Some("b"), Some("b"), Some("b"), Some("b"), Some("a")]
    );
    let seen = last.0.lock().unwrap().clone();
    let both = vec!["a".to_string(), "b".to_string()];
    assert_eq!(seen[..4], vec![both; 4]);
    assert_eq!(seen[4], vec!["a".to_string()]);
}

/// Catches: a placer that declines, or one that names a host past the candidates, booking
/// anyway: the start is `NoCapacity` and nothing is booked.
#[test]
fn a_placer_that_picks_none_books_none() {
    let mut p = pool(&["a"]);
    p.fx.registry
        .set_placer(Arc::new(Named(vec![(1, "nowhere")])));
    assert_eq!(
        p.provider.start(sharing_spec(1, 600)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert!(p.provider.booked().is_empty());
}

/// Catches: the quiet rule applied to a lease that shares a host that is busy with its
/// first lease: a run of lease 1 is live on `a`, so the host is not quiet, and lease 2
/// still starts there.
#[test]
fn a_host_running_the_first_lease_takes_a_second() {
    let mut p = pool(&["a"]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    assert_eq!(on(&p.provider.start(sharing_spec(1, 600))), Some("a"));
    p.fx.registry
        .queue_run(&host("a"), &lease(1), job("r1"))
        .unwrap();
    assert_eq!(on(&p.provider.start(sharing_spec(2, 600))), Some("a"));
}

/// Catches: a lease that is put back booking the whole host whatever it shares: two
/// restored leases of 2 cores each fill the host's 4 cores, so a start of one more core is
/// `NoCapacity` until one of them is stopped, and then it starts.
#[test]
fn a_restored_sharing_lease_keeps_its_resources() {
    let mut p = pool(&[]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let two = || Some(Resources::new(2000, 1, 0, None).unwrap());
    let seat = |n: u64| p.provider.seat_booked(&host("a"), lease(n), None, two());
    assert_eq!((seat(1), seat(2)), (Ok(()), Ok(())));
    p.fx.join("a", "t");
    assert_eq!(
        p.provider.start(sharing_spec(3, 600)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    p.provider.stop(&lease(1));
    assert_eq!(on(&p.provider.start(sharing_spec(3, 600))), Some("a"));
    assert_eq!(p.provider.capacity()[0].leases, vec![lease(2), lease(3)]);
}

/// The world of the reservation tests with `book` leases beside its whole-node requests:
/// hosts `a` and `b`, a placer that books each lease on the host named for it.
fn tail_world(homes: Vec<(u64, &'static str)>) -> World {
    let mut w = World::new(&["a", "b"], plain);
    w.fx.registry.set_placer(Arc::new(Named(homes)));
    w.provider = w.provider.clone().with_classes(vec![whole(), book()]);
    w
}

/// Starts the `book` lease `sequence` of `wall_s` seconds.
fn lease_of(w: &mut World, sequence: u64, wall_s: u64) {
    let ack = w.provider.start(sharing_spec(sequence, wall_s));
    w.check();
    assert!(on(&ack).is_some(), "{ack:?}");
}

/// Catches: only the lease started last counted toward the host's tail. `a` carries a
/// lease that may run 900 s and one started after it that may run 100 s; `b` one of 600 s.
/// `a` ends last (900 s), so the whole-node request is set aside `b`. Counting the later
/// lease alone puts `a` at 100 s and sets it aside.
#[test]
fn every_lease_of_a_host_counts_toward_its_tail() {
    let mut w = tail_world(vec![(1, "a"), (2, "a"), (3, "b")]);
    lease_of(&mut w, 1, 900);
    lease_of(&mut w, 2, 100);
    lease_of(&mut w, 3, 600);
    w.ask(4, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Catches: the end of the lease started first lost when the lease started after it is
/// stopped: `a` is left with its 900 s lease, so it ends after `b` (600 s) and the request
/// is set aside `b`. With the end lost `a` reads as empty and is set aside.
#[test]
fn stopping_the_later_lease_leaves_the_earlier_end_counted() {
    let mut w = tail_world(vec![(1, "a"), (2, "a"), (3, "b")]);
    lease_of(&mut w, 1, 900);
    lease_of(&mut w, 2, 100);
    lease_of(&mut w, 3, 600);
    w.stop(2);
    w.ask(4, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Guard: stopping the lease started first leaves the later lease's end counted: `a` is
/// left with its 900 s lease and ends after `b` (600 s), so the request is set aside `b`.
#[test]
fn stopping_the_earlier_lease_leaves_the_later_end_counted() {
    let mut w = tail_world(vec![(1, "a"), (2, "a"), (3, "b")]);
    lease_of(&mut w, 1, 100);
    lease_of(&mut w, 2, 900);
    lease_of(&mut w, 3, 600);
    w.stop(1);
    w.ask(4, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Catches: the stop of a lease other than the one started last counting nothing. Two
/// leases share `a`; the first stops: its end is counted (it has no estimate, as no source
/// of lives is set), and so is the second's after it.
#[test]
fn stopping_an_earlier_lease_on_a_shared_host_is_counted() {
    let mut p = pool(&["a"]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    p.provider.start(sharing_spec(1, 600));
    p.provider.start(sharing_spec(2, 600));
    p.provider.stop(&lease(1));
    assert_eq!(p.fx.registry.tail_counters().ended_unestimated, 1);
    p.provider.stop(&lease(2));
    assert_eq!(p.fx.registry.tail_counters().ended_unestimated, 2);
}
