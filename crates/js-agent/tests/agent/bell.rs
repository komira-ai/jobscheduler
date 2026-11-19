//! What the session says of its wait ([`Session::beats_on_news`]) and how the loop around it
//! waits: a wait that may end on the bell after an accepted heartbeat, a plain sleep after
//! anything else, and a rung bell that makes the next heartbeat go at once. Waits are
//! recorded, never timed.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use js_agent::{Exit, Session, Sleeper, Step, Wake, drive};
use serde_json::json;

use crate::bounds::WakeBudget;
use crate::support::{Reply, beat_ok, hello, lowest, refused, reply, welcome};

/// Which method of the sleeper a wait asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Sleep,
    OrBell,
}

/// A sleeper that records which method each wait asked for and answers each from a list: the
/// plain sleep and the sleep that may end on the bell take the next of `answers` (the last one
/// again when it is used up).
struct Recording {
    asked: Rc<RefCell<Vec<Asked>>>,
    answers: Vec<Wake>,
    budget: WakeBudget,
}

impl Recording {
    fn answering(answers: Vec<Wake>) -> (Self, Rc<RefCell<Vec<Asked>>>) {
        let asked = Rc::default();
        let sleeper = Self {
            asked: Rc::clone(&asked),
            answers,
            budget: WakeBudget::new(),
        };
        (sleeper, asked)
    }

    fn answer(&mut self, how: Asked) -> Wake {
        self.budget.spend();
        let n = self.asked.borrow().len();
        self.asked.borrow_mut().push(how);
        let last = self.answers.len() - 1;
        self.answers[n.min(last)]
    }
}

impl Sleeper for Recording {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.answer(Asked::Sleep)
    }

    fn sleep_or_bell(&mut self, _duration: Duration) -> Wake {
        self.answer(Asked::OrBell)
    }
}

fn down() -> Reply {
    Err("connection refused".to_owned())
}

/// Proves a wait may end on the bell only after a heartbeat the server accepted: not before
/// any exchange, not after a hello, not after a failed exchange, a refusal that is not for
/// good, a refusal for good, or a lost session, and again after the next accepted heartbeat
/// (the answer is renewed by each tick). Catches a bell that cuts short a wait the session
/// asked for to back off.
#[test]
fn a_wait_may_end_on_the_bell_only_after_an_accepted_heartbeat() {
    let replies = vec![
        welcome(1),
        beat_ok(),
        down(),
        beat_ok(),
        refused(503, "unavailable"),
        beat_ok(),
        refused(409, "stale-epoch"),
        welcome(2),
        beat_ok(),
        refused(403, "host-bound"),
    ];
    let (mut script, _) = crate::support::Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest());
    assert!(!session.beats_on_news(), "before any tick");
    let expected = [false, true, false, true, false, true, false, false, true];
    for (n, news) in expected.into_iter().enumerate() {
        assert!(matches!(session.tick(), Step::Wait { .. }), "tick {n}");
        assert_eq!(session.beats_on_news(), news, "after tick {n}");
    }
    assert!(matches!(session.tick(), Step::Fatal(_)));
    assert!(!session.beats_on_news(), "after a refusal for good");
}

/// Proves a reply that carries commands to a session with no worker, which the session notes
/// and drops, still counts as an accepted heartbeat for the wait. Catches a wait that depends
/// on a worker being there.
#[test]
fn a_reply_with_commands_to_a_session_without_a_worker_is_still_accepted() {
    let commands = json!({ "commands": [{ "type": "stop", "run_id": "r1" }] });
    let replies = vec![welcome(1), reply(200, commands)];
    let (mut script, _) = crate::support::Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest());
    session.tick();
    assert!(matches!(session.tick(), Step::Wait { note: Some(_), .. }));
    assert!(session.beats_on_news());
}

/// Proves the loop asks for the wait that may end on the bell after an accepted heartbeat and
/// for a plain sleep after the hello and after a failed heartbeat. Catches a loop that lets a
/// bell cut a backoff short, or one that never lets the bell end the wait it may.
#[test]
fn the_loop_waits_on_the_bell_after_an_accepted_heartbeat_and_not_otherwise() {
    let replies = vec![welcome(1), beat_ok(), down(), beat_ok(), beat_ok()];
    let (mut script, _) = crate::support::Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest());
    let (mut sleeper, asked) = Recording::answering(vec![
        Wake::Elapsed,
        Wake::Elapsed,
        Wake::Elapsed,
        Wake::Stop,
    ]);
    let mut log = Vec::new();
    let exit = drive(&mut session, &mut sleeper, &mut log);
    assert_eq!(exit, Exit::Stopped);
    assert_eq!(
        *asked.borrow(),
        [Asked::Sleep, Asked::OrBell, Asked::Sleep, Asked::OrBell]
    );
}

/// Proves a wait that the bell ended is not a stop: the next tick sends a heartbeat at once,
/// the loop goes on, and a stop at the wait after it ends the loop with the drain's heartbeat
/// as the last message. Catches a `Bell` taken for a stop, and one that is taken for the
/// time having passed with the wait still running.
#[test]
fn a_bell_ends_the_wait_and_the_next_heartbeat_goes_at_once() {
    let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok()];
    let (mut script, sent) = crate::support::Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest());
    let (mut sleeper, asked) = Recording::answering(vec![Wake::Elapsed, Wake::Bell, Wake::Stop]);
    let mut log = Vec::new();
    let exit = drive(&mut session, &mut sleeper, &mut log);
    assert_eq!(exit, Exit::Stopped, "{}", String::from_utf8_lossy(&log));
    assert_eq!(
        *asked.borrow(),
        [Asked::Sleep, Asked::OrBell, Asked::OrBell]
    );
    let phases: Vec<String> = sent
        .borrow()
        .iter()
        .skip(1)
        .map(|(_, body)| body["phase"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(phases, ["serving", "serving", "draining"]);
}

/// A sleeper that has only the plain sleep.
struct Plain(Vec<Duration>);

impl Sleeper for Plain {
    fn sleep(&mut self, duration: Duration) -> Wake {
        self.0.push(duration);
        Wake::Elapsed
    }
}

/// Proves a sleeper that defines only `sleep` has no bell, and that its wait that may end on
/// the bell is the plain sleep. Catches a default that waits another way or forgets the wait.
#[test]
fn a_sleeper_with_only_a_sleep_has_no_bell_and_sleeps_for_the_bell_wait() {
    let mut plain = Plain(Vec::new());
    assert!(plain.bell().is_none());
    assert_eq!(plain.sleep_or_bell(Duration::from_secs(3)), Wake::Elapsed);
    assert_eq!(plain.0, [Duration::from_secs(3)]);
}
