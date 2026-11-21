//! The handover in place: a replace request (SIGHUP, which the loop reads as [`Wake::Replace`])
//! that finds the handover file hands the runs over as a stop does and ends the loop with
//! `Exit::Replaced`, so the program runs again in the same process. The sleepers are the ones of
//! the handover tests: the worker is settled around each wake, and nothing waits for a time.

use std::time::Duration;

use js_agent::lifecycle::handover;
use js_agent::{Delivery, Drain, Exit, Wake};
use js_agent_wire::HeldState;

use super::{LONG, handed, next_process, phases, running, state};
use crate::jobs::fake::FakeMachine;
use crate::jobs::{job, start, starts};
use crate::support::{Scratch, beat_ok, welcome};

/// Asserts that a replace request that finds the handover file hands the runs over as a stop
/// does: the agent says `draining` once, stops nothing, ends with `Exit::Replaced`, and the
/// next process adopts both runs as running without starting them again. It catches a replace
/// that drains or stops a job, one that ends as a plain stop (the program would then not run
/// again), and a next process that loses a run.
#[test]
fn a_replace_asked_to_hand_over_leaves_the_runs_to_the_next_process_and_ends_replaced() {
    let dir = Scratch::new("jobs-in-place-busy");
    let machine = FakeMachine::new();
    let worker = running(&dir, &machine, &["r1", "r2"]);
    let replies = vec![welcome(1), beat_ok()];
    let (exit, log, sent) = handed(&dir, &worker, replies, LONG, |_| Wake::Replace);

    assert_eq!(exit, Exit::Replaced);
    assert_eq!(phases(&sent), ["serving", "draining"]);
    assert_eq!(sent[1].1["held"][0]["state"], "running");
    assert_eq!(sent[1].1["held"][1]["state"], "running");
    assert_eq!(machine.world.alive(), 2);
    assert!(
        !machine
            .world
            .log()
            .iter()
            .any(|call| call.starts_with("stop ")),
        "{:?}",
        machine.world.log()
    );
    assert!(
        log.contains("js-agent: handed over: every run is left as it is to the next process"),
        "{log}"
    );

    let next = next_process(&dir, &machine, worker);
    let held = next.snapshot().held;
    let states: Vec<(&str, HeldState)> = held
        .iter()
        .map(|run| (run.run_id.as_str(), run.state))
        .collect();
    assert_eq!(
        states,
        [("r1", HeldState::Running), ("r2", HeldState::Running)]
    );
    assert_eq!(
        machine.world.created(),
        ["r1", "r2"],
        "nothing is started twice"
    );
}

/// Asserts that a replace request with no usable handover file is ignored: the log says why,
/// the agent sends another heartbeat as `serving` and goes on, and a stop after it still
/// drains and ends `Exit::Stopped` with the run left. It catches a replace that hands over
/// without the request, one that ends the loop, and one that stops serving.
#[test]
fn a_replace_without_a_usable_request_is_ignored_and_a_later_stop_drains() {
    for (name, open, said) in [
        (
            "jobs-in-place-none",
            false,
            "js-agent: a replace request is ignored: there is no handover file",
        ),
        (
            "jobs-in-place-open",
            true,
            "js-agent: a replace request is ignored: the handover file is not used (group or \
             others have permission on it (mode 0644))",
        ),
    ] {
        let dir = Scratch::new(name);
        let machine = FakeMachine::new();
        let worker = start(&dir, &machine);
        assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
        assert!(worker.handle().settle());
        if open {
            use std::os::unix::fs::PermissionsExt;
            handover::write(&state(&dir)).unwrap();
            let file = state(&dir).join(handover::HANDOVER_FILE);
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok()];
        let act = |n: usize| match n {
            0 => Wake::Replace,
            _ => Wake::Stop,
        };
        let (exit, log, sent) = handed(&dir, &worker, replies, LONG, act);

        assert_eq!(exit, Exit::Stopped, "{name}");
        assert_eq!(phases(&sent), ["serving", "serving", "draining"], "{name}");
        assert!(log.contains(said), "{name}: {log}");
        assert!(
            log.contains("draining: a run or a result is left"),
            "{name}: {log}"
        );
        assert!(!log.contains("handing over"), "{name}: {log}");
        assert_eq!(machine.world.alive(), 1, "{name}");
    }
}

/// Asserts that a replace request during a drain is ignored: the drain goes on, with one more
/// heartbeat, and ends at the stop request after it. It catches a drain that a replace request
/// ends, or turns into a handover.
#[test]
fn a_replace_during_a_drain_is_ignored() {
    let dir = Scratch::new("jobs-in-place-drain");
    let machine = FakeMachine::new();
    let worker = start(&dir, &machine);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    // A request that is there when the stop comes would hand over: the stop finds none.
    let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok()];
    let act = |n: usize| match n {
        0 | 2 => Wake::Stop,
        _ => Wake::Replace,
    };
    let (exit, log, sent) = handed(&dir, &worker, replies, LONG, act);

    assert_eq!(exit, Exit::Stopped);
    assert_eq!(phases(&sent), ["serving", "draining", "draining"]);
    assert!(!log.contains("replace request"), "{log}");
    assert_eq!(machine.world.alive(), 1);
}

/// Waits a start under way: a first run and a second whose image pull does not end, and
/// the replies a hand-over that has to wait needs.
fn held_by_a_pull(
    dir: &Scratch,
    machine: &FakeMachine,
) -> (js_agent::Worker, String, Vec<crate::support::Reply>) {
    let pulled = job("r2").image;
    let worker = running(dir, machine, &["r1"]);
    machine.world.begin_pull(&pulled);
    assert_eq!(worker.try_deliver(starts(&["r2"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    let replies = vec![welcome(1), beat_ok(), beat_ok(), beat_ok(), beat_ok()];
    (worker, pulled, replies)
}

/// Asserts that a stop request during an in-place hand-over that waits for a start ends it
/// `Exit::Stopped`: a stop wins, the process exits and nothing runs again; the log says what
/// was left, and nothing was stopped. It catches a hand-over in place that a stop does not
/// end, or one that ends replaced after it.
#[test]
fn a_stop_during_a_handover_in_place_ends_stopped() {
    let dir = Scratch::new("jobs-in-place-stop");
    let machine = FakeMachine::new();
    let (worker, _, replies) = held_by_a_pull(&dir, &machine);
    let act = |n: usize| match n {
        0 => Wake::Replace,
        _ => Wake::Stop,
    };
    let (exit, log, sent) = handed(&dir, &worker, replies, LONG, act);

    assert_eq!(exit, Exit::Stopped);
    assert_eq!(phases(&sent), ["serving", "draining"]);
    assert!(
        log.contains(
            "js-agent: handing over: a start, a stop or a check is still under way and is left \
             to the next process"
        ),
        "{log}"
    );
    assert!(!log.contains("js-agent: handed over"), "{log}");
    assert_eq!(machine.world.alive(), 1);
}

/// Asserts that a second replace request during an in-place hand-over is ignored: the
/// hand-over goes on past it, with one more heartbeat, and ends `Exit::Replaced` once the
/// start it waited for has finished. It catches a second request that restarts or ends the
/// hand-over.
#[test]
fn a_second_replace_during_a_handover_in_place_is_ignored() {
    let dir = Scratch::new("jobs-in-place-twice");
    let machine = FakeMachine::new();
    let (worker, pulled, replies) = held_by_a_pull(&dir, &machine);
    let world = machine.world.clone();
    let act = move |n: usize| {
        if n == 1 {
            world.end_pull(&pulled, true);
        }
        Wake::Replace
    };
    let (exit, log, sent) = handed(&dir, &worker, replies, LONG, act);

    assert_eq!(exit, Exit::Replaced);
    assert_eq!(phases(&sent), ["serving", "draining", "draining"]);
    assert!(
        log.contains("js-agent: handed over: every run is left as it is to the next process"),
        "{log}"
    );
    let _ = next_process(&dir, &machine, worker);
    assert_eq!(machine.world.created(), ["r1", "r2"]);
}

/// Asserts that an in-place hand-over that the work under way does not let finish ends
/// `Exit::Replaced` once the drain wait has passed, says that what was under way is left, and
/// stops nothing. It catches a hand-over in place that waits for ever, or ends as a stop.
#[test]
fn a_handover_in_place_held_past_the_drain_wait_ends_replaced() {
    let dir = Scratch::new("jobs-in-place-bound");
    let machine = FakeMachine::new();
    let (worker, _, replies) = held_by_a_pull(&dir, &machine);
    let drain = Drain {
        wait: Duration::from_secs(10),
        settle: Duration::from_secs(30),
    };
    let act = |n: usize| match n {
        0 => Wake::Replace,
        _ => Wake::Elapsed,
    };
    let (exit, log, _) = handed(&dir, &worker, replies, drain, act);

    assert_eq!(exit, Exit::Replaced);
    assert!(
        log.contains(
            "js-agent: handing over: a start, a stop or a check is still under way and is left \
             to the next process"
        ),
        "{log}"
    );
    assert_eq!(machine.world.alive(), 1);
    assert!(
        !machine
            .world
            .log()
            .iter()
            .any(|call| call.starts_with("stop ")),
        "{:?}",
        machine.world.log()
    );
}

/// Writes the handover file at the first wait, as the update hook does while the agent runs,
/// and reports a replace request there; any later wait is a failure of the loop.
struct Replacing {
    root: std::path::PathBuf,
    waits: usize,
}

impl js_agent::Sleeper for Replacing {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.waits += 1;
        handover::write(&self.root).unwrap();
        Wake::Replace
    }
}

/// [`Replacing`], with a stop request that comes between the end of the loop and the exec.
struct ReplacingThenStopped(Replacing);

impl js_agent::Sleeper for ReplacingThenStopped {
    fn sleep(&mut self, duration: Duration) -> Wake {
        self.0.sleep(duration)
    }

    fn ready_to_run_again(&mut self) -> bool {
        false
    }
}

/// Asserts the whole agent's end after a handover in place: the program is run again with the
/// arguments it was started with, `--flags-file` unexpanded, once the run has ended; and an
/// exec that returns an error ends it with status 1 and the line that says the runs are left
/// to the next process. It catches an agent that exits as after a stop (no run again), one that
/// runs the expanded arguments (the next program would not read the file again), and one that
/// ends with status 0 when it could not run again.
#[test]
fn the_agent_runs_its_command_line_again_in_place_and_ends_with_status_one_when_that_fails() {
    use std::cell::RefCell;
    use std::sync::Arc;

    use crate::jobs::drain::container_flags;
    use crate::jobs::private;
    use crate::support::{Jobs, Script, ScriptConnector, code, flags, linux_host, run_in_place};

    let dir = Scratch::new("jobs-in-place-exec");
    let owned = container_flags(&dir);
    let mut lines = Vec::new();
    let mut words = owned.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--allow-container-engine" => lines.push(word.clone()),
            _ => lines.push(format!("{word}={}", words.next().unwrap())),
        }
    }
    let file = dir.write("flags", &format!("{}\n", lines.join("\n")), 0o600);
    let file = file.display().to_string();
    let args = flags(&["--flags-file", &file]);
    let (script, _) = Script::new(vec![welcome(1), beat_ok()]);
    let mut sleeper = Replacing {
        root: private(&dir, "state"),
        waits: 0,
    };
    let machine = Arc::new(FakeMachine::new());
    let jobs: Jobs<'_> = (machine, None);
    let given = RefCell::new(Vec::new());
    let again = |words: &[std::ffi::OsString]| {
        given.borrow_mut().push(words.to_vec());
        std::io::Error::other("stand-in")
    };
    let ran = run_in_place(
        &args,
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        jobs,
        &again,
    );

    assert_eq!(ran.status(), code(1), "{}", ran.err);
    assert_eq!(sleeper.waits, 1);
    let mut expected: Vec<std::ffi::OsString> = vec!["js-agent".into()];
    expected.extend(args.iter().map(std::ffi::OsString::from));
    assert_eq!(*given.borrow(), [expected]);
    let handed = ran
        .err
        .find("js-agent: handed over: every run is left")
        .unwrap();
    let again_line = ran
        .err
        .find("js-agent: handed over in place: running js-agent again\n")
        .unwrap();
    assert!(handed < again_line, "{}", ran.err);
    assert!(
        ran.err.ends_with(
            "js-agent: cannot run js-agent again in place: stand-in; the runs are left as they \
             are for the agent the unit starts next\n"
        ),
        "{}",
        ran.err
    );
}

/// Asserts that a stop request that comes after the loop ended for a handover in place, and
/// before the exec, wins: the program is not run again and the agent ends with status 0, its
/// runs handed over. It catches an agent that runs its program again over a stop, which the new
/// program never sees, so that systemd's stop waits out its whole timeout.
#[test]
fn a_stop_after_the_handover_in_place_and_before_the_exec_ends_the_agent() {
    use std::cell::Cell;
    use std::sync::Arc;

    use crate::jobs::drain::container_flags;
    use crate::jobs::private;
    use crate::support::{Jobs, Script, ScriptConnector, code, flags, linux_host, run_in_place};

    let dir = Scratch::new("jobs-in-place-stop-before-exec");
    let owned = container_flags(&dir);
    let words: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args = flags(&words);
    let (script, _) = Script::new(vec![welcome(1), beat_ok()]);
    let mut sleeper = ReplacingThenStopped(Replacing {
        root: private(&dir, "state"),
        waits: 0,
    });
    let jobs: Jobs<'_> = (Arc::new(FakeMachine::new()), None);
    let ran_again = Cell::new(false);
    let again = |_: &[std::ffi::OsString]| {
        ran_again.set(true);
        std::io::Error::other("stand-in")
    };
    let ran = run_in_place(
        &args,
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        jobs,
        &again,
    );

    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!ran_again.get());
    assert!(
        ran.err.contains("js-agent: handed over: every run is left"),
        "{}",
        ran.err
    );
    assert!(!ran.err.contains("running js-agent again"), "{}", ran.err);
}
