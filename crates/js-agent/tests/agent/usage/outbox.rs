//! The hand-over between the worker and the session, and the wire form of a reading.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use js_agent::usage::{ClosedMinute, MAX_UNSENT_AGE_USEC, Outbox, Reading, Sample};
use serde_json::json;

/// One closed minute of `lease` with a mean of `mean` thousandths of a core.
pub fn minute(lease: &str, minute: u16, mean: u32) -> ClosedMinute {
    ClosedMinute {
        lease_id: lease.to_owned(),
        minute,
        cpu_mean_millis: mean,
        cpu_peak_window_millis: mean + 1,
        ws_max_bytes: 4096,
    }
}

/// A reading taken at `at` microseconds that carries `minutes`.
pub fn reading(seq: u64, at: u64, minutes: Vec<ClosedMinute>) -> Reading {
    Reading {
        seq,
        sample: Sample {
            mono_usec: at,
            cpu_usage_usec: 7_000,
            working_set_bytes: 8_000,
            active_file_bytes: 9_000,
            cpu_limited: true,
            leases: Vec::new(),
            unreadable_leases: 0,
        },
        covered: vec!["1-1".to_owned()],
        clocks: vec![("1-1".to_owned(), 10), ("1-2".to_owned(), 20)],
        minutes,
    }
}

/// The minutes of a load, as `(lease, minute)`.
fn names(minutes: &[ClosedMinute]) -> Vec<(String, u16)> {
    minutes
        .iter()
        .map(|m| (m.lease_id.clone(), m.minute))
        .collect()
}

/// Asserts the wire form of a reading field by field, with a different value in every
/// field: the sample's counters and gauges, the covered leases, the clocks and the minutes
/// in the order they were given. It catches two fields swapped (the working set and the
/// active file cache), a dropped list and a renamed key.
#[test]
fn a_reading_is_sent_field_for_field() {
    let outbox = Outbox::new();
    outbox.publish(reading(3, 5_000, vec![minute("1-1", 0, 900)]));
    let load = outbox.take().expect("a reading was published");
    assert_eq!(
        serde_json::to_value(load).unwrap(),
        json!({
            "seq": 3,
            "mono_usec": 5_000,
            "cpu_usage_usec": 7_000,
            "working_set_bytes": 8_000,
            "active_file_bytes": 9_000,
            "actions_cpu_limited": true,
            "covered": ["1-1"],
            "clocks": [
                { "lease_id": "1-1", "started_mono_usec": 10 },
                { "lease_id": "1-2", "started_mono_usec": 20 },
            ],
            "minutes": [{
                "lease_id": "1-1",
                "minute": 0,
                "cpu_mean_millis": 900,
                "cpu_peak_window_millis": 901,
                "ws_max_bytes": 4096,
            }],
        })
    );
}

/// Asserts that an empty outbox gives no load, that a reading is taken once, and that the
/// heartbeat interval is the window (0 until it is set, saturating for a duration that does
/// not fit). It catches a load repeated on every beat and a worker that samples before it
/// has a session.
#[test]
fn a_reading_is_taken_once_and_the_window_is_the_sessions() {
    let outbox = Outbox::new();
    assert!(outbox.take().is_none());
    assert_eq!(outbox.window_usec(), 0);
    outbox.set_window(Duration::from_secs(5));
    assert_eq!(outbox.window_usec(), 5_000_000);
    outbox.set_window(Duration::MAX);
    assert_eq!(outbox.window_usec(), u64::MAX);
    outbox.publish(reading(1, 0, Vec::new()));
    assert!(outbox.take().is_some());
    assert!(outbox.take().is_none());
}

/// Asserts that a delivered load leaves nothing to resend and a failed one gives its
/// minutes back exactly once. It catches minutes resent for ever after a 2xx (the
/// duplicate defect) and minutes lost with a failed send.
#[test]
fn delivered_minutes_are_gone_and_failed_ones_come_back_once() {
    let outbox = Outbox::new();
    outbox.publish(reading(1, 100, vec![minute("1-1", 0, 5)]));
    outbox.take();
    outbox.delivered();
    assert!(outbox.reclaim(100).is_empty());

    outbox.publish(reading(2, 200, vec![minute("1-1", 1, 6)]));
    outbox.take();
    outbox.failed();
    assert_eq!(names(&outbox.reclaim(200)), [("1-1".to_owned(), 1)]);
    assert!(outbox.reclaim(200).is_empty(), "back once");
    assert_eq!(outbox.dropped(), 0);
}

/// Asserts that a load taken and never answered counts as failed when the next one is taken,
/// so the minutes of the first are not lost and not sent with the second twice, and that the
/// second, once delivered, is not given back. It catches a session that takes twice without
/// an answer (a beat that was never sent) and loses or doubles the first.
#[test]
fn two_takes_without_an_answer_never_send_a_minute_twice() {
    let outbox = Outbox::new();
    outbox.publish(reading(1, 100, vec![minute("1-1", 0, 5)]));
    let first = outbox.take().unwrap();
    outbox.publish(reading(2, 200, vec![minute("1-1", 1, 6)]));
    let second = outbox.take().unwrap();
    assert_eq!((first.minutes.len(), second.minutes.len()), (1, 1));
    outbox.delivered();
    assert_eq!(names(&outbox.reclaim(200)), [("1-1".to_owned(), 0)]);
}

/// Asserts that a reading nobody took is replaced by the next one and its minutes are kept,
/// and that the worker reclaims the minutes of an untaken reading (which is then gone). It
/// catches minutes lost when the worker samples faster than the session beats.
#[test]
fn an_untaken_reading_keeps_its_minutes_for_the_worker() {
    let outbox = Outbox::new();
    outbox.publish(reading(1, 100, vec![minute("1-1", 0, 5)]));
    outbox.publish(reading(2, 200, vec![minute("1-1", 1, 6)]));
    let load = outbox.take().unwrap();
    assert_eq!(load.seq, 2);
    assert_eq!(names(&outbox.reclaim(200)), [("1-1".to_owned(), 0)]);

    outbox.publish(reading(3, 300, vec![minute("1-1", 2, 7)]));
    assert_eq!(names(&outbox.reclaim(300)), [("1-1".to_owned(), 2)]);
    assert!(outbox.take().is_none(), "the reading was discarded");
}

/// Asserts the bound on how long a minute is kept: minutes of a sample exactly the bound
/// old are given back, one microsecond older are dropped and counted. It catches minutes
/// kept for ever through a long outage.
#[test]
fn minutes_older_than_the_bound_are_dropped_and_counted() {
    let outbox = Outbox::new();
    outbox.publish(reading(
        1,
        100,
        vec![minute("1-1", 0, 5), minute("1-2", 0, 5)],
    ));
    outbox.take();
    outbox.failed();
    assert_eq!(outbox.reclaim(100 + MAX_UNSENT_AGE_USEC).len(), 2);
    assert_eq!(outbox.dropped(), 0);

    outbox.publish(reading(
        2,
        100,
        vec![minute("1-1", 1, 5), minute("1-2", 1, 5)],
    ));
    outbox.take();
    outbox.failed();
    assert!(outbox.reclaim(101 + MAX_UNSENT_AGE_USEC).is_empty());
    assert_eq!(outbox.dropped(), 2);
}

/// Asserts that a refused beat drops its minutes and counts them, that the answers with
/// nothing in flight change nothing, and that the load that follows is clean. It catches
/// a refused beat whose minutes are resent for ever.
#[test]
fn a_refused_load_is_dropped_and_counted() {
    let outbox = Outbox::new();
    outbox.refused();
    outbox.failed();
    outbox.delivered();
    assert_eq!(outbox.dropped(), 0);
    outbox.publish(reading(
        1,
        100,
        vec![minute("1-1", 0, 5), minute("1-1", 1, 5)],
    ));
    outbox.take();
    outbox.refused();
    assert!(outbox.reclaim(100).is_empty());
    assert_eq!(outbox.dropped(), 2);
    outbox.publish(reading(2, 200, Vec::new()));
    assert!(outbox.take().unwrap().minutes.is_empty());
}

/// Asserts that a delivered load is settled for good, whatever the session does next: the
/// next take, with no new reading and with one, gives back nothing of it, the worker
/// reclaims nothing and nothing is counted as dropped. The settling shows only at the take
/// after the 2xx (an unsettled load is then counted as unanswered and given back), so a
/// check right after `delivered` cannot see it. It catches a session that forgets to say
/// `delivered` on a 2xx and an outbox whose `delivered` clears nothing: both resend the
/// same minutes on a later beat.
#[test]
fn a_delivered_load_is_not_given_back_by_a_later_take() {
    let delivered = || {
        let outbox = Outbox::new();
        outbox.publish(reading(1, 100, vec![minute("1-1", 0, 5)]));
        assert_eq!(outbox.take().unwrap().minutes.len(), 1);
        outbox.delivered();
        outbox
    };

    let outbox = delivered();
    assert!(outbox.take().is_none(), "no new reading");
    assert!(outbox.reclaim(100).is_empty());
    assert_eq!(outbox.dropped(), 0);

    let outbox = delivered();
    outbox.publish(reading(2, 150, vec![minute("1-1", 1, 6)]));
    let next = outbox.take().unwrap();
    let carried: Vec<_> = next
        .minutes
        .iter()
        .map(|m| (m.lease_id.as_str(), m.minute))
        .collect();
    assert_eq!(carried, [("1-1", 1)], "only the new minute");
    assert!(
        outbox.reclaim(150).is_empty(),
        "minute 0 does not come back"
    );
    assert_eq!(outbox.dropped(), 0);
}

/// Asserts that a thread that died holding the outbox's lock does not take the hand-over
/// down with it: a reading is published and taken, a failed beat gives its minutes back
/// and the counts are read. It catches a lock that panics on poison, which would kill the
/// session's heartbeat (or the worker) over a fault of the other thread.
#[test]
fn a_poisoned_lock_does_not_stop_the_hand_over() {
    let outbox = Arc::new(Outbox::new());
    let poisoner = outbox.clone();
    let died = thread::spawn(move || poisoner.panic_while_locked()).join();
    assert!(died.is_err(), "the thread panicked");

    outbox.publish(reading(1, 100, vec![minute("1-1", 0, 5)]));
    assert_eq!(outbox.take().unwrap().minutes.len(), 1);
    outbox.failed();
    assert_eq!(names(&outbox.reclaim(100)), [("1-1".to_owned(), 0)]);
    outbox.refused();
    outbox.delivered();
    assert_eq!(outbox.dropped(), 0);
}
