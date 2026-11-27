//! The process id a hand-over leaves: however the hand-over ends, the session's agent process id
//! is left in the state root for the next process to say hello as, so the server takes that
//! hello at once instead of refusing it `host-live`; a drain leaves none. The sleepers are the
//! ones of the handover tests: nothing waits for a time.

use std::path::Path;

use js_agent::{Delivery, Exit, Session, Wake, Worker, drive};
use serde_json::Value;

use super::{LONG, Settling, phases, running, state};
use crate::bounds::{Capped, WakeBudget};
use crate::jobs::fake::FakeMachine;
use crate::jobs::{start, starts};
use crate::support::{Reply, Scratch, Script, beat_ok, hello, lowest, welcome};

/// The id the sessions here say hello as.
const ID: &str = "0123456789abcdef0123456789abcdef";

/// Drives a session saying hello as `id` over `replies`, looking for a handover in the state
/// root of `dir`, with a sleeper that settles the worker around `act`; returns how it ended,
/// what it logged and what it sent.
fn driven_as(
    dir: &Scratch,
    worker: &Worker,
    id: Option<&str>,
    replies: Vec<Reply>,
    act: impl FnMut(usize) -> Wake,
) -> (Exit, String, Vec<(String, Value)>) {
    let (mut script, sent) = Script::new(replies);
    let root = state(dir);
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(worker, LONG)
        .with_handover(Some(&root))
        .with_agent_process(id.map(str::to_owned));
    let mut sleeper = Settling {
        handle: worker.handle(),
        calls: 0,
        act: Box::new(act),
        budget: WakeBudget::new(),
    };
    let mut log = Capped::new();
    let exit = drive(&mut session, &mut sleeper, &mut log);
    let sent = sent.borrow().clone();
    (exit, log.text(), sent)
}

/// The process file of the state root of `dir`, when there is one. Its name is the one the
/// next process, of this release or a later one, reads.
fn left(dir: &Scratch) -> Option<String> {
    std::fs::read_to_string(state(dir).join("handover-process")).ok()
}

/// Asserts that a hand-over at a stop and one in place each leave the id the session said hello
/// as in the state root, as the id and a newline, and end as a hand-over does; and that a
/// session that names no process leaves no file. Catches a hand-over whose next process draws a
/// new id, which the server refuses `host-live` until the last session's grace runs out, so the
/// host reads lost while its agent is updated.
#[test]
fn a_hand_over_leaves_the_process_id_for_the_next_process() {
    for (name, wake, ends, id) in [
        ("jobs-process-stop", Wake::Stop, Exit::Stopped, Some(ID)),
        (
            "jobs-process-place",
            Wake::Replace,
            Exit::Replaced,
            Some(ID),
        ),
        ("jobs-process-none", Wake::Replace, Exit::Replaced, None),
    ] {
        let dir = Scratch::new(name);
        let machine = FakeMachine::new();
        let worker = running(&dir, &machine, &["r1"]);
        let replies = vec![welcome(1), beat_ok()];
        let (exit, log, sent) = driven_as(&dir, &worker, id, replies, move |_| wake);

        assert_eq!(exit, ends, "{name}");
        assert_eq!(phases(&sent), ["serving", "draining"], "{name}");
        assert_eq!(
            sent[0].1.get("agent_process").and_then(Value::as_str),
            id,
            "{name}"
        );
        assert_eq!(left(&dir), id.map(|id| format!("{id}\n")), "{name}");
        assert!(!log.contains("warning"), "{name}: {log}");
        assert_eq!(machine.world.alive(), 1, "{name}");
    }
}

/// Asserts that a stop with no handover file drains and leaves no process id: the next process
/// of a drained host draws its own. Catches an id left by a stop that was no hand-over.
#[test]
fn a_drain_leaves_no_process_id() {
    let dir = Scratch::new("jobs-process-drain");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    let replies = vec![welcome(1), beat_ok(), beat_ok()];
    let (exit, log, _) = driven_as(&dir, &worker, Some(ID), replies, |_| Wake::Stop);

    assert_eq!(exit, Exit::Stopped);
    assert!(log.contains("draining: a run or a result is left"), "{log}");
    assert_eq!(left(&dir), None);
}

/// Asserts that a process file that cannot be written is named in a warning that says what it
/// costs, and that the hand-over still ends as one: nothing is stopped and the runs are left.
/// Catches a hand-over that a failed write turns into a drain or a failure, and one that fails
/// unsaid.
#[test]
fn a_process_id_that_cannot_be_left_is_named_in_a_warning() {
    let dir = Scratch::new("jobs-process-unwritable");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1"]);
    // A directory where the temporary file is written: the write fails, for root too.
    let temp: &Path = &state(&dir).join("handover-process.tmp");
    std::fs::create_dir(temp).unwrap();
    let replies = vec![welcome(1), beat_ok()];
    let (exit, log, _) = driven_as(&dir, &worker, Some(ID), replies, |_| Wake::Replace);

    assert_eq!(exit, Exit::Replaced);
    let line = log
        .lines()
        .find(|line| line.contains("warning"))
        .expect("a warning");
    assert!(
        line.starts_with(
            "js-agent: warning: the agent process id cannot be left to the next process ("
        ),
        "{line}"
    );
    assert!(
        line.ends_with("): its hello waits out the server's grace for this session"),
        "{line}"
    );
    assert!(
        log.contains("js-agent: handed over: every run is left as it is to the next process"),
        "{log}"
    );
    assert_eq!(left(&dir), None);
    assert_eq!(machine.world.alive(), 1);
}
