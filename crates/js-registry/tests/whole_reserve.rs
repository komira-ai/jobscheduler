//! The reservation and drain state machine of a dedicated pool: which waiting request is
//! given which host and when, what a marked host refuses, when a reservation is given up,
//! and what the caps drop. The registry is driven directly on a manual clock, with
//! hand-built heartbeats and a consumer that polls its start every five seconds; nothing
//! sleeps and no test asserts how long anything takes on a wall clock.

mod support;

use std::time::Duration;

use js_agent_wire::Command;
use js_lease::Timestamp;
use js_provider::{Provider, RefusalCode, StartAck};
use js_registry::{Exclusion, PoolConfig, RunState, WholeNodeConfig};
use js_types::{LeaseId, ProviderId};
use support::world::*;
use support::*;

/// Asserts the quiet-host rule at start: with `drain_after` ten minutes and a quiet host,
/// the request's first start commits at once; nothing waits and no mark is left.
#[test]
fn a_quiet_host_is_committed_at_once_whatever_drain_after_is() {
    let mut w = World::new(&["a"], |w| WholeNodeConfig {
        drain_after_ms: 600_000,
        ..w
    });
    assert_eq!(w.ask(1, "r1"), StartAck::Unknown);
    assert_eq!(w.lease_on("a"), Some(lease(1)));
    assert!(w.reservations().is_empty());
    assert_eq!(w.fx.registry.queue_position(&id("r1")), None);
}

/// Asserts that a host that turns quiet while the request waits is assigned to it on the
/// next tick, ten minutes of `drain_after` or not: the mark is `assigned`, is made at the
/// time of the tick, and is taken by the request's next start. The tick here is the lease
/// table's observe.
#[test]
fn a_host_that_turns_quiet_is_assigned_before_drain_after() {
    let mut w = World::new(&["a"], |w| WholeNodeConfig {
        drain_after_ms: 600_000,
        ..w
    });
    assert_eq!(w.share(1, HOUR_S), StartAck::Started { host: host("a") });
    assert_eq!(w.ask(2, "r1"), StartAck::Unknown);
    w.step(60_000);
    assert!(w.reservations().is_empty());
    w.stop(1);
    w.provider.observe(&lease(1));
    let mark = w.reservation("r1").unwrap();
    assert_eq!(
        (mark.host.clone(), mark.assigned, mark.at),
        (host("a"), true, w.now())
    );
    // A second tick changes nothing: the request keeps its quiet host.
    w.provider.observe(&lease(1));
    assert_eq!(w.reservation("r1"), Some(mark));
    w.step(POLL_MS);
    assert_eq!(w.lease_on("a"), Some(lease(2)));
    assert!(w.reservations().is_empty());
}

/// Asserts `drain_after` for a request that finds no quiet host: no mark one millisecond
/// before ten minutes, a draining mark at ten minutes counted from the first start.
#[test]
fn a_request_reserves_only_after_drain_after_when_no_host_is_quiet() {
    let mut w = World::new(&["a"], |w| WholeNodeConfig {
        drain_after_ms: 600_000,
        ..w
    });
    w.share(1, HOUR_S);
    let first = w.now();
    w.ask(2, "r1");
    w.step(599_999);
    assert!(w.reservations().is_empty());
    w.step(1);
    let mark = w.reservation("r1").unwrap();
    assert_eq!((mark.host, mark.assigned), (host("a"), false));
    assert_eq!(mark.at, first.plus(Duration::from_millis(600_000)));
}

/// Asserts the cost order through the reserved host: three busy hosts whose leases end
/// 600, 300 and 900 seconds after their start; the request reserves the second.
#[test]
fn the_host_that_empties_first_is_the_one_reserved() {
    let mut w = World::new(&["a", "b", "c"], plain);
    for (sequence, wall) in [(1, 600), (2, 300), (3, 900)] {
        w.share(sequence, wall);
    }
    w.ask(4, "r1");
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
}

/// Asserts that a marked host takes no other start: another request's start with an id,
/// one without an id, and a `BookAll` lease are all refused or kept waiting while the
/// host is quiet and marked, and the marked request's own start commits on it.
#[test]
fn a_marked_host_takes_only_the_start_of_its_request() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    w.stop(1);
    w.fx.registry.advance();
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    assert_eq!(w.share(3, 60), no_capacity());
    let anonymous = World::whole_spec(4, None, 600);
    assert_eq!(w.provider.start(anonymous), no_capacity());
    w.asks.clear();
    assert_eq!(w.ask(5, "r2"), StartAck::Unknown);
    assert_eq!(w.lease_on("a"), None);
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    assert_eq!(w.poll(2, "r1", 600), StartAck::Unknown);
    assert_eq!(w.lease_on("a"), Some(lease(2)));
    assert!(w.reservations().is_empty());
}

/// Asserts that a host marked for an older request is not taken by a newer one: the older
/// request reserved a busy host and is then assigned a quiet one that joins; the newer
/// request's start finds that host marked, and the older request's next start commits it.
#[test]
fn a_later_request_cannot_take_the_host_assigned_to_an_earlier_one() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.join("b");
    w.fx.registry.advance();
    let mark = w.reservation("r1").unwrap();
    assert_eq!((mark.host.to_string(), mark.assigned), ("b".into(), true));
    // The newer request polls first. The assigned mark does not drain anything, so the
    // newer request may reserve the busy host, but it cannot have the quiet one.
    assert_eq!(w.poll(3, "r2", 600), StartAck::Unknown);
    assert_eq!(w.lease_on("b"), None);
    assert_eq!(w.reserved_host("r2"), Some("a".to_string()));
    assert_eq!(w.poll(2, "r1", 600), StartAck::Unknown);
    assert_eq!(w.lease_on("b"), Some(lease(2)));
}

/// Asserts the take-a-quiet-host rule: a reserved request moves, dropping its mark, to a
/// host that turns quiet, and the host it left takes a start again.
#[test]
fn a_reserved_request_moves_to_a_host_that_turns_quiet() {
    let mut w = World::new(&["a", "b"], plain);
    w.share(1, HOUR_S);
    w.share(2, HOUR_S);
    w.ask(3, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.stop(2);
    w.provider.observe(&lease(2));
    let mark = w.reservation("r1").unwrap();
    assert_eq!((mark.host.to_string(), mark.assigned), ("b".into(), true));
    assert_eq!(w.reservations().len(), 1);
    w.stop(1);
    assert_eq!(w.share(4, 60), StartAck::Started { host: host("a") });
}

/// Asserts the no-thrash rule: a request given up on the cheaper host and reserved on the
/// dearer one stays on it when the back-off ends, until its own deadline.
#[test]
fn a_reserved_request_does_not_switch_to_a_cheaper_host_later() {
    let mut w = World::new(&["a", "b"], |w| WholeNodeConfig {
        drain_deadline_ms: Some(100_000),
        reserve_backoff_ms: 30_000,
        ..w
    });
    w.share(1, 3_000);
    w.share(2, 3_600);
    w.ask(3, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.step(100_000);
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
    // Host a is out of the back-off at 130 seconds and is the cheaper of the two.
    w.step(40_000);
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
}

/// Asserts the default deadline: with no `drain_deadline` it is the wall time, taken as
/// the longer of the request's and the time the host's lease has left, so a request with a
/// short wall time does not give up on a host whose lease is longer.
#[test]
fn the_default_deadline_covers_the_longest_lease_on_the_host() {
    let mut w = World::new(&["a"], plain);
    w.share(1, 600);
    w.ask_for(2, "r1", 60);
    let mark = w.reservation("r1").unwrap();
    assert_eq!(mark.deadline, w.now().plus(Duration::from_millis(600_000)));
}

/// Asserts that an assigned mark drains nothing and so does not count toward
/// `max_draining`: one request holds the only draining slot on a host it gave up and
/// is excluded from another that turns quiet, and a second request is given that host.
#[test]
fn an_assigned_mark_is_not_held_to_max_draining() {
    let mut w = World::new(&["a", "b"], |w| WholeNodeConfig {
        drain_deadline_ms: Some(100_000),
        reserve_backoff_ms: 1_000_000,
        ..w
    });
    w.share(1, HOUR_S);
    w.share(2, HOUR_S);
    w.ask(3, "r1");
    w.ask(4, "r2");
    w.step(100_000);
    assert_eq!(
        names(&w.reservations()),
        [("r1".to_string(), "b".to_string())]
    );
    w.stop(1);
    w.provider.observe(&lease(1));
    let second = w.reservation("r2").unwrap();
    assert_eq!(
        (second.host.to_string(), second.assigned),
        ("a".into(), true)
    );
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
}

/// Asserts that a mark that moves to a quiet host keeps its age, which orders the marks
/// dropped when the ceiling falls: the request that reserved first keeps its place.
#[test]
fn a_mark_that_moves_keeps_its_age_when_the_ceiling_falls() {
    let mut w = World::new(&["a", "b", "c", "d"], |w| WholeNodeConfig {
        max_draining: 2,
        ..w
    });
    w.ask(1, "held");
    w.asks.clear();
    for sequence in 2..=4 {
        w.share(sequence, HOUR_S);
    }
    w.ask(5, "r1");
    w.step(POLL_MS);
    w.ask(6, "r2");
    w.step(POLL_MS);
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
    assert_eq!(w.reserved_host("r2"), Some("c".to_string()));
    w.stop(4);
    w.provider.observe(&lease(4));
    assert_eq!(w.reserved_host("r1"), Some("d".to_string()));
    w.taint("a");
    w.taint("b");
    w.provider.observe(&lease(2));
    assert_eq!(w.reserved_host("r1"), Some("d".to_string()));
    assert_eq!(w.reserved_host("r2"), None);
}

/// Asserts that the end bound of a lease is the one recorded at its start, and only for
/// that lease: a lease put back on a host after a restart has none and counts as ending
/// now, whatever the host's previous lease was, so the reservation takes that host over
/// one whose recorded lease runs another 300 seconds.
#[test]
fn a_lease_put_back_has_no_bound_and_the_old_one_is_not_used() {
    let mut w = World::of(config(4), &["a", "b"]);
    w.share(1, 900);
    w.stop(1);
    w.provider.seat(&host("a"), lease(9), None).unwrap();
    w.share(2, 300);
    assert_eq!(w.lease_on("b"), Some(lease(2)));
    w.ask(3, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
}

/// Asserts that the end bound survives the host's new session: a host that was lost and
/// says hello again keeps its booking and the bound recorded for it.
#[test]
fn a_new_session_keeps_the_end_bound_of_the_booking() {
    let mut w = World::new(&["a", "b"], plain);
    w.share(1, 900);
    w.share(2, 300);
    w.silent.insert("a".to_string());
    w.step(20_000);
    w.silent.remove("a");
    w.join("a");
    w.ask(3, "r1");
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
}

/// Asserts the give-up at the deadline: kept one millisecond before, given up exactly at
/// it, counted once, the host excluded for `reserve_backoff` from then, and the request
/// on the next cheapest host in the same tick.
#[test]
fn a_reservation_is_given_up_exactly_at_its_deadline() {
    let mut w = World::new(&["a", "b"], drain_deadline(120_000));
    w.share(1, HOUR_S);
    w.share(2, HOUR_S);
    w.ask(3, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    let reserved = w.reservation("r1").unwrap();
    assert_eq!(
        reserved.deadline,
        w.now().plus(Duration::from_millis(120_000))
    );
    w.step(119_999);
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    assert_eq!(w.fx.registry.request_record(&id("r1")), None);
    w.step(1);
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
    let record = w.fx.registry.request_record(&id("r1")).unwrap();
    assert_eq!(record.give_ups, 1);
    let until = w.now().plus(Duration::from_millis(300_000));
    assert_eq!(
        record.exclusions.get(&host("a")),
        Some(&Exclusion::Until(until))
    );
}

/// Asserts that a request with no other host waits unreserved after a give-up and
/// reserves again exactly when the back-off ends: not one millisecond before.
#[test]
fn with_no_other_host_the_request_waits_out_the_backoff_and_reserves_again() {
    let mut w = World::new(&["a"], |w| WholeNodeConfig {
        drain_deadline_ms: Some(100_000),
        reserve_backoff_ms: 50_000,
        ..w
    });
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    w.step(100_000);
    assert_eq!(w.reservation("r1"), None);
    assert_eq!(w.fx.registry.queue_position(&id("r1")), Some(0));
    w.step(49_999);
    assert_eq!(w.reservation("r1"), None);
    w.step(1);
    let again = w.reservation("r1").unwrap();
    assert_eq!(again.deadline, w.now().plus(Duration::from_millis(100_000)));
    assert_eq!(w.fx.registry.request_record(&id("r1")).unwrap().give_ups, 1);
}

/// Asserts W6: with a drain deadline of 100 seconds, a wall time of 600 and two attempts,
/// the request gives up exactly twice, its third reservation has the wall time as its
/// deadline and is not given up at that deadline while the host is busy, and it commits
/// when the host empties. The give-up count stays at two.
#[test]
fn after_max_reserve_attempts_the_reservation_waits_for_the_host_and_commits() {
    let mut w = World::new(&["a"], |w| WholeNodeConfig {
        drain_deadline_ms: Some(100_000),
        reserve_backoff_ms: 10_000,
        max_reserve_attempts: 2,
        ..w
    });
    w.share(1, 600);
    w.ask(2, "r1");
    w.step(220_000);
    let capped = w.reservation("r1").unwrap();
    let record = w.fx.registry.request_record(&id("r1")).unwrap();
    assert_eq!(record.give_ups, 2);
    // The host's lease still has 380 seconds; the request's wall time is 600.
    assert_eq!(
        capped.deadline,
        w.now().plus(Duration::from_millis(600_000))
    );
    w.step(610_000);
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    assert_eq!(w.fx.registry.request_record(&id("r1")).unwrap().give_ups, 2);
    w.stop(1);
    w.step(POLL_MS);
    assert_eq!(w.lease_on("a"), Some(lease(2)));
    assert_eq!(w.fx.registry.request_record(&id("r1")).unwrap().give_ups, 2);
}

/// Asserts W7: a give-up changes no lease and no run on the host and sends the host no
/// stop or cancel, in any heartbeat reply of the whole drain.
#[test]
fn a_give_up_touches_no_lease_and_no_run() {
    let mut w = World::new(&["a"], drain_deadline(100_000));
    w.share(1, HOUR_S);
    w.fx.registry
        .queue_run(&host("a"), &lease(1), job("run-1"))
        .unwrap();
    w.step(POLL_MS);
    let runs = |w: &World| {
        let views = w.fx.registry.runs_of(&lease(1));
        views
            .into_iter()
            .map(|v| (v.run_id, v.state))
            .collect::<Vec<_>>()
    };
    let before = (w.provider.booked(), runs(&w));
    assert_eq!(before.1, [("run-1".to_string(), RunState::Queued)]);
    w.ask(2, "r1");
    w.step(100_000);
    assert_eq!(w.fx.registry.request_record(&id("r1")).unwrap().give_ups, 1);
    assert_eq!((w.provider.booked(), runs(&w)), before);
    let touched = w
        .commands
        .iter()
        .any(|c| matches!(c, Command::Stop { .. } | Command::Cancel { .. }));
    assert!(!touched);
}

/// Asserts that a cancel drops the mark at once: the host it held takes a `BookAll` start
/// again in the same moment, and was refused one before.
#[test]
fn a_cancel_drops_the_mark_at_once() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    w.stop(1);
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    assert_eq!(w.share(3, 60), no_capacity());
    assert!(w.cancel("r1"));
    assert!(w.reservations().is_empty());
    assert_eq!(w.share(4, 60), StartAck::Started { host: host("a") });
}

/// Asserts that a request that stops polling loses a draining mark when its entry
/// expires, to the millisecond of the regain grace.
#[test]
fn an_unpolled_request_loses_its_mark_when_its_entry_expires() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.asks.clear();
    let grace = js_registry::DEFAULT_REGAIN_GRACE_MS;
    w.fx.clock.advance(Duration::from_millis(grace - 1));
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.fx.clock.advance(Duration::from_millis(1));
    assert!(w.reservations().is_empty());
}

/// Asserts `max_draining`: with one slot and two busy hosts the second request waits
/// without a mark, and reserves after the first has committed.
#[test]
fn max_draining_makes_the_second_request_wait_for_the_first_to_commit() {
    let mut w = World::new(&["a", "b"], plain);
    w.share(1, HOUR_S);
    w.share(2, HOUR_S);
    w.ask(3, "r1");
    w.ask(4, "r2");
    assert_eq!(
        names(&w.reservations()),
        [("r1".to_string(), "a".to_string())]
    );
    w.step(30_000);
    assert_eq!(w.reserved_host("r2"), None);
    w.stop(1);
    w.step(POLL_MS);
    assert_eq!(w.lease_on("a"), Some(lease(3)));
    assert_eq!(w.reserved_host("r2"), Some("b".to_string()));
}

/// Asserts the ceiling on a start: with `max_whole_node` 1 a second request waits though a
/// host is quiet, and so does a request without an id; both get it when the first lease
/// ends and the host has been restored.
#[test]
fn the_ceiling_keeps_a_quiet_host_from_a_second_whole_request() {
    let mut w = World::new(&["a", "b"], |w| WholeNodeConfig {
        max_whole_node: 1,
        ..w
    });
    assert_eq!(w.ask(1, "r1"), StartAck::Unknown);
    assert_eq!(w.lease_on("a"), Some(lease(1)));
    assert_eq!(w.ask(2, "r2"), StartAck::Unknown);
    assert_eq!(w.lease_on("b"), None);
    assert!(w.reservations().is_empty());
    let anonymous = World::whole_spec(3, None, 600);
    assert_eq!(w.provider.start(anonymous), no_capacity());
    w.stop(1);
    w.step(3 * POLL_MS);
    assert_eq!(w.lease_on("b"), Some(lease(2)));
}

/// Asserts a whole-node start without a request id is booked on a quiet host when the
/// caps leave room, as before the queue existed.
#[test]
fn a_start_without_an_id_still_books_a_quiet_host() {
    let mut w = World::new(&["a"], plain);
    let anonymous = World::whole_spec(1, None, 600);
    assert_eq!(w.provider.start(anonymous), StartAck::Unknown);
    assert_eq!(w.lease_on("a"), Some(lease(1)));
}

/// Asserts W5, first form: the ceiling falls under a held lease (its host tainted); the
/// newest reservation is dropped, the older kept, the held lease untouched, and no
/// reservation starts while whole hosts are at the ceiling.
#[test]
fn a_falling_ceiling_drops_the_newest_reservation_and_never_a_held_lease() {
    let mut w = World::new(&["a", "b", "c"], |w| WholeNodeConfig {
        max_draining: 2,
        ..w
    });
    assert_eq!(w.ask(1, "held"), StartAck::Unknown);
    w.asks.clear();
    assert_eq!(w.lease_on("a"), Some(lease(1)));
    w.share(2, HOUR_S);
    w.share(3, HOUR_S);
    w.ask(4, "r1");
    w.step(POLL_MS);
    w.ask(5, "r2");
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
    assert_eq!(w.reserved_host("r2"), Some("c".to_string()));
    w.taint("a");
    w.step(POLL_MS);
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
    assert_eq!(w.reserved_host("r2"), None);
    assert_eq!(w.lease_on("a"), Some(lease(1)));
    w.step(60_000);
    assert_eq!(w.reserved_host("r2"), None);
}

/// Asserts W5, second form: held leases alone exceed the ceiling, every reservation is
/// dropped and the held leases stay.
#[test]
fn held_leases_over_the_ceiling_drop_every_reservation() {
    let mut w = World::new(&["a", "b", "c"], |w| WholeNodeConfig {
        max_draining: 2,
        ..w
    });
    w.ask(1, "h1");
    w.ask(2, "h2");
    w.asks.clear();
    w.share(3, HOUR_S);
    w.ask(4, "r1");
    assert_eq!(w.reserved_host("r1"), Some("c".to_string()));
    w.taint("a");
    w.taint("b");
    w.step(POLL_MS);
    assert!(w.reservations().is_empty());
    assert_eq!(w.lease_on("a"), Some(lease(1)));
    assert_eq!(w.lease_on("b"), Some(lease(2)));
    w.step(60_000);
    assert!(w.reservations().is_empty());
}

/// Asserts that a mark on a host that turns tainted is dropped without counting a give-up
/// or excluding the host, and that a lost host is not reserved.
#[test]
fn a_tainted_host_loses_its_mark_and_a_lost_one_is_not_reserved() {
    let mut w = World::new(&["a", "b"], plain);
    w.share(1, HOUR_S);
    w.share(2, HOUR_S);
    w.ask(3, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.taint("a");
    w.step(POLL_MS);
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
    assert_eq!(w.fx.registry.request_record(&id("r1")), None);
    // Host b goes silent: lost, still capable, and the mark it holds is kept.
    w.silent.insert("b".to_string());
    w.step(20_000);
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
    w.cancel("r1");
    w.ask(4, "r2");
    assert_eq!(w.reservation("r2"), None);
    assert_eq!(w.fx.registry.queue_position(&id("r2")), Some(0));
}

/// Asserts that a mark on a host the registry forgot is dropped: the host is silent past
/// the grace and the retention, holds no lease, and its mark goes at the next tick.
#[test]
fn a_mark_on_a_forgotten_host_is_dropped() {
    let name = ProviderId::new("builders").unwrap();
    let config = PoolConfig::new(name, whole(), pool_shape(), 4)
        .with_lost_retention(60_000)
        .unwrap();
    let mut w = World::of(config, &["a", "b"]);
    w.share(1, HOUR_S);
    w.share(2, HOUR_S);
    w.ask(3, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.silent.insert("a".to_string());
    w.step(20_000);
    w.stop(1);
    w.step(40_000);
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.step(30_000);
    assert_eq!(w.reserved_host("r1"), Some("b".to_string()));
    let hosts: Vec<String> =
        w.fx.registry
            .hosts()
            .iter()
            .map(|h| h.id.to_string())
            .collect();
    assert_eq!(hosts, ["b"]);
}

/// Asserts that a start of a lease that was already stopped leaves no entry and no mark,
/// though the host it would have been given is free.
#[test]
fn a_start_of_an_ended_lease_is_refused_and_not_kept() {
    let mut w = World::new(&["a"], plain);
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    assert_eq!(w.reserved_host("r1"), Some("a".to_string()));
    w.asks.clear();
    w.stop(2);
    w.stop(1);
    let ack = w.poll(2, "r1", 600);
    assert_eq!(ack, StartAck::Refused(RefusalCode::LeaseEnded));
    assert!(w.reservations().is_empty());
    assert_eq!(w.fx.registry.queue_position(&id("r1")), None);
}

/// Asserts W10, first form: a stream of share requests that refills every host the
/// moment it is freed does not hold off a whole-node request; it is committed within the
/// longest remaining lease plus one poll.
#[test]
fn a_stream_of_shares_cannot_starve_a_whole_request() {
    let mut w = World::new(&["a", "b"], plain);
    let mut next = 1;
    let mut ends: Vec<(u64, Timestamp)> = Vec::new();
    let begin = w.now();
    let start_shares = |w: &mut World, next: &mut u64, ends: &mut Vec<(u64, Timestamp)>| {
        while let StartAck::Started { .. } = w.share(*next, 60) {
            ends.push((*next, w.now().plus(Duration::from_secs(60))));
            *next += 1;
        }
    };
    start_shares(&mut w, &mut next, &mut ends);
    let whole_sequence = 1_000;
    w.ask(whole_sequence, "r1");
    let mut committed = None;
    for _ in 0..40 {
        w.step(POLL_MS);
        let now = w.now();
        let done: Vec<u64> = ends
            .iter()
            .filter(|(_, end)| *end <= now)
            .map(|e| e.0)
            .collect();
        ends.retain(|(_, end)| *end > now);
        done.into_iter().for_each(|sequence| w.stop(sequence));
        start_shares(&mut w, &mut next, &mut ends);
        let booked = w.provider.booked();
        if booked.values().any(|held| *held == lease(whole_sequence)) {
            committed = Some(now);
            break;
        }
    }
    let committed = committed.expect("the whole request was never committed");
    let waited = committed.millis() - begin.millis();
    assert!(waited <= 60_000 + POLL_MS, "committed after {waited} ms");
}

/// Asserts W2 and W4 over a seeded random run of 200 events: requests asked, cancelled,
/// share leases started and stopped, hosts joining, time passing. `World::check` asserts
/// after every call to the registry that a host carries at most one mark, a request holds
/// at most one, at most `max_draining` marks drain and no lease lands on a marked host
/// unless it is the marked request's own.
#[test]
fn marks_stay_one_to_one_and_a_marked_host_takes_only_its_request() {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut roll = move |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    let mut w = World::new(&["a", "b", "c"], |w| WholeNodeConfig {
        drain_after_ms: 20_000,
        drain_deadline_ms: Some(90_000),
        reserve_backoff_ms: 30_000,
        ..w
    });
    let mut sequence = 1;
    let mut joined = 3;
    let mut seen_marks = 0;
    let mut committed = 0;
    for _ in 0..400 {
        match roll(20) {
            0..=5 => {
                w.ask_for(sequence, &format!("r{sequence}"), 30 + roll(300));
                sequence += 1;
            }
            6..=11 => {
                w.share(sequence, 30 + roll(300));
                sequence += 1;
            }
            12 | 13 => {
                let booked: Vec<LeaseId> = w.provider.booked().into_values().collect();
                if !booked.is_empty() {
                    let at = roll(booked.len() as u64) as usize;
                    w.stop_lease(&booked[at]);
                }
            }
            14 => {
                let live: Vec<String> = w.asks.iter().map(|a| a.1.clone()).collect();
                if !live.is_empty() {
                    let name = live[roll(live.len() as u64) as usize].clone();
                    w.cancel(&name);
                }
            }
            15 => {
                if joined < 4 {
                    joined += 1;
                    w.join(&format!("h{joined}"));
                }
            }
            _ => w.step(1_000 * (1 + roll(60))),
        }
        seen_marks += w.reservations().len();
        let booked = w.provider.booked();
        committed += booked
            .values()
            .filter(|l| w.owners.keys().any(|s| lease(*s) == **l))
            .count();
    }
    // The run is not vacuous: marks existed.
    assert!(seen_marks > 50, "{seen_marks}");
    assert!(committed > 20, "{committed}");
}
