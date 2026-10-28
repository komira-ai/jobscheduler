//! The guards on the start path and the caps of the reservation state machine, one test
//! each: what a repeated start leaves alone, which starts never wait, which reservation
//! the ceiling drops, and the pool's `keep_for_ordinary` read through the registry. The
//! registry is driven on a manual clock; no test asserts how long anything takes.

mod support;

use std::collections::BTreeSet;

use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{PoolConfig, WholeNodeConfig};
use js_types::{Admission, ClassId, Dims, ProviderId, Request, ResourceClass, Resources, Shape};
use support::world::*;
use support::*;

/// Asserts that a repeated start of a lease already on a host leaves that host's end bound
/// where the first start put it. Lease 1 (300 s) is on `a` and lease 2 (400 s) on `b`;
/// lease 1 is started again 200 s in. A bound moved by the repeat would read 500 s and make
/// `b` the cheaper host to drain, so the request reserves `a` only if the bound stayed.
#[test]
fn a_repeated_start_does_not_move_the_end_bound_of_its_host() {
    let mut w = World::new(&["a", "b"], plain);
    w.share(1, 300);
    w.share(2, 400);
    w.step(200_000);
    assert_eq!(w.share(1, 300), StartAck::Started { host: host("a") });
    w.poll(3, "r1", 600);
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
}

/// Asserts that a `BookAll` start carrying a request id is turned away like one without:
/// the id is not put on the wait list and no host is set aside for it, however long it
/// polls.
#[test]
fn a_share_start_with_a_request_id_never_waits_and_is_never_marked() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    let waiting = StartSpec::new(
        lease(2),
        World::share_spec(2, 60).request().clone(),
        &[share_class()],
    )
    .unwrap()
    .with_request_id(id("s1"));
    for _ in 0..3 {
        let refused = StartAck::Refused(RefusalCode::NoCapacity);
        assert_eq!(w.provider.start(waiting.clone()), refused);
        w.fx.registry.advance();
        assert_eq!(w.fx.registry.queue_position(&id("s1")), None);
        assert!(w.reservations().is_empty());
        w.step(POLL_MS);
    }
}

/// Asserts that a repeated start of a whole-node lease that is on its host and ready does
/// not put its request id on the wait list again: the request stays off the list and
/// holds no mark.
#[test]
fn a_repeated_start_of_a_lease_on_its_host_is_not_queued() {
    let mut w = World::new(&["a"], plain);
    assert_eq!(w.poll(1, "r1", 600), StartAck::Unknown);
    let epoch = w.epochs["a"];
    let mut ready = w.fx.heartbeat("a", epoch);
    ready.exclusive_ready = Some(lease(1).to_string());
    w.fx.registry.heartbeat(&ready, &cred("t")).unwrap();
    assert_eq!(w.poll(1, "r1", 600), StartAck::Started { host: host("a") });
    assert_eq!(w.fx.registry.queue_position(&id("r1")), None);
    w.fx.registry.advance();
    assert!(w.reservations().is_empty());
}

/// Asserts that a start the class gate refuses is not queued: a whole-node start of a
/// class the provider does not list is refused `UnsupportedClass`, with the host busy,
/// and its request id is neither on the list nor marked.
#[test]
fn a_start_refused_by_the_class_gate_is_not_queued() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    let mut unlisted = w.fx.registry.provider().with_classes(vec![share_class()]);
    let spec = World::whole_spec(2, Some("r1"), 600);
    let refused = StartAck::Refused(RefusalCode::UnsupportedClass);
    assert_eq!(unlisted.start(spec), refused);
    w.fx.registry.advance();
    assert_eq!(w.fx.registry.queue_position(&id("r1")), None);
    assert!(w.reservations().is_empty());
}

/// Asserts that a falling ceiling drops a reservation older than a held lease and keeps
/// the held mark. `r1` is assigned host `c` at the first instant; `r2` commits on `d`
/// later, so its held mark is the newer. Tainting `a` and `d` leaves one capable host, a
/// ceiling of one under two whole marks: the reservation goes, the held lease stays.
#[test]
fn a_falling_ceiling_drops_a_reservation_older_than_the_held_lease() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    w.poll(2, "r1", 600);
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.join("c");
    w.fx.registry.advance();
    assert_eq!(w.reserved_host("r1"), Some("c".to_string()));
    w.step(POLL_MS);
    w.join("d");
    assert_eq!(w.poll(3, "r2", 600), StartAck::Unknown);
    assert_eq!(w.lease_on("d"), Some(lease(3)));
    assert!(w.reservation("r1").is_some_and(|mark| mark.assigned));
    w.taint("a");
    w.taint("d");
    w.fx.registry.advance();
    assert_eq!(w.reserved_host("r1"), None);
    assert_eq!(w.lease_on("d"), Some(lease(3)));
}

/// A mixed pool of up to four hosts that may hold three whole and keeps one for ordinary
/// work.
fn mixed_keeping_one(hosts: &[&str]) -> World {
    let name = ProviderId::new("builders").unwrap();
    let book = ResourceClass::new(
        ClassId::new("mixed").unwrap(),
        Admission::Book(Dims::new()),
        0,
    );
    let settings = WholeNodeConfig {
        max_whole_node: 3,
        max_draining: 3,
        keep_for_ordinary: 1,
        ..WholeNodeConfig::dedicated(4)
    };
    let config = PoolConfig::new(name, book, pool_shape(), 4)
        .with_whole_node(settings)
        .unwrap();
    World::of(config, hosts)
}

/// Asserts that the pool's `keep_for_ordinary` limits the reservations the registry makes:
/// with three busy hosts and one kept, the ceiling is two, so the third request gets no
/// mark.
#[test]
fn keep_for_ordinary_limits_how_many_hosts_are_reserved() {
    let mut w = mixed_keeping_one(&["a", "b", "c"]);
    for (sequence, wall) in [(1, 300), (2, 400), (3, 500)] {
        w.share(sequence, wall);
    }
    w.ask(4, "r1");
    w.ask(5, "r2");
    w.ask(6, "r3");
    w.step(POLL_MS);
    let marked: BTreeSet<_> = w.reservations().into_iter().map(|r| r.request).collect();
    assert_eq!(marked, BTreeSet::from([id("r1"), id("r2")]));
}

/// Asserts that the pool's `keep_for_ordinary` is read when the ceiling falls: two
/// reservations stand under a ceiling of two; tainting the third host leaves two capable
/// hosts and one kept, a ceiling of one, so the newer reservation is dropped.
#[test]
fn keep_for_ordinary_lowers_the_ceiling_that_drops_reservations() {
    let mut w = mixed_keeping_one(&["a", "b", "c"]);
    for (sequence, wall) in [(1, 300), (2, 400), (3, 500)] {
        w.share(sequence, wall);
    }
    w.ask(4, "r1");
    w.step(POLL_MS);
    w.ask(5, "r2");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    assert_eq!(w.reserved_host("r2"), Some("b".to_string()));
    w.taint("c");
    w.step(POLL_MS);
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    assert_eq!(w.reserved_host("r2"), None);
}

/// A pool of one-GPU hosts, serving `gpu-host` next to the whole-node class.
fn gpu_world() -> World {
    let floor = Resources::new(4000, 8 << 30, 1, Some("a100".into())).unwrap();
    let shape = Shape::new(floor, map(&[("os", "macos")]));
    let name = ProviderId::new("builders").unwrap();
    let config = PoolConfig::new(name, whole(), shape, 4);
    let config = config
        .with_whole_node(WholeNodeConfig::dedicated(4))
        .unwrap();
    let mut w = World::of(config, &[]);
    w.provider =
        w.fx.registry
            .provider()
            .with_classes(vec![ResourceClass::gpu_host(), whole()]);
    w
}

fn join_gpu(w: &mut World, name: &str) {
    let mut message = hello(name);
    message.shape.gpus = 1;
    message.shape.gpu_class = Some("a100".into());
    let epoch = w.fx.hello(&message, &cred("t")).unwrap().epoch;
    w.epochs.insert(name.to_string(), epoch);
    w.check();
}

fn gpu_host_start(w: &mut World, sequence: u64) -> StartAck {
    let request = Request {
        class: ResourceClass::gpu_host().id().clone(),
        resources: Resources::new(1000, 1, 1, Some("a100".into())).unwrap(),
        ..request()
    };
    let spec = StartSpec::new(lease(sequence), request, &[ResourceClass::gpu_host()]).unwrap();
    let ack = w.provider.start(spec);
    w.check();
    ack
}

/// Asserts that a `gpu-host` start is affected only by a host a waiting whole-node request
/// marked. Host `a` is busy with a `gpu-host` lease and request `r1` waits; host `c` joins
/// and is assigned to `r1`. A `gpu-host` start finds only the marked `c` free and is turned
/// away; once an unmarked `d` joins, the same kind of start lands on `d`, and `c` stays
/// marked for `r1`.
#[test]
fn a_gpu_host_start_is_refused_only_on_a_marked_host() {
    let mut w = gpu_world();
    join_gpu(&mut w, "a");
    assert_eq!(
        gpu_host_start(&mut w, 1),
        StartAck::Started { host: host("a") }
    );
    assert_eq!(w.ask(2, "r1"), StartAck::Unknown);
    join_gpu(&mut w, "c");
    w.fx.registry.advance();
    assert_eq!(w.reserved_host("r1"), Some("c".to_string()));
    assert_eq!(gpu_host_start(&mut w, 3), no_capacity());
    assert_eq!(w.lease_on("c"), None);
    join_gpu(&mut w, "d");
    assert_eq!(
        gpu_host_start(&mut w, 4),
        StartAck::Started { host: host("d") }
    );
    assert_eq!(w.reserved_host("r1"), Some("c".to_string()));
}
