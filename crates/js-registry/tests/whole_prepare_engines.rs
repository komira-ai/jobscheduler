//! The prepare, ready and failure path of a whole-node lease, and the record its request
//! keeps, under each engine: the registry's own pass (`--placement registry`) and the
//! js-sched engine (`--placement js-sched`), which keeps the deadline, the failed-prepare
//! count, the bars and the record of a request it commits from its queue. Every test runs
//! the same steps and asserts the same under both ([`over_both`]), on the World harness:
//! hosts that beat every five seconds on a manual clock and a consumer that polls its
//! whole-node requests by id. The registry's tick (`Registry::advance`) is the only place
//! a deadline is checked.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::RefusalReason;
use js_lease::Timestamp;
use js_provider::{Observed, Pool, RefusalCode, StartAck};
use js_registry::{Exclusion, WholeNodeConfig};
use support::world::*;
use support::*;

/// `prepare_timeout` of the pool.
const TIMEOUT: u64 = 120_000;
/// `reserve_backoff` of the pool.
const BACKOFF: u64 = 60_000;
/// The registry's idle horizon for these settings: a back-off for each of the
/// `max_replace + 1` tries, at least one prepare timeout.
const HORIZON: u64 = 3 * BACKOFF;

/// Short settings: a two-minute prepare, a one-minute back-off, two tries.
fn short(w: WholeNodeConfig) -> WholeNodeConfig {
    WholeNodeConfig {
        prepare_timeout_ms: TIMEOUT,
        reserve_backoff_ms: BACKOFF,
        max_replace: 2,
        ..w
    }
}

fn after(at: Timestamp, ms: u64) -> Timestamp {
    at.plus(Duration::from_millis(ms))
}

/// The record request `r` keeps, as the registry answers it.
fn record(w: &World) -> Option<js_registry::RequestRecord> {
    w.fx.registry.request_record(&id("r"))
}

/// The hosts a request is barred from, and until when.
type Bars = BTreeMap<js_types::HostId, Exclusion>;

/// A row of the reasons table: the code, the hosts it bars the request from given when the
/// refusal came, and whether it taints the host.
type Row<'a> = (RefusalReason, &'a dyn Fn(Timestamp) -> Bars, bool);

fn barred(pairs: &[(&str, Exclusion)]) -> Bars {
    pairs.iter().map(|(name, e)| (host(name), *e)).collect()
}

/// A pool of `a` and `b` under `engine` in which request `r` has committed lease `1-1` on
/// `a`, the first host by id.
fn committed(engine: Engine) -> World {
    let mut w = World::on(engine, &["a", "b"], short);
    assert_eq!(w.ask(1, "r"), StartAck::Unknown);
    assert_eq!(w.lease_on("a"), Some(lease(1)));
    w
}

/// Catches: a prepare that never fails (the host would be held for ever), one that fails a
/// tick early, a failure not counted, a failing host offered to the request again (it is
/// first by id and restored, so it would be taken), or a request that is not placed again
/// at once on the other quiet host. The committed host never reports ready: at the first
/// tick at its deadline the lease is gone, the host is barred for `reserve_backoff` and the
/// request counts one failure, which it keeps until a lease of it is ready on `b`.
#[test]
fn a_prepare_that_times_out_places_the_request_on_another_host() {
    over_both(|engine| {
        let mut w = committed(engine);
        w.step(TIMEOUT - 1);
        w.fx.registry.advance();
        assert_eq!(w.lease_on("a"), Some(lease(1)));
        assert_eq!(record(&w), None);
        w.step(1);
        let failed = w.now();
        w.fx.registry.advance();
        assert_eq!(w.provider.observe(&lease(1)), Observed::Absent);
        let kept = record(&w).unwrap();
        assert_eq!((kept.prepare_failures, kept.terminal), (1, false));
        let until = Exclusion::Until(after(failed, BACKOFF));
        assert_eq!(kept.exclusions, barred(&[("a", until)]));
        assert_eq!(kept.last.unwrap().code, "prepare-timeout");
        // Two beats restore `a`: only the bar keeps the request off it now.
        w.drop_ask(1);
        w.step(2 * POLL_MS);
        assert_eq!(w.ask(2, "r"), StartAck::Unknown);
        assert_eq!(w.lease_on("b"), Some(lease(2)));
        assert_eq!(w.lease_on("a"), None);
        assert_eq!(record(&w).unwrap().prepare_failures, 1);
        w.ready("b", &lease(2));
        assert_eq!(
            w.provider.observe(&lease(2)),
            Observed::Present { host: host("b") }
        );
        assert_eq!(record(&w), None);
    });
}

/// Catches: the deadline checked before the ready report it races (at the heartbeat, or a
/// tick that runs ahead of the beats): the report arrives in the beat at the deadline,
/// before the tick that would end the lease, and wins. The lease is ready and no failure is
/// counted.
#[test]
fn a_ready_report_that_crosses_the_deadline_wins() {
    over_both(|engine| {
        let mut w = committed(engine);
        w.step(TIMEOUT);
        w.ready("a", &lease(1));
        w.fx.registry.advance();
        let present = Observed::Present { host: host("a") };
        assert_eq!(w.provider.observe(&lease(1)), present);
        assert_eq!(record(&w), None);
        w.step(TIMEOUT);
        w.fx.registry.advance();
        assert_eq!(w.provider.observe(&lease(1)), present);
    });
}

/// Catches: a cause mapped to another effect, or a cause that does not requeue the request.
/// One case per row of the reasons table (`whole/reasons.rs`): `not-quiet`,
/// `noise-over-bound` and `setup-failed` bar the host for `reserve_backoff`, `unsupported`
/// for the request's life, and `host-unsafe` taints the host and bars nothing of its own;
/// in every case the request counts one failure and is placed again on `b` at once.
#[test]
fn each_cause_has_its_effect() {
    over_both(|engine| {
        let backoff = |at| barred(&[("a", Exclusion::Until(after(at, BACKOFF)))]);
        let rows: [Row<'_>; 5] = [
            (RefusalReason::NotQuiet, &backoff, false),
            (RefusalReason::NoiseOverBound, &backoff, false),
            (RefusalReason::SetupFailed, &backoff, false),
            (
                RefusalReason::Unsupported,
                &|_| barred(&[("a", Exclusion::ForLife)]),
                false,
            ),
            (RefusalReason::HostUnsafe, &|_| barred(&[]), true),
        ];
        for (code, exclusions, tainted) in rows {
            let mut w = committed(engine);
            let refused = w.now();
            w.refuse("a", &lease(1), code);
            assert_eq!(w.provider.observe(&lease(1)), Observed::Absent, "{code:?}");
            let kept = record(&w).unwrap();
            assert_eq!(kept.prepare_failures, 1, "{code:?}");
            assert_eq!(kept.exclusions, exclusions(refused), "{code:?}");
            let taint = w.fx.registry.hosts()[0].taint.clone();
            assert_eq!(taint.is_some(), tainted, "{code:?}");
            assert_eq!(w.fx.registry.hosts()[1].taint, None, "{code:?}");
            w.drop_ask(1);
            assert_eq!(w.ask(2, "r"), StartAck::Unknown, "{code:?}");
            assert_eq!(w.lease_on("b"), Some(lease(2)), "{code:?}");
        }
    });
}

/// Catches: a terminal refusal that repeats, one given before `max_replace` failures, or
/// bars and counts that survive it (the start after it would not be a fresh request). Two
/// refused prepares, on `a` then `b`, use up `max_replace`; the next start is refused
/// `exclusive-not-ready` once, naming the last code; the start after it is placed on `a`,
/// first by id, which the first failure had barred.
#[test]
fn max_replace_makes_the_next_start_exclusive_not_ready() {
    over_both(|engine| {
        let mut w = committed(engine);
        w.refuse("a", &lease(1), RefusalReason::NotQuiet);
        w.drop_ask(1);
        assert_eq!(w.ask(2, "r"), StartAck::Unknown);
        assert_eq!(w.lease_on("b"), Some(lease(2)));
        assert!(!record(&w).unwrap().terminal);
        w.refuse("b", &lease(2), RefusalReason::SetupFailed);
        w.drop_ask(2);
        let kept = record(&w).unwrap();
        assert_eq!((kept.prepare_failures, kept.terminal), (2, true));
        let last = w.fx.registry.not_ready(&id("r")).unwrap();
        assert_eq!(last.code, "setup-failed");
        w.step(2 * POLL_MS);
        let refused = StartAck::Refused(RefusalCode::ExclusiveNotReady);
        assert_eq!(w.poll(3, "r", 600), refused);
        assert_eq!(record(&w), None);
        assert_eq!(w.fx.registry.not_ready(&id("r")), None);
        assert_eq!(w.ask(4, "r"), StartAck::Unknown);
        assert_eq!(w.lease_on("a"), Some(lease(4)));
        assert_eq!(record(&w), None);
    });
}

/// Catches: a ready report that moves the committed lease when it names another lease,
/// comes from a host in another state (free, with nothing committed) or from a tainted host.
/// Each is ignored: the lease stays pending, and at its deadline it fails as a prepare that
/// never became ready.
#[test]
fn a_ready_report_for_another_lease_changes_nothing() {
    over_both(|engine| {
        let mut w = committed(engine);
        w.ready("a", &lease(9));
        assert_eq!(w.provider.observe(&lease(1)), Observed::Unknown);
        w.ready("b", &lease(1));
        assert_eq!(w.provider.observe(&lease(1)), Observed::Unknown);
        w.taint("a");
        w.ready("a", &lease(1));
        assert_eq!(w.provider.observe(&lease(1)), Observed::Unknown);
        assert_eq!(record(&w), None);
        w.step(TIMEOUT);
        w.fx.registry.advance();
        assert_eq!(w.provider.observe(&lease(1)), Observed::Absent);
        assert_eq!(record(&w).unwrap().prepare_failures, 1);
    });
}

/// Catches: a record, and its bar for life, that outlives its consumer, or one dropped a
/// tick early. `r` is refused `unsupported` on `a` and not started again: one millisecond
/// short of the idle horizon its record holds the bar; at the horizon record and bar are
/// gone, and the next start of `r` is placed on `a`.
#[test]
fn an_idle_record_is_dropped_after_the_horizon() {
    over_both(|engine| {
        let mut w = committed(engine);
        w.refuse("a", &lease(1), RefusalReason::Unsupported);
        w.drop_ask(1);
        let kept = record(&w).unwrap();
        assert_eq!(kept.exclusions, barred(&[("a", Exclusion::ForLife)]));
        w.step(HORIZON - 1);
        assert_eq!(record(&w).unwrap().exclusions, kept.exclusions);
        w.step(1);
        assert_eq!(record(&w), None);
        assert_eq!(w.ask(2, "r"), StartAck::Unknown);
        assert_eq!(w.lease_on("a"), Some(lease(2)));
    });
}

/// Catches: a request cancelled while it waits that keeps its failures and bars (the id
/// would not name a fresh request when it is used again). `r` is barred from `a` for life
/// and `b` is busy, so its second start waits; its lease is stopped, which cancels it; the
/// next start of `r` is placed on `a`.
#[test]
fn a_cancelled_request_forgets_its_failures_and_bars() {
    over_both(|engine| {
        let mut w = committed(engine);
        assert!(matches!(w.share(10, 3_600), StartAck::Started { .. }));
        assert_eq!(w.lease_on("b"), Some(lease(10)));
        w.refuse("a", &lease(1), RefusalReason::Unsupported);
        w.drop_ask(1);
        w.step(2 * POLL_MS);
        assert_eq!(w.ask(2, "r"), StartAck::Unknown);
        assert_eq!(w.lease_on("a"), None);
        assert!(record(&w).is_some());
        w.drop_ask(2);
        w.stop(2);
        assert_eq!(record(&w), None);
        assert_eq!(w.ask(3, "r"), StartAck::Unknown);
        assert_eq!(w.lease_on("a"), Some(lease(3)));
    });
}

/// Catches: a request barred for its life from every host left waiting for ever. With one
/// host, refused `unsupported` there, the next start finds no host that could ever take it
/// and is refused `no-capable-host`.
#[test]
fn a_request_barred_for_life_from_every_host_is_refused() {
    over_both(|engine| {
        let mut w = World::on(engine, &["a"], short);
        assert_eq!(w.ask(1, "r"), StartAck::Unknown);
        w.refuse("a", &lease(1), RefusalReason::Unsupported);
        w.drop_ask(1);
        w.step(2 * POLL_MS);
        let refused = StartAck::Refused(RefusalCode::NoCapableHost);
        assert_eq!(w.poll(2, "r", 600), refused);
    });
}
