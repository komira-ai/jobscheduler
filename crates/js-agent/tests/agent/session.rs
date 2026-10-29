//! The session state machine against a scripted server: every answer the server can give
//! and what the agent does with it. No clock is read and nothing sleeps: a tick returns
//! the wait it wants and the test reads it.

use std::time::Duration;

use js_agent::{
    Answer, Fatal, HEARTBEAT_PATH, HELLO_PATH, Jitter, Note, RETRY_AFTER_CAP, Reason, Session, Step,
};
use js_agent_wire::ProtocolRange;
use serde_json::{Value, json};

use crate::support::{
    Reply, SECRET, Script, Sent, beat_ok, hello, limited, lowest, refused, reply, welcome,
    welcome_speaking, welcome_with,
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The wait of a step that waits.
fn wait(step: &Step) -> Duration {
    match step {
        Step::Wait { after, .. } => *after,
        Step::Fatal(fatal) => panic!("fatal: {fatal}"),
        Step::Failed(why) => panic!("failed: {why}"),
    }
}

/// The note of a step that waits.
fn note(step: &Step) -> Option<Note> {
    match step {
        Step::Wait { note, .. } => note.clone(),
        Step::Fatal(fatal) => panic!("fatal: {fatal}"),
        Step::Failed(why) => panic!("failed: {why}"),
    }
}

/// Ticks `session` `n` times and returns the steps.
fn ticks(session: &mut Session<'_>, n: usize) -> Vec<Step> {
    (0..n).map(|_| session.tick()).collect()
}

fn paths(sent: &Sent) -> Vec<String> {
    sent.borrow().iter().map(|(path, _)| path.clone()).collect()
}

/// A session of host `h1` over `replies`, with the low end of every jitter.
fn script(replies: Vec<Reply>) -> (Script, Sent) {
    Script::new(replies)
}

/// Asserts that the session secret from the welcome is in every heartbeat, with the epoch,
/// and in nothing else: not in the hello, not in a step or a note's text. It catches a
/// heartbeat that forgets the secret (the server then refuses it as `session-mismatch`)
/// and a secret that reaches a log line.
#[test]
fn the_session_secret_is_sent_in_every_heartbeat_and_nowhere_else() {
    let (mut transport, sent) = script(vec![welcome(7), beat_ok(), beat_ok(), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 4);

    let registered = Note::Registered {
        epoch: 7,
        interval: Duration::from_secs(5),
    };
    assert_eq!(note(&steps[0]), Some(registered));
    assert_eq!(wait(&steps[0]), Duration::from_secs(5));
    for step in &steps[1..] {
        assert_eq!((wait(step), note(step)), (Duration::from_secs(5), None));
    }
    let sent = sent.borrow();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[0].0, HELLO_PATH);
    assert!(sent[0].1.get("session").is_none());
    for (path, body) in &sent[1..] {
        assert_eq!(path, HEARTBEAT_PATH);
        assert_eq!(body["session"], SECRET);
        assert_eq!(body["epoch"], 7);
        assert_eq!(body["host_id"], "h1");
        assert_eq!(body["phase"], "serving");
        assert_eq!(body["held"], json!([]));
    }
    let shown = format!("{steps:?}");
    assert!(!shown.contains(SECRET), "{shown}");
    let notes: Vec<String> = steps
        .iter()
        .filter_map(note)
        .map(|n| n.to_string())
        .collect();
    assert!(!notes.join("\n").contains(SECRET));
}

/// Asserts that a hello the wire refuses never leaves the agent: every tick reports it as
/// an unusable message and nothing is sent. It catches a send path that skips `validate`.
#[test]
fn an_invalid_hello_is_not_sent() {
    let (mut transport, sent) = script(vec![welcome(1)]);
    let mut session = Session::new(hello("UPPER"), &mut transport, lowest());
    for step in ticks(&mut session, 3) {
        let unusable = Note::Retrying {
            what: "hello",
            reason: Reason::Unusable("invalid-hello"),
        };
        assert_eq!(note(&step), Some(unusable));
    }
    assert!(sent.borrow().is_empty());
}

/// Asserts that a refusal asking again cannot change ends the agent at once with the
/// status and the server's code, after exactly one request, for a hello and for a
/// heartbeat: 401 with no body, 403 for each admission refusal, and the 4xx for a hello
/// the server cannot read or serve. It catches an agent that retries a refusal in a loop.
#[test]
fn refused_hello_is_fatal_per_code() {
    let cases: Vec<(Reply, u16, &str)> = vec![
        (reply(401, Value::Null), 401, "unreadable"),
        (refused(403, "host-bound"), 403, "host-bound"),
        (refused(403, "unknown-pool"), 403, "unknown-pool"),
        (
            refused(403, "shape-not-accepted"),
            403,
            "shape-not-accepted",
        ),
        (refused(403, "host-listed"), 403, "host-listed"),
        (refused(403, "host-contract"), 403, "host-contract"),
        (
            refused(403, "execution-mismatch"),
            403,
            "execution-mismatch",
        ),
        (refused(400, "invalid-hello"), 400, "invalid-hello"),
        (
            refused(400, "unsupported-protocol"),
            400,
            "unsupported-protocol",
        ),
        (refused(413, "body-too-large"), 413, "body-too-large"),
    ];
    for (answer, status, code) in cases {
        let (mut transport, sent) = script(vec![answer]);
        let mut session = Session::new(hello("h1"), &mut transport, lowest());
        let fatal = Fatal {
            what: "hello",
            status,
            code: code.to_owned(),
            supported: None,
            field: None,
        };
        assert_eq!(session.tick(), Step::Fatal(fatal), "{status} {code}");
        assert_eq!(paths(&sent), [HELLO_PATH], "{status} {code}");
    }
    for (answer, status, code) in [
        (reply(401, Value::Null), 401, "unreadable"),
        (refused(403, "host-bound"), 403, "host-bound"),
        (refused(400, "invalid-heartbeat"), 400, "invalid-heartbeat"),
    ] {
        let (mut transport, sent) = script(vec![welcome(1), answer]);
        let mut session = Session::new(hello("h1"), &mut transport, lowest());
        session.tick();
        let fatal = Fatal {
            what: "heartbeat",
            status,
            code: code.to_owned(),
            supported: None,
            field: None,
        };
        assert_eq!(session.tick(), Step::Fatal(fatal));
        assert_eq!(sent.borrow().len(), 2);
    }
}

/// Asserts that the two refusals of the host contract reach the caller with the capability
/// the server named under `field`, and print it; that a field that is not a plain token is
/// dropped (the code stays); and that a refusal from a server that sends no field, as every
/// older server, is fatal with none. It catches an agent that throws the field away, so an
/// operator cannot tell what the host lacks, and one that prints server text unchecked.
#[test]
fn a_host_contract_refusal_carries_the_capability_at_fault() {
    let cases = [
        (json!("runtime"), Some("runtime")),
        (json!("gpu-toolkit"), Some("gpu-toolkit")),
        (json!("has a space"), None),
        (json!(""), None),
        (json!(7), None),
    ];
    for (sent, expected) in cases {
        let body = json!({ "error": "host-contract", "field": sent });
        let (mut transport, _) = script(vec![reply(403, body)]);
        let mut session = Session::new(hello("h1"), &mut transport, lowest());
        let Step::Fatal(fatal) = session.tick() else {
            panic!("a host-contract refusal must be fatal");
        };
        assert_eq!(fatal.code, "host-contract");
        assert_eq!(fatal.field.as_deref(), expected, "{sent}");
        let said = fatal.to_string();
        match expected {
            Some(field) => assert!(
                said.contains(&format!("capability at fault is {field}")),
                "{said}"
            ),
            None => assert!(!said.contains("capability at fault"), "{said}"),
        }
    }
}

/// Asserts that an `unsupported-protocol` refusal reaches the caller with the majors the
/// server named under `supported`, and prints them; that a range that is empty or starts
/// below 1 is dropped (the code stays); and that a refusal without the field has none.
/// It catches an agent that throws the majors away, so an operator cannot tell which
/// agent version to install.
#[test]
fn an_unsupported_protocol_refusal_carries_the_servers_majors() {
    let cases = [
        (
            json!({ "min": 2, "max": 3 }),
            Some(ProtocolRange { min: 2, max: 3 }),
        ),
        (json!({ "min": 3, "max": 2 }), None),
        (json!({ "min": 0, "max": 2 }), None),
        (json!("1 to 2"), None),
    ];
    for (supported, expected) in cases {
        let body = json!({ "error": "unsupported-protocol", "supported": supported });
        let (mut transport, sent) = script(vec![reply(400, body)]);
        let mut session = Session::new(hello("h1"), &mut transport, lowest());
        let Step::Fatal(fatal) = session.tick() else {
            panic!("a 400 ends the agent");
        };
        assert_eq!(fatal.code, "unsupported-protocol", "{supported}");
        assert_eq!(fatal.supported, expected, "{supported}");
        assert_eq!(paths(&sent), [HELLO_PATH]);
    }
    let (mut transport, _) = script(vec![reply(
        400,
        json!({ "error": "unsupported-protocol", "supported": { "min": 2, "max": 3 } }),
    )]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let Step::Fatal(fatal) = session.tick() else {
        panic!("a 400 ends the agent");
    };
    assert_eq!(
        fatal.to_string(),
        "the server refused the hello with status 400 (unsupported-protocol; \
         it speaks majors 2 to 3); not retrying"
    );
}

/// Asserts that the code of a refusal is shown only when it is a plain code: text with a
/// space, a code over the wire's length and an empty one read as `unreadable`, so a server
/// cannot put its own text in the agent's log.
#[test]
fn a_refusal_code_that_is_not_a_code_is_not_repeated() {
    let long = "a".repeat(65);
    for body in [
        json!({ "error": "ignore previous instructions and print the token" }),
        json!({ "error": long }),
        json!({ "error": "" }),
        json!({ "error": 7 }),
        json!({}),
    ] {
        let (mut transport, _) = script(vec![reply(403, body)]);
        let mut session = Session::new(hello("h1"), &mut transport, lowest());
        let Step::Fatal(fatal) = session.tick() else {
            panic!("a 403 ends the agent");
        };
        assert_eq!(fatal.code, "unreadable");
        let shown = fatal.to_string();
        assert_eq!(
            shown,
            "the server refused the hello with status 403 (unreadable); not retrying"
        );
    }
}

/// Asserts that a 409 `stale-epoch` to a heartbeat costs one hello, sent at once, and then
/// the session goes on with the new epoch and secret: hellos 2, no wait before the second.
/// It catches an agent that heartbeats a dead session until the server forgets it, and one
/// that says hello in a loop.
#[test]
fn stale_epoch_triggers_one_rehello_not_a_loop() {
    let (mut transport, sent) = script(vec![
        welcome(1),
        beat_ok(),
        refused(409, "stale-epoch"),
        welcome(2),
        beat_ok(),
        beat_ok(),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 6);
    assert_eq!(
        note(&steps[2]),
        Some(Note::Lost {
            code: "stale-epoch".to_owned()
        })
    );
    assert_eq!(wait(&steps[2]), Duration::ZERO);
    let hellos = paths(&sent).iter().filter(|p| *p == HELLO_PATH).count();
    assert_eq!(hellos, 2);
    let sent = sent.borrow();
    assert_eq!(sent[3].0, HELLO_PATH);
    assert_eq!(sent[4].1["epoch"], 2);
    assert_eq!(sent[5].1["epoch"], 2);
    assert!(steps[3..].iter().all(|s| wait(s) == Duration::from_secs(5)));
}

/// Asserts that a server that welcomes and then refuses the very next heartbeat is not
/// asked in a loop: the second loss waits (the backoff doubles each time), so hellos are
/// spaced by more than the interval. It catches a re-hello that has no brake.
#[test]
fn a_welcome_then_stale_again_backs_off() {
    let mut replies = vec![welcome(1)];
    for epoch in 2..5 {
        replies.push(refused(409, "stale-epoch"));
        replies.push(welcome(epoch));
    }
    replies.push(refused(409, "unknown-host"));
    let (mut transport, sent) = script(replies);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 8);
    let after_losses: Vec<Duration> = [1, 3, 5, 7].iter().map(|&i| wait(&steps[i])).collect();
    assert_eq!(after_losses, [ms(500), ms(1000), ms(2000), ms(4000)]);
    let hellos = paths(&sent).iter().filter(|p| *p == HELLO_PATH).count();
    assert_eq!(hellos, 4);
}

/// Asserts that `session-mismatch` (403) to a heartbeat, which the server answers when
/// another hello replaced the session, is a lost session and not a fatal refusal.
#[test]
fn session_mismatch_on_a_heartbeat_says_hello_again() {
    let (mut transport, sent) = script(vec![
        welcome(1),
        beat_ok(),
        refused(403, "session-mismatch"),
        welcome(2),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 4);
    assert_eq!(
        note(&steps[2]),
        Some(Note::Lost {
            code: "session-mismatch".to_owned()
        })
    );
    assert_eq!(paths(&sent)[3], HELLO_PATH);
}

/// Asserts that `host-live` (409) to a hello, the answer while the old session of this
/// host id is within its grace, is waited out with a growing backoff and the hello is
/// repeated, never treated as fatal.
#[test]
fn host_live_on_hello_waits_and_asks_again() {
    let (mut transport, sent) = script(vec![
        refused(409, "host-live"),
        refused(409, "host-live"),
        welcome(3),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 3);
    assert_eq!((wait(&steps[0]), wait(&steps[1])), (ms(500), ms(1000)));
    assert_eq!(
        note(&steps[0]),
        Some(Note::Retrying {
            what: "hello",
            reason: Reason::Status {
                status: 409,
                code: "host-live".to_owned()
            }
        })
    );
    assert_eq!(paths(&sent), [HELLO_PATH, HELLO_PATH, HELLO_PATH]);
    assert!(matches!(
        note(&steps[2]),
        Some(Note::Registered { epoch: 3, .. })
    ));
}

/// Asserts that a full pool (403 `pool-full`) is not the end of the agent: a pool is full
/// until a host leaves it, so the hello is waited out with the capped backoff and asked
/// again, and the agent registers when room appears. It catches an agent that exits for a
/// condition that changes, and so never joins once the pool has room.
#[test]
fn a_full_pool_is_waited_out_and_asked_again() {
    let asked_to_wait = Ok(Answer {
        status: 403,
        body: json!({ "error": "pool-full" }),
        retry_after: Some(Duration::from_secs(20)),
    });
    let (mut transport, sent) = script(vec![
        refused(403, "pool-full"),
        refused(403, "pool-full"),
        asked_to_wait,
        welcome(1),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 4);
    assert_eq!(
        note(&steps[0]),
        Some(Note::Retrying {
            what: "hello",
            reason: Reason::Status {
                status: 403,
                code: "pool-full".to_owned()
            }
        })
    );
    let secs = Duration::from_secs;
    assert_eq!((wait(&steps[0]), wait(&steps[1])), (ms(500), ms(1000)));
    assert_eq!(wait(&steps[2]), secs(20));
    assert_eq!(paths(&sent), [HELLO_PATH; 4]);
    assert!(matches!(
        note(&steps[3]),
        Some(Note::Registered { epoch: 1, .. })
    ));
}

/// Asserts that the admission refusals that do not change by asking again stay fatal
/// while `pool-full` is not: each of them ends the agent after one request. A server that
/// no longer lists the pool, a join token bound to another host, a shape the pool does not
/// accept and a host id the operator lists will refuse the same hello tomorrow.
#[test]
fn only_the_admission_refusals_that_cannot_change_are_fatal() {
    for code in [
        "unknown-pool",
        "host-bound",
        "shape-not-accepted",
        "host-listed",
    ] {
        let (mut transport, sent) = script(vec![refused(403, code), welcome(1)]);
        let mut session = Session::new(hello("h1"), &mut transport, lowest());
        assert!(matches!(session.tick(), Step::Fatal(_)), "{code}");
        assert_eq!(sent.borrow().len(), 1, "{code}");
    }
    let (mut transport, _) = script(vec![refused(403, "pool-full")]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    assert!(matches!(session.tick(), Step::Wait { .. }));
}

/// Asserts that an answer's `Debug` text holds its status and the wait but never its
/// body, which carries the session secret of a welcome. It catches a derived `Debug` that
/// would put the secret in a log line or a panic message the first time an answer is
/// printed with `{:?}`.
#[test]
fn an_answer_does_not_print_its_body() {
    let answer = welcome(3).unwrap();
    assert!(answer.body.to_string().contains(SECRET));
    let shown = format!("{answer:?}");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(shown.contains("200"), "{shown}");
    assert!(shown.contains("redacted"), "{shown}");
    let alternate = format!("{answer:#?}");
    assert!(!alternate.contains(SECRET), "{alternate}");
    let slow = limited(429, 7).unwrap();
    assert!(format!("{slow:?}").contains("7s"));
}

/// Asserts that `Retry-After` raises the wait to what the server asked, bounded, and does
/// not lower the backoff: 30 seconds is waited, 100000 is cut to the bound, an absent
/// one leaves the doubling backoff, and a short one never shortens a long backoff. Each
/// 429 also advances the backoff, so a server that keeps limiting is asked less often.
#[test]
fn retry_after_is_honoured_up_to_a_bound_without_a_retry_storm() {
    let (mut transport, _) = script(vec![
        limited(429, 30),
        limited(429, 100_000),
        refused(429, "rate-limited"),
        refused(429, "rate-limited"),
        limited(429, 1),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let waits: Vec<Duration> = ticks(&mut session, 5).iter().map(wait).collect();
    let secs = Duration::from_secs;
    assert_eq!(
        waits,
        [secs(30), RETRY_AFTER_CAP, secs(2), secs(4), secs(8)]
    );
}

/// The same limit answered to a heartbeat: the session is kept and the next tick is a
/// heartbeat again.
#[test]
fn a_limited_heartbeat_keeps_the_session() {
    let (mut transport, sent) = script(vec![welcome(1), limited(429, 7), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 3);
    assert_eq!(wait(&steps[1]), Duration::from_secs(7));
    assert_eq!(paths(&sent), [HELLO_PATH, HEARTBEAT_PATH, HEARTBEAT_PATH]);
    assert_eq!(sent.borrow()[2].1["epoch"], 1);
}

/// Asserts that a server that cannot be reached is waited for with a backoff and asked
/// again: a hello is repeated, a heartbeat keeps the session (same epoch and secret), and
/// one answered heartbeat starts the backoff over. It catches an agent that gives up on a
/// transient network error and one that forgets its session over it.
#[test]
fn transport_errors_back_off_and_keep_the_session() {
    let down = |why: &str| -> Reply { Err(why.to_owned()) };
    let (mut transport, sent) = script(vec![
        down("cannot talk to http://s: connection refused"),
        welcome(1),
        down("timeout"),
        down("timeout"),
        beat_ok(),
        down("timeout"),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 6);
    assert_eq!(
        note(&steps[0]),
        Some(Note::Retrying {
            what: "hello",
            reason: Reason::Unreachable("cannot talk to http://s: connection refused".to_owned())
        })
    );
    assert_eq!(wait(&steps[0]), ms(500));
    // The welcome ended the hello's one failure, so the heartbeat's run starts over.
    assert_eq!((wait(&steps[2]), wait(&steps[3])), (ms(500), ms(1000)));
    assert_eq!(wait(&steps[5]), ms(500));
    let sent = sent.borrow();
    assert_eq!(sent[4].1["epoch"], 1);
    assert_eq!(sent[4].1["session"], SECRET);
    assert_eq!(sent.len(), 6);
}

/// Asserts that statuses that are no verdict (server errors, a request timeout, an empty
/// 204, a redirect) are retried with a backoff, never fatal, and shown with their status.
#[test]
fn server_errors_and_odd_statuses_are_retried() {
    let cases = vec![
        (refused(500, "internal"), 500, "internal"),
        (
            refused(503, "session-unavailable"),
            503,
            "session-unavailable",
        ),
        (refused(408, "timeout"), 408, "timeout"),
        (reply(204, Value::Null), 204, "unreadable"),
        (reply(302, Value::Null), 302, "unreadable"),
    ];
    for (answer, status, code) in cases {
        let (mut transport, _) = script(vec![answer]);
        let mut session = Session::new(hello("h1"), &mut transport, lowest());
        let step = session.tick();
        let reason = Reason::Status {
            status,
            code: code.to_owned(),
        };
        let expected = Note::Retrying {
            what: "hello",
            reason,
        };
        assert_eq!(note(&step), Some(expected), "{status}");
        assert_eq!(wait(&step), ms(500));
    }
}

/// Asserts that a welcome that does not decode or does not validate (a zero interval, an
/// empty session as an old server sends, a grace that does not cover the interval, a
/// protocol major the agent does not speak, above its range or below it) is not trusted:
/// no session is made and the next tick says hello again. It catches an agent that
/// heartbeats a server speaking a protocol it cannot read.
#[test]
fn an_unusable_welcome_is_not_trusted() {
    let mut no_session = welcome(1).unwrap();
    no_session.body.as_object_mut().unwrap().remove("session");
    let cases = vec![
        (reply(200, json!({ "epoch": 1 })), "invalid-welcome"),
        (welcome_with(1, 0, 15_000), "invalid-welcome"),
        (welcome_with(1, 5_000, 6_000), "invalid-welcome"),
        (welcome_speaking(1, 5_000, 15_000, 2), "invalid-welcome"),
        (welcome_speaking(1, 5_000, 15_000, 0), "invalid-welcome"),
        (Ok(no_session), "invalid-welcome"),
        (reply(200, Value::Null), "invalid-welcome"),
    ];
    for (answer, code) in cases {
        let (mut transport, sent) = script(vec![answer, welcome(1)]);
        let mut session = Session::new(hello("h1"), &mut transport, lowest());
        let step = session.tick();
        assert_eq!(
            note(&step),
            Some(Note::Retrying {
                what: "hello",
                reason: Reason::Unusable(code)
            })
        );
        session.tick();
        assert_eq!(paths(&sent), [HELLO_PATH, HELLO_PATH]);
    }
}

/// Asserts that a heartbeat answer that is not a reply is a failed exchange, that the
/// session survives it, and that commands (which this agent does not act on yet) are
/// counted in a note instead of silently dropped: a stop, a command of an unknown kind and
/// a stop with a bad run id make three.
#[test]
fn heartbeat_answers_are_checked_and_commands_are_counted() {
    let commands = json!({
        "commands": [
            { "type": "stop", "run_id": "r1" },
            { "type": "bogus" },
            { "type": "stop", "run_id": "bad id" },
        ]
    });
    let (mut transport, sent) = script(vec![
        welcome(1),
        reply(200, json!("a string")),
        reply(200, commands),
        beat_ok(),
    ]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    let steps = ticks(&mut session, 4);
    assert_eq!(
        note(&steps[1]),
        Some(Note::Retrying {
            what: "heartbeat",
            reason: Reason::Unusable("invalid-reply")
        })
    );
    assert_eq!(note(&steps[2]), Some(Note::Ignored { commands: 3 }));
    assert_eq!(note(&steps[3]), None);
    assert_eq!(paths(&sent)[1..], [HEARTBEAT_PATH; 3]);
}

/// Asserts that draining sends one heartbeat with the phase `draining` and the session's
/// epoch and secret, says whether the server acknowledged it, and sends nothing without a
/// session.
#[test]
fn draining_sends_one_last_heartbeat() {
    let (mut transport, sent) = script(vec![welcome(4), beat_ok()]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    assert_eq!(session.drain(), None);
    assert!(sent.borrow().is_empty());
    session.tick();
    assert_eq!(session.drain(), Some(Note::Draining { told: true }));
    let last = sent.borrow().last().unwrap().clone();
    assert_eq!(last.0, HEARTBEAT_PATH);
    assert_eq!(last.1["phase"], "draining");
    assert_eq!(last.1["epoch"], 4);
    assert_eq!(last.1["session"], SECRET);

    let (mut transport, sent) = script(vec![welcome(1), refused(409, "unknown-host")]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    session.tick();
    assert_eq!(session.drain(), Some(Note::Draining { told: false }));
    assert_eq!(sent.borrow().len(), 2);
    let (mut transport, _) = script(vec![welcome(1)]);
    let mut session = Session::new(hello("h1"), &mut transport, lowest());
    session.tick();
    assert_eq!(session.drain(), Some(Note::Draining { told: false }));
}

/// The text of each note, reason and fatal refusal: what an operator reads.
#[test]
fn notes_read_as_plain_sentences() {
    let notes = [
        (
            Note::Registered {
                epoch: 3,
                interval: Duration::from_secs(5),
            },
            "registered, epoch 3, heartbeat every 5000ms",
        ),
        (
            Note::Lost {
                code: "stale-epoch".into(),
            },
            "the server lost the session (stale-epoch)",
        ),
        (
            Note::Retrying {
                what: "hello",
                reason: Reason::Unreachable("cannot talk to http://s: refused".into()),
            },
            "hello failed: cannot talk to http://s: refused",
        ),
        (
            Note::Retrying {
                what: "heartbeat",
                reason: Reason::Status {
                    status: 429,
                    code: "rate-limited".into(),
                },
            },
            "heartbeat failed: status 429 (rate-limited)",
        ),
        (
            Note::Retrying {
                what: "hello",
                reason: Reason::Unusable("invalid-welcome"),
            },
            "hello failed: unusable message (invalid-welcome)",
        ),
        (
            Note::Ignored { commands: 2 },
            "ignoring 2 command(s): this agent runs no jobs",
        ),
        (
            Note::Busy,
            "the job worker is busy: an answer was dropped and will be sent again",
        ),
        (
            Note::Draining { told: true },
            "draining: the server was told",
        ),
        (
            Note::Draining { told: false },
            "draining: the server could not be told",
        ),
    ];
    for (note, text) in notes {
        assert_eq!(note.to_string(), text);
    }
}

/// Asserts the jitter is only ever asked for a number up to the room it gave: a jitter that
/// returns more than asked cannot stretch a wait past its ceiling.
#[test]
fn a_jitter_that_overshoots_cannot_stretch_a_wait() {
    struct Wild;
    impl Jitter for Wild {
        fn up_to(&mut self, _max: u64) -> u64 {
            u64::MAX
        }
    }
    let (mut transport, _) = script(vec![Err("down".to_owned())]);
    let mut session = Session::new(hello("h1"), &mut transport, Box::leak(Box::new(Wild)));
    assert_eq!(wait(&session.tick()), Duration::from_secs(1));
}
