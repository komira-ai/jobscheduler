//! Which placement calls the registry's own reservation machine: its wait list
//! (`whole/queue.rs`), its request records (`whole/requests.rs`), its marks and its pass
//! (`whole/reserve.rs`, `Registry::place_waiting`). Each part counts its calls under the
//! crate's `test-support` feature ([`Registry::machine_calls`]), which the tests of this
//! crate turn on.
//!
//! One scenario reaches every path of the machine a registry has: a whole-node request that
//! is committed on a quiet host and fails its prepare at its agent's refusal; one that
//! waits, marks a busy host and is read in place, in the scaler's rows, the hosts listing,
//! the counts and the ledger; one cancelled by the consumer and one by a stop of the lease
//! that presented it; a start without an id that is committed and fails its prepare at
//! its deadline; a host retired, drained and released; the ledger read back by a second
//! registry, which binds its request, takes its mark back when the host says hello, queues
//! a new request behind it and lets it lapse after `regain_grace`. Under `--placement js-sched` the engine
//! keeps all of it and the machine is never called; under `--placement registry` every part
//! of it is.
//!
//! The registry runs on a manual clock; nothing sleeps.

mod support;

use js_agent_wire::RefusalReason;
use js_lease::ReservationRecord;
use js_provider::{Pool, StartAck};
use js_registry::{
    DEFAULT_REGAIN_GRACE_MS, MachineCalls, PoolConfig, Registry, RequestKey, WholeNodeConfig,
};
use js_types::ProviderId;
use support::world::*;
use support::*;

/// A pool of up to four hosts that gives a reservation thirty seconds, a reserve back-off
/// of a minute and two tries at a prepare.
fn pool() -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    let settings = WholeNodeConfig {
        drain_deadline_ms: Some(30_000),
        reserve_backoff_ms: 60_000,
        max_replace: 2,
        ..WholeNodeConfig::dedicated(4)
    };
    PoolConfig::new(name, whole(), world_shape(), 4)
        .with_whole_node(settings)
        .unwrap()
}

/// Reads everything an operator, the scaler and the lease file read of the whole-node
/// queue of `registry`, as a test asserts it, and answers what the scenario checks.
fn read_all(registry: &Registry, w: &World) -> (Option<usize>, usize) {
    let place = registry.queue_position(&id("r2"));
    let queued = registry.whole_counts().queued;
    let _ = registry.reservations();
    let _ = registry.is_reserved(&w.host_id("a"));
    let _ = registry.waiting();
    let _ = registry.snapshot();
    let _ = registry.hosts();
    let _ = registry.request_record(&id("r1"));
    let _ = registry.not_ready(&id("r1"));
    let _ = registry.reservation_ledger();
    let _ = registry.dropped_reservations();
    (place, queued)
}

/// The scenario of the module documentation under placement `p`: the first registry, the
/// ledger it wrote, and the registry restarted over that ledger.
fn run(p: Placement) -> (World, Vec<ReservationRecord>, World) {
    let mut w = World::of_in(p, pool(), &["a", "b"]);
    w.share(1, HOUR_S);
    // A start without an id is committed on the quiet `b`, and its prepare times out: its
    // request, told apart by its content, counts the failure.
    let anonymous = World::whole_spec(6, None, 600);
    let key = RequestKey::Content(anonymous.request().clone());
    assert_eq!(w.provider.start(anonymous), StartAck::Unknown);
    assert_eq!(w.lease_on("b"), Some(lease(6)));
    w.step(DEFAULT_PREPARE_S * 1_000);
    w.fx.registry.advance();
    let record = w.fx.registry.request_record(key).unwrap();
    assert_eq!(record.prepare_failures, 1);
    w.step(10_000);
    // `a` is busy; `r1` takes the quiet `b` and is committed there.
    assert_eq!(w.ask(2, "r1"), StartAck::Unknown);
    w.step(5_000);
    let r1 = lease(2);
    assert_eq!(w.lease_on("b"), Some(r1));
    // `r2` waits behind it, and marks the busy `a`, with thirty seconds to drain it.
    assert_eq!(w.ask(3, "r2"), StartAck::Unknown);
    w.step(20_000);
    let (place, queued) = read_all(&w.fx.registry, &w);
    assert_eq!((place, queued), (Some(0), 1));
    assert_eq!(w.reserved_host("r2").as_deref(), Some("a"));
    assert!(w.provider.waits(&lease(3)));
    // The tick's pass while `r2` waits.
    w.fx.registry.advance();
    assert_eq!(w.reserved_host("r2").as_deref(), Some("a"));
    // The ledger while `r2` waits at the front with its mark on `a`.
    let ledger = w.fx.registry.reservation_ledger();
    // `r1`'s agent refuses the prepare: one failure, counted.
    w.refuse("b", &r1, RefusalReason::SetupFailed);
    let record = w.fx.registry.request_record(&id("r1")).unwrap();
    assert_eq!(record.prepare_failures, 1);
    // `r3` waits, and is cancelled by the consumer; `r4` waits, and is cancelled by a stop
    // of the lease that presented it.
    assert_eq!(w.ask(4, "r3"), StartAck::Unknown);
    assert!(w.cancel("r3"));
    assert_eq!(w.ask(5, "r4"), StartAck::Unknown);
    w.stop(5);
    assert!(w.fx.registry.queue_position(&id("r4")).is_none());
    // A host retired, then one drained and released.
    let _ = w.fx.registry.retire(&w.host_id("a"));
    let _ = w.fx.registry.drain(&w.host_id("b"));
    let _ = w.fx.registry.release_host(&w.host_id("b"));
    w.fx.registry.advance();
    // A second registry on the same clock, over the ledger: it binds `r2` to its lease,
    // takes it back at its place with its mark once `a` says hello and `r2` is presented,
    // and lets it lapse when nobody presents it again.
    let mut back = World::of_in(p, pool(), &[]);
    back.step(w.now().millis() - back.now().millis());
    let restored = back.fx.registry.restore_reservations(&ledger);
    assert_eq!(restored.dropped, 0);
    assert_eq!(back.fx.registry.bind_waiting(&[(lease(3), id("r2"))]), 1);
    assert_eq!(back.fx.registry.queue_position(&id("r2")), Some(0));
    back.join("a");
    assert_eq!(back.reserved_host("r2").as_deref(), Some("a"));
    // A new request is presented and waits behind `r2`, whose mark holds the only host.
    assert_eq!(back.ask(7, "r5"), StartAck::Unknown);
    let _ = read_all(&back.fx.registry, &back);
    back.drop_ask(7);
    back.step(DEFAULT_REGAIN_GRACE_MS + 5_000);
    assert_eq!(back.fx.registry.queue_position(&id("r2")), None);
    assert_eq!(back.fx.registry.dropped_reservations(), 1);
    let _ = read_all(&back.fx.registry, &back);
    (w, ledger, back)
}

/// The prepare timeout of a dedicated pool's defaults, in seconds.
const DEFAULT_PREPARE_S: u64 = js_registry::DEFAULT_PREPARE_TIMEOUT_MS / 1_000;

/// Catches a call of the registry's own wait list, request records, marks or pass under a
/// placer that keeps the wait list: each of them would count it. Every read and every
/// change of the scenario reaches the engine instead, and the scenario still waits, places,
/// counts the failure, cancels, puts back and binds as it does under the registry.
#[test]
fn the_registry_reservation_machine_is_not_called_under_js_sched() {
    let (w, ledger, back) = run(Placement::JsSched);
    assert_eq!(w.fx.registry.machine_calls(), MachineCalls::default());
    assert_eq!(back.fx.registry.machine_calls(), MachineCalls::default());
    assert!(w.fx.registry.machine_calls().none());
    // The scenario did reach the queue: the ledger held `r2`'s place.
    let r2 = ledger.iter().find(|row| row.request_id == "r2").unwrap();
    assert_eq!(r2.queued.map(|queued| queued.place), Some(0));
}

/// The negative case: under the registry's own placement the same scenario calls every
/// part of the machine, so the counters move and a test that reads them would see a call
/// that should not be there.
#[test]
fn under_placement_registry_every_part_of_the_machine_is_called() {
    let (w, ledger, back) = run(Placement::Registry);
    for calls in [
        w.fx.registry.machine_calls(),
        back.fx.registry.machine_calls(),
    ] {
        assert!(!calls.none());
        assert!(calls.wait_list > 0, "{calls:?}");
        assert!(calls.records > 0, "{calls:?}");
        assert!(calls.marks > 0, "{calls:?}");
        assert!(calls.passes > 0, "{calls:?}");
    }
    let r2 = ledger.iter().find(|row| row.request_id == "r2").unwrap();
    assert_eq!(r2.queued.map(|queued| queued.place), Some(0));
}
