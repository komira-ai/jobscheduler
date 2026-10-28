//! The session and the outbox: what the heartbeat carries, and what each answer does to the
//! minutes that were in it. The server is a script; nothing waits.

use std::sync::Arc;

use js_agent::usage::Outbox;
use js_agent::{HEARTBEAT_PATH, Note, Session};
use serde_json::{Value, json};

use super::outbox::{minute, reading};
use crate::support::{Reply, Script, Sent, beat_ok, hello, lowest, refused, welcome};

/// A session over `script` that takes its load from `outbox`.
fn session_over<'a>(script: &'a mut Script, outbox: &Arc<Outbox>) -> Session<'a> {
    Session::new(hello("h1"), script, lowest()).with_usage(outbox.clone())
}

/// The heartbeats sent so far.
fn beats(sent: &Sent) -> Vec<Value> {
    let sent = sent.borrow();
    let beats = sent.iter().filter(|(path, _)| path == HEARTBEAT_PATH);
    beats.map(|(_, body)| body.clone()).collect()
}

/// Asserts that the welcome's heartbeat interval is the window the worker samples at, and
/// that the heartbeat after it carries the outbox's reading in every field. It catches a
/// worker that never learns its window and a beat that leaves the load out.
#[test]
fn the_beat_carries_the_load_and_the_welcome_sets_the_window() {
    let outbox = Arc::new(Outbox::new());
    let (mut script, sent) = Script::new(vec![welcome(1), beat_ok()]);
    let mut session = session_over(&mut script, &outbox);
    session.tick();
    assert_eq!(outbox.window_usec(), 5_000_000, "the welcome's 5000 ms");
    outbox.publish(reading(4, 9_000, vec![minute("1-1", 2, 700)]));
    session.tick();
    let beat = &beats(&sent)[0];
    assert_eq!(
        beat["load"],
        json!({
            "seq": 4,
            "mono_usec": 9_000,
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
                "minute": 2,
                "cpu_mean_millis": 700,
                "cpu_peak_window_millis": 701,
                "ws_max_bytes": 4096,
            }],
        })
    );
}

/// Asserts that a beat with nothing published has no `load` key, that a session with no
/// outbox never has one, and that a published load rides on one beat only. It catches a
/// load repeated on every beat and a `load: null` that an older server would take badly.
#[test]
fn a_load_rides_on_one_beat_only() {
    let outbox = Arc::new(Outbox::new());
    let (mut script, sent) = Script::new(vec![welcome(1), beat_ok(), beat_ok(), beat_ok()]);
    let mut session = session_over(&mut script, &outbox);
    session.tick();
    session.tick();
    outbox.publish(reading(1, 100, Vec::new()));
    session.tick();
    session.tick();
    let loads: Vec<bool> = beats(&sent)
        .iter()
        .map(|beat| beat.get("load").is_some())
        .collect();
    assert_eq!(loads, [false, true, false]);

    let (mut plain, sent) = Script::new(vec![welcome(1), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut plain, lowest());
    session.tick();
    session.tick();
    assert!(beats(&sent)[0].get("load").is_none());
}

/// Asserts what each answer to a beat does to the minutes it carried. A 2xx delivers them
/// (they are not given back). A transport error, a conflict (409, 403 `session-mismatch`),
/// 408, 429 and 5xx keep them to ride on a later beat. Any other 4xx, `invalid-heartbeat`
/// among them, which also ends the session, drops them and counts them: the same bytes
/// would be refused again, so a refusal for another fault must not requeue for ever. It
/// catches `delivered` forgotten on a 2xx (minutes resent for ever), `failed` called on a
/// refusal (minutes requeued for ever), and minutes lost on an answer that asks for a retry.
#[test]
fn each_answer_settles_the_minutes_it_carried() {
    let down: Reply = Err("the server is down".to_owned());
    let cases: Vec<(&str, Reply, usize, u64)> = vec![
        ("2xx", beat_ok(), 0, 0),
        ("transport error", down, 1, 0),
        ("500", refused(500, "internal"), 1, 0),
        ("503", refused(503, "unavailable"), 1, 0),
        ("408", refused(408, "timeout"), 1, 0),
        ("429", refused(429, "rate-limited"), 1, 0),
        ("409", refused(409, "stale-epoch"), 1, 0),
        (
            "403 session-mismatch",
            refused(403, "session-mismatch"),
            1,
            0,
        ),
        (
            "400 invalid-heartbeat",
            refused(400, "invalid-heartbeat"),
            0,
            1,
        ),
        ("401", refused(401, "unauthorized"), 0, 1),
    ];
    for (name, answer, kept, dropped) in cases {
        let outbox = Arc::new(Outbox::new());
        let (mut script, _) = Script::new(vec![welcome(1), answer]);
        let mut session = session_over(&mut script, &outbox);
        session.tick();
        outbox.publish(reading(1, 100, vec![minute("1-1", 0, 5)]));
        session.tick();
        assert_eq!(outbox.reclaim(100).len(), kept, "{name}");
        assert_eq!(outbox.dropped(), dropped, "{name}");
    }
}

/// Asserts that the draining beat carries the load too, and that it is settled like any
/// other: delivered when the server took it, kept when it did not. It catches the last
/// minutes of leases that just ended being left behind by a drain.
#[test]
fn the_draining_beat_carries_the_load() {
    let outbox = Arc::new(Outbox::new());
    let (mut script, sent) = Script::new(vec![welcome(1), beat_ok()]);
    let mut session = session_over(&mut script, &outbox);
    session.tick();
    outbox.publish(reading(1, 100, vec![minute("1-1", 3, 5)]));
    assert_eq!(session.drain(), Some(Note::Draining { told: true }));
    let beat = &beats(&sent)[0];
    assert_eq!(beat["phase"], "draining");
    assert_eq!(beat["load"]["minutes"][0]["minute"], 3);
    assert!(outbox.reclaim(100).is_empty());

    let outbox = Arc::new(Outbox::new());
    let (mut down, _) = Script::new(vec![welcome(1)]);
    let mut session = session_over(&mut down, &outbox);
    session.tick();
    outbox.publish(reading(1, 100, vec![minute("1-1", 3, 5)]));
    assert_eq!(session.drain(), Some(Note::Draining { told: false }));
    assert_eq!(outbox.reclaim(100).len(), 1, "the beat was not told");
}
