//! The heartbeat's sequence and the hello's agent process id, against a scripted server. No
//! clock is read and nothing sleeps.

use js_agent::{HEARTBEAT_PATH, HELLO_PATH, Session};
use serde_json::{Value, json};

use crate::support::{
    Script, Sent, beat_ok, hello, linux_host, lowest, refused, run_scripted, welcome,
};

/// The `seq` of every heartbeat sent, in order, and `null` for each hello, so a reset shows.
fn seqs(sent: &Sent) -> Vec<Value> {
    let sent = sent.borrow();
    let pick = |(path, body): &(String, Value)| match path.as_str() {
        HEARTBEAT_PATH => body["seq"].clone(),
        _ => Value::Null,
    };
    sent.iter().map(pick).collect()
}

/// Asserts the first heartbeat after a welcome carries `seq` 1 and each later one the next
/// number. Catches a beat with no sequence (a server then cannot order beats) or one that
/// starts at 0.
#[test]
fn the_sequence_starts_at_one_after_a_welcome_and_rises() {
    let (mut transport, sent) = Script::new(vec![welcome(1), beat_ok(), beat_ok(), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    (0..4).for_each(|_| drop(session.tick()));
    assert_eq!(seqs(&sent), [json!(null), json!(1), json!(2), json!(3)]);
}

/// Asserts a heartbeat that failed (no answer, then a 503) is followed by one with the next
/// number, not the same one. Catches a retry that reuses the sequence, which a server would
/// take for a late duplicate and answer without applying.
#[test]
fn a_retry_after_a_failed_exchange_has_a_new_sequence() {
    let (mut transport, sent) = Script::new(vec![
        welcome(1),
        Err("connection reset".to_owned()),
        refused(503, "unavailable"),
        beat_ok(),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    (0..4).for_each(|_| drop(session.tick()));
    assert_eq!(seqs(&sent), [json!(null), json!(1), json!(2), json!(3)]);
}

/// Asserts the sequence starts at 1 again after a re-hello and its welcome. Catches a counter
/// that goes on across sessions: a new session's first beat would then look older than it is.
#[test]
fn a_new_hello_resets_the_sequence() {
    let (mut transport, sent) = Script::new(vec![
        welcome(1),
        beat_ok(),
        beat_ok(),
        refused(409, "unknown-host"),
        welcome(2),
        beat_ok(),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    (0..6).for_each(|_| drop(session.tick()));
    let paths: Vec<String> = sent.borrow().iter().map(|(path, _)| path.clone()).collect();
    assert_eq!(paths[4], HELLO_PATH);
    assert_eq!(
        seqs(&sent),
        [
            json!(null),
            json!(1),
            json!(2),
            json!(3),
            json!(null),
            json!(1)
        ]
    );
}

/// Asserts the drain's heartbeat also takes the next number. Catches a path that sends a
/// beat without raising the sequence.
#[test]
fn the_draining_heartbeat_takes_the_next_sequence() {
    let (mut transport, sent) = Script::new(vec![welcome(1), beat_ok(), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    (0..2).for_each(|_| drop(session.tick()));
    assert!(session.drain().is_some());
    assert_eq!(seqs(&sent), [json!(null), json!(1), json!(2)]);
}

/// Asserts every hello of a session names the same agent process, the first and the one after
/// a lost session. Catches an id drawn per hello, which would make the server take the
/// re-hello for another process and refuse it `host-live`.
#[test]
fn every_hello_of_a_session_carries_the_same_agent_process() {
    let (mut transport, sent) = Script::new(vec![
        welcome(1),
        beat_ok(),
        refused(409, "unknown-host"),
        welcome(2),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest())
        .with_agent_process(Some("0123456789abcdef0123456789abcdef".to_owned()));
    (0..4).for_each(|_| drop(session.tick()));
    let sent = sent.borrow();
    let hellos: Vec<&Value> = sent
        .iter()
        .filter(|(path, _)| path == HELLO_PATH)
        .map(|(_, body)| &body["agent_process"])
        .collect();
    assert_eq!(hellos, [&json!("0123456789abcdef0123456789abcdef"); 2]);
    assert!(sent[1].1.get("agent_process").is_none());
}

/// Asserts a session given no process id sends a hello without the field. Catches a default
/// id that an agent of a test or an older caller would then claim.
#[test]
fn a_session_without_an_agent_process_sends_none() {
    let (mut transport, sent) = Script::new(vec![welcome(1)]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    drop(session.tick());
    assert!(sent.borrow()[0].1.get("agent_process").is_none());
}

/// Asserts the program's own hello names an agent process: 32 lowercase hex characters, in
/// the first and the re-hello after a lost session, both the same. Catches an agent that
/// never draws an id, or draws a new one for each hello.
#[test]
fn the_program_draws_one_agent_process_for_all_its_hellos() {
    let replies = vec![welcome(1), refused(409, "unknown-host"), welcome(2)];
    let (_, sent, _) = run_scripted(&["--host-id", "h1"], &linux_host(), replies, 3);
    let sent = sent.borrow();
    let ids: Vec<&str> = sent
        .iter()
        .filter(|(path, _)| path == HELLO_PATH)
        .filter_map(|(_, body)| body["agent_process"].as_str())
        .collect();
    assert_eq!(ids.len(), 2, "{sent:?}");
    assert_eq!(ids[0], ids[1]);
    assert_eq!(ids[0].len(), 32);
}
