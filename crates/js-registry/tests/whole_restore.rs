//! The reservation ledger across a restart: what the registry hands the lease file to
//! keep, and what a new registry does with it. A restart is a second registry on the same
//! manual clock, given the ledger of the first before any host says hello again. Nothing
//! sleeps and no test asserts how long anything takes on a wall clock.

mod support;

use js_agent_wire::{ExclusiveRefusal, RefusalReason};
use js_lease::{
    Exclusion as LeaseExclusion, ExclusionEnd, Held, Queued, ReservationRecord, Timestamp,
};
use js_provider::{Provider, RefusalCode, StartAck};
use js_registry::{Exclusion, PoolConfig, Restored, WholeNodeConfig};
use js_types::ProviderId;
use support::world::*;
use support::*;

/// A pool that gives a reservation thirty seconds and keeps the defaults of everything
/// else (a request not presented again is dropped after the default regain grace).
fn pool() -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    let settings = WholeNodeConfig {
        drain_deadline_ms: Some(30_000),
        ..WholeNodeConfig::dedicated(4)
    };
    PoolConfig::new(name, whole(), pool_shape(), 4)
        .with_whole_node(settings)
        .unwrap()
}

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

/// The first instant of every world: its clock starts here.
const T0: u64 = 1_000_000;

/// One busy host `a` and two requests: `r1` reserved it, gave it up at its deadline
/// (thirty seconds in) and is excluded from it for five minutes; `r2`, behind it, was
/// set the host then. A minute of the clock has not passed: the world is thirty-one
/// seconds old.
fn two_waiting() -> World {
    let mut w = World::of(pool(), &["a"]);
    w.share(1, HOUR_S);
    w.ask(2, "r1");
    w.ask(3, "r2");
    w.step(31_000);
    w
}

/// A new registry on the clock of `old`, given `ledger`. With `host_a` the host `a` is up
/// and busy with an ordinary lease before the ledger is given (a start on a host a mark
/// holds is refused, and a seat would book the lease as a committed whole-node one, which
/// with a reservation on the same host is over the pool's ceiling); without it no host is
/// up.
fn restarted(old: &World, ledger: &[ReservationRecord], host_a: bool) -> (World, Restored) {
    let hosts: &[&str] = if host_a { &["a"] } else { &[] };
    let mut back = World::of(pool(), hosts);
    if host_a {
        back.share(1, HOUR_S);
    }
    back.step(old.now().millis() - back.now().millis());
    let restored = back.fx.registry.restore_reservations(ledger);
    (back, restored)
}

fn held(name: &str, reserved: u64, deadline: u64) -> Held {
    Held {
        host: host(name),
        reserved_at: at(reserved),
        deadline: at(deadline),
        assigned: false,
        tail_end: None,
    }
}

fn queued(place: u32) -> Option<Queued> {
    Some(Queued {
        place,
        since: at(T0),
    })
}

fn record(id: &str) -> ReservationRecord {
    ReservationRecord {
        request_id: id.to_string(),
        held: None,
        queued: None,
        give_ups: 0,
        prepare_failures: 0,
        exclusions: Vec::new(),
        claim_by: None,
    }
}

/// Asserts the ledger the registry keeps while two requests wait: for each request its
/// place and the instant it was first presented, its give-up count and the host it is
/// kept off with the instant that ends it, and for the one that holds a host the host,
/// when it was set aside, the deadline and that the host was not quiet. It catches a
/// ledger without the order of the wait list, one without the exclusion that bounds the
/// next try, and a mark written without its deadline.
#[test]
fn the_ledger_holds_marks_places_counters_and_exclusions() {
    let w = two_waiting();
    let first = ReservationRecord {
        queued: queued(0),
        give_ups: 1,
        exclusions: vec![LeaseExclusion {
            host: host("a"),
            until: ExclusionEnd::At(at(T0 + 30_000 + 300_000)),
        }],
        ..record("r1")
    };
    let second = ReservationRecord {
        // The host's one lease ends at its bound, an hour after its start at T0.
        held: Some(Held {
            tail_end: Some(T0 + 3_600_000),
            ..held("a", T0 + 30_000, T0 + 60_000)
        }),
        queued: queued(1),
        ..record("r2")
    };
    assert_eq!(w.fx.registry.reservation_ledger(), vec![first, second]);
}

/// Asserts a restart over the ledger: the host is marked for the same request with the
/// same instants, the requests wait in the same order, the give-up count and the
/// exclusion are the first registry's to the field, and nothing is dropped. Then the
/// consumer presents both requests again: the mark, the places and the count are as they
/// were (the count is not reset, so the bound on give-ups holds across the restart). It
/// catches a restart that forgets the wait list, one that resets `max_reserve_attempts`
/// by forgetting the count, and a presentation that takes the place of a restored entry
/// to the back.
#[test]
fn a_restart_puts_back_the_mark_the_places_and_the_counters() {
    let w = two_waiting();
    let marks = w.reservations();
    assert_eq!(names(&marks), [("r2".to_string(), "a".to_string())]);
    let ledger = w.fx.registry.reservation_ledger();

    let (mut back, restored) = restarted(&w, &ledger, true);
    assert_eq!(
        restored,
        Restored {
            resumed: 2,
            dropped: 0
        }
    );
    assert_eq!(back.reservations(), marks);
    assert_eq!(back.fx.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(back.fx.registry.queue_position(&id("r2")), Some(1));
    for name in ["r1", "r2"] {
        let key = id(name);
        assert_eq!(
            back.fx.registry.request_record(&key),
            w.fx.registry.request_record(&key)
        );
    }

    back.ask(2, "r1");
    back.ask(3, "r2");
    assert_eq!(back.reservations(), marks);
    assert_eq!(back.fx.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(back.fx.registry.queue_position(&id("r2")), Some(1));
    let kept = back.fx.registry.request_record(&id("r1")).unwrap();
    assert_eq!(kept.give_ups, 1);
    let until = at(T0 + 30_000 + 300_000);
    assert_eq!(
        kept.exclusions.get(&host("a")),
        Some(&Exclusion::Until(until))
    );
}

/// Asserts that a request nobody presents again is let go with its reservation once the
/// regain grace has passed since the restart, and not before: one millisecond short of
/// it both requests still hold their places and the mark stands; at it both are gone,
/// two restored reservations are counted dropped, and the request's give-up count is
/// still kept (the counters outlive the wait). It catches an unclaimed record kept for
/// ever, one dropped early, and one dropped without being counted.
#[test]
fn a_request_that_is_not_presented_again_is_dropped_and_counted() {
    let w = two_waiting();
    let ledger = w.fx.registry.reservation_ledger();
    let (mut back, _) = restarted(&w, &ledger, true);

    back.step(14_999);
    assert_eq!(back.fx.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(back.reserved_host("r2"), Some("a".to_string()));
    assert_eq!(back.fx.registry.dropped_reservations(), 0);
    back.step(1);
    assert_eq!(back.fx.registry.queue_position(&id("r1")), None);
    assert_eq!(back.fx.registry.queue_position(&id("r2")), None);
    assert!(back.reservations().is_empty());
    assert_eq!(back.fx.registry.dropped_reservations(), 2);
    let kept = back.fx.registry.request_record(&id("r1")).unwrap();
    assert_eq!(kept.give_ups, 1);
}

/// Asserts that a mark on a host that never says hello again is dropped and counted once
/// the regain grace has passed, while the request that held it, presented again, keeps
/// waiting: another busy host is up, so the request is still servable. It catches a mark
/// on a gone host that holds the request's place for ever, and a drop that is not
/// counted.
#[test]
fn a_mark_on_a_host_that_never_returns_is_dropped_and_counted() {
    let w = two_waiting();
    let ledger = w.fx.registry.reservation_ledger();
    let (mut back, _) = restarted(&w, &ledger, false);
    back.join("b");
    back.share(10, HOUR_S);
    back.ask(2, "r1");
    back.ask(3, "r2");
    assert_eq!(back.reserved_host("r2"), Some("a".to_string()));

    back.step(14_999);
    assert_eq!(back.reserved_host("r2"), Some("a".to_string()));
    assert_eq!(back.fx.registry.dropped_reservations(), 0);
    back.step(1);
    assert_eq!(back.reserved_host("r2"), None);
    assert_eq!(back.fx.registry.dropped_reservations(), 1);
    assert_eq!(back.fx.registry.queue_position(&id("r2")), Some(1));
}

/// Asserts what a restore does with records that cannot stand, counting each: a request
/// id that is not one, or is listed twice, is dropped whole; a host two records claim
/// stays with the first and the second keeps its place without it; a host being released keeps no mark; a
/// mark with no place on the wait list is dropped and its counters kept. The one good
/// record keeps its assigned flag. It catches two marks on one host, a mark on a host
/// that is leaving, and a count that misses a kind of drop.
#[test]
fn records_that_cannot_stand_are_dropped_at_restore() {
    let w = two_waiting();
    let (back, _) = restarted(&w, &[], false);
    back.fx.registry.mark_ending(&host("e"));
    let good = ReservationRecord {
        held: Some(Held {
            assigned: true,
            ..held("a", T0, T0 + 60_000)
        }),
        queued: queued(0),
        ..record("r-a")
    };
    let same_host = ReservationRecord {
        held: Some(held("a", T0, T0 + 60_000)),
        queued: queued(1),
        ..record("r-b")
    };
    let bad_id = ReservationRecord {
        queued: queued(2),
        ..record("BAD ID")
    };
    let no_place = ReservationRecord {
        held: Some(held("d", T0, T0 + 60_000)),
        give_ups: 2,
        ..record("r-d")
    };
    let leaving = ReservationRecord {
        held: Some(held("e", T0, T0 + 60_000)),
        queued: queued(2),
        ..record("r-e")
    };
    let again = ReservationRecord {
        queued: queued(3),
        ..record("r-a")
    };
    let ledger = [good, same_host, bad_id, no_place, leaving, again];
    let restored = back.fx.registry.restore_reservations(&ledger);
    assert_eq!(
        restored,
        Restored {
            resumed: 4,
            dropped: 5
        }
    );
    let marks = back.reservations();
    assert_eq!(names(&marks), [("r-a".to_string(), "a".to_string())]);
    assert!(marks[0].assigned);
    assert_eq!(back.fx.registry.queue_position(&id("r-a")), Some(0));
    assert_eq!(back.fx.registry.queue_position(&id("r-b")), Some(1));
    assert_eq!(back.fx.registry.queue_position(&id("r-e")), Some(2));
    assert_eq!(back.fx.registry.queue_position(&id("r-d")), None);
    assert_eq!(
        back.fx
            .registry
            .request_record(&id("r-d"))
            .unwrap()
            .give_ups,
        2
    );
    assert_eq!(back.fx.registry.dropped_reservations(), 5);
}

/// Asserts that a reservation made after the give-up limit comes back capped: with two
/// give-ups on the record (the default limit) and a deadline that passes while the host
/// is busy, the request keeps its host and its count stays two. It catches a restart
/// that makes a capped reservation givable-up again, which would let a restart loop
/// push the count past the bound the design promises.
#[test]
fn a_capped_reservation_is_not_given_up_at_its_deadline_after_a_restart() {
    let w = World::of(pool(), &[]);
    let capped = ReservationRecord {
        held: Some(held("a", T0, T0 + 10_000)),
        queued: queued(0),
        give_ups: 2,
        ..record("r1")
    };
    let (mut back, restored) = restarted(&w, &[capped], true);
    assert_eq!(restored.dropped, 0);
    back.ask(2, "r1");
    back.step(20_000);
    assert_eq!(back.reserved_host("r1"), Some("a".to_string()));
    assert_eq!(
        back.fx.registry.request_record(&id("r1")).unwrap().give_ups,
        2
    );
}

/// Asserts that the count of failed prepares comes back with its consequence: a request
/// that had failed `max_replace` (three) times is terminal and its next start is refused
/// `exclusive-not-ready` and clears the record; one short of it is not terminal and is
/// not refused. It catches a restart that gives a request that used up its tries the
/// full number again.
#[test]
fn a_request_that_used_up_its_tries_is_still_terminal_after_a_restart() {
    let w = World::of(pool(), &[]);
    let used = ReservationRecord {
        prepare_failures: 3,
        ..record("r-t")
    };
    let short = ReservationRecord {
        prepare_failures: 2,
        ..record("r-s")
    };
    let (mut back, restored) = restarted(&w, &[used, short], false);
    assert_eq!(
        restored,
        Restored {
            resumed: 2,
            dropped: 0
        }
    );
    assert!(
        back.fx
            .registry
            .request_record(&id("r-t"))
            .unwrap()
            .terminal
    );
    assert!(
        !back
            .fx
            .registry
            .request_record(&id("r-s"))
            .unwrap()
            .terminal
    );

    let refused = StartAck::Refused(RefusalCode::ExclusiveNotReady);
    assert_eq!(back.poll(2, "r-t", 600), refused);
    assert_eq!(back.fx.registry.request_record(&id("r-t")), None);
    assert_ne!(back.poll(3, "r-s", 600), refused);
}

/// Asserts that an exclusion for the rest of a request's life comes back from a restart
/// and keeps the request off the host. Reached as production reaches it: the request,
/// which carries an id, is committed to host `a`, and `a`'s agent refuses the lease as
/// `unsupported`. The ledger then holds one record with one failed prepare and `a`
/// excluded for good; a second registry given it holds the same record, and the same
/// request, started again, is placed on `b` and never on `a`. It catches an exclusion
/// that is kept as a timed one or not at all, which would offer the host again.
#[test]
fn an_exclusion_for_life_comes_back_and_keeps_the_request_off_the_host() {
    let first = fixture_of(pool());
    let epochs = [("a", first.join("a", "t")), ("b", first.join("b", "t"))];
    let mut provider = first.provider.clone();
    let spec = World::whole_spec(1, Some("r1"), 600);
    assert_eq!(provider.start(spec), StartAck::Unknown);
    let mut message = first.heartbeat("a", epochs[0].1);
    message.exclusive_refused = Some(ExclusiveRefusal {
        lease_id: "1-1".into(),
        code: RefusalReason::Unsupported,
        detail: "no hook".into(),
    });
    first.registry.heartbeat(&message, &cred("t")).unwrap();
    let before = first.registry.request_record(&id("r1")).unwrap();
    assert_eq!(before.exclusions.get(&host("a")), Some(&Exclusion::ForLife));
    let ledger = first.registry.reservation_ledger();
    assert_eq!(ledger.len(), 1);

    let second = fixture_of(pool());
    let restored = second.registry.restore_reservations(&ledger);
    assert_eq!(
        restored,
        Restored {
            resumed: 1,
            dropped: 0
        }
    );
    second.join("a", "t");
    second.join("b", "t");
    let kept = second.registry.request_record(&id("r1")).unwrap();
    assert_eq!(kept.exclusions.get(&host("a")), Some(&Exclusion::ForLife));
    assert_eq!(kept.prepare_failures, 1);
    let mut provider = second.provider.clone();
    assert_eq!(
        provider.start(World::whole_spec(2, Some("r1"), 600)),
        StartAck::Unknown
    );
    assert_eq!(provider.booked().get(&host("b")), Some(&lease(2)));
    assert_eq!(provider.booked().get(&host("a")), None);
}

/// Asserts the wait list's size cap at a restore. A ledger of 1026 records, each with a
/// place, is given: the first 1024 by place are put back in order, the last two are
/// dropped and counted, and the registry's own cap on the list is the same constant. It
/// catches a restore that loads a list the live path would never hold.
#[test]
fn a_ledger_past_the_wait_list_cap_is_trimmed_and_counted() {
    let w = World::of(pool(), &[]);
    let over = js_registry::MAX_WAITING_REQUESTS + 2;
    let ledger: Vec<ReservationRecord> = (0..over)
        .map(|n| ReservationRecord {
            queued: queued(u32::try_from(n).unwrap()),
            ..record(&format!("q{n:04}"))
        })
        .collect();
    let (back, restored) = restarted(&w, &ledger, false);
    assert_eq!(
        restored,
        Restored {
            resumed: js_registry::MAX_WAITING_REQUESTS,
            dropped: 2
        }
    );
    let last = js_registry::MAX_WAITING_REQUESTS - 1;
    let kept = id(&format!("q{last:04}"));
    assert_eq!(back.fx.registry.queue_position(&kept), Some(last));
    let first_over = id(&format!("q{:04}", last + 1));
    assert_eq!(back.fx.registry.queue_position(&first_over), None);
    assert_eq!(back.fx.registry.whole_counts().queued, last + 1);
    assert_eq!(back.fx.registry.dropped_reservations(), 2);
}

/// Asserts the bound on exclusions per record: a request is kept off a host at most
/// once, so the pool's machine limit (four here) is the bound. A record with six hosts
/// keeps the first four by id, is counted as one drop, and its give-up count is kept; a
/// record with exactly four is untouched. It catches an unbounded list loaded from an
/// edited file.
#[test]
fn exclusions_past_the_pool_size_are_trimmed_and_counted() {
    let w = World::of(pool(), &[]);
    let barred = |names: &[&str]| -> Vec<LeaseExclusion> {
        let hosts = names.iter().map(|name| LeaseExclusion {
            host: host(name),
            until: ExclusionEnd::Forever,
        });
        hosts.collect()
    };
    let many = ReservationRecord {
        give_ups: 1,
        exclusions: barred(&["h1", "h2", "h3", "h4", "h5", "h6"]),
        ..record("r-many")
    };
    let exact = ReservationRecord {
        give_ups: 1,
        exclusions: barred(&["h1", "h2", "h3", "h4"]),
        ..record("r-exact")
    };
    let (back, restored) = restarted(&w, &[many, exact], false);
    assert_eq!(
        restored,
        Restored {
            resumed: 2,
            dropped: 1
        }
    );
    let kept = back.fx.registry.request_record(&id("r-many")).unwrap();
    let hosts: Vec<String> = kept.exclusions.keys().map(|h| h.to_string()).collect();
    assert_eq!(hosts, ["h1", "h2", "h3", "h4"]);
    assert_eq!(kept.give_ups, 1);
    let exact = back.fx.registry.request_record(&id("r-exact")).unwrap();
    assert_eq!(exact.exclusions.len(), 4);
    assert_eq!(back.fx.registry.dropped_reservations(), 1);
}
