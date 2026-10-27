//! `lease run` and the process group of its command: what the command leaves running is
//! ended before the lease is released, however the run ends.
//!
//! The commands here start a process that would leave a mark after a while (twenty
//! seconds) if nothing ended it, and the tests learn that it is gone from the system
//! (a handle on the process says when it ends), never from a clock: a test whose
//! process is not ended fails on the mark, not on a hang.

use std::fs;
use std::sync::{Arc, Mutex};
use std::thread::scope;
use std::time::Duration;

use js_cli::{ForwardedSignal, Signaller, Waiter};

use crate::procs::{fifo, path_str, secs, wait_gone, watch_pid};
use crate::support::{Harness, Manual};

const POLL: Duration = Duration::from_millis(100);

/// Asserts that a process the command leaves running when it exits (here a shell with
/// its own `sleep`, in the command's group) is ended before `js` returns, and the lease
/// is released after that: the command's status (0) is `js`'s, and the mark the process
/// would have left after twenty seconds is not there when it is gone.
#[test]
fn what_the_command_leaves_running_is_ended_before_js_returns() {
    let h = Harness::new("group_left");
    let (ready, go) = (fifo(&h.scratch, "ready"), fifo(&h.scratch, "go"));
    let mark = h.scratch.path("survived");
    let script =
        r#"sh -c 'sleep 20; echo survived > "$0"' "$1" & echo $! > "$0"; read line < "$2""#;
    let words = crate::support::run_words(
        &[],
        &[
            "sh",
            "-c",
            script,
            path_str(&ready),
            path_str(&mark),
            path_str(&go),
        ],
    );
    let (got, watch) = scope(|s| {
        let run = s.spawn(|| h.js.waiting(&words, Manual::new()));
        let watch = watch_pid(&fs::read_to_string(&ready).unwrap());
        fs::write(&go, "done\n").unwrap();
        (run.join().unwrap(), watch)
    });
    wait_gone(&watch);
    assert_eq!(got.code, 0, "{}", got.err);
    assert!(!mark.exists());
    assert!(h.listed().is_empty());
}

/// Asserts that a process of the command's group that ignores SIGTERM, left running when
/// the lease is lost and the command (which stops on SIGTERM) has ended, is killed after
/// the grace period: the sweep asked the waiter for it in steps of 100ms, fifty of them
/// for `--stop-grace 5s`, `js` exits 7, and the process is gone with no mark left.
#[test]
fn a_survivor_of_a_lost_lease_is_killed_after_the_grace_period() {
    let h = Harness::new("group_lost");
    let waiter = Manual::new();
    let (services, _signals) = h.js.services_with_signals(waiter.clone());
    let (ready, mark) = (fifo(&h.scratch, "ready"), h.scratch.path("survived"));
    let script = r#"trap 'exit 0' TERM; sh -c 'trap "" TERM; echo $$ > "$0"; sleep 20; echo survived > "$1"' "$0" "$1" & wait"#;
    let words = crate::support::run_words(
        &["--stop-grace", "5s"],
        &["sh", "-c", script, path_str(&ready), path_str(&mark)],
    );
    let (got, watch) = scope(|s| {
        let run = s.spawn(|| h.js.with(&words, services));
        let watch = watch_pid(&fs::read_to_string(&ready).unwrap());
        h.advance(secs(601));
        h.tick();
        waiter.release();
        (run.join().unwrap(), watch)
    });
    wait_gone(&watch);
    assert_eq!(got.code, 7, "{}", got.err);
    assert!(!mark.exists());
    assert_eq!(waiter.instant(), vec![POLL; 50]);
}

/// A waiter that delivers a signal to the run at every look at the group (an interval of
/// 100ms) and leaves every other wait to the test.
struct SignalAtPoll {
    slot: Arc<Mutex<Option<Signaller>>>,
    manual: Arc<Manual>,
}

impl Waiter for SignalAtPoll {
    fn wait(&self, interval: Duration) {
        if interval == POLL {
            let slot = self.slot.lock().unwrap();
            slot.as_ref().unwrap().deliver(ForwardedSignal::Terminate);
        }
        self.manual.wait(interval);
    }
}

/// Asserts that a signal sent to `js` while it waits for what the command left running
/// to end ends the wait: the survivor, which ignores SIGTERM, is killed after one look
/// at the group instead of the thirty seconds of the grace period, and the command's
/// status is `js`'s.
#[test]
fn a_signal_ends_the_wait_for_what_the_command_left_running() {
    let h = Harness::new("group_signalled");
    let slot = Arc::new(Mutex::new(None));
    let manual = Manual::new();
    let waiter = Arc::new(SignalAtPoll {
        slot: slot.clone(),
        manual: manual.clone(),
    });
    let source = {
        let slot = slot.clone();
        Box::new(move |signaller| {
            *slot.lock().unwrap() = Some(signaller);
            Ok(())
        })
    };
    let services = js_cli::Services::new(waiter, h.js.clock.clone(), source);
    let (ready, go) = (fifo(&h.scratch, "ready"), fifo(&h.scratch, "go"));
    let mark = h.scratch.path("survived");
    let script = r#"sh -c 'trap "" TERM; echo $$ > "$0"; sleep 20; echo survived > "$1"' "$0" "$1" & read line < "$2""#;
    let words = crate::support::run_words(
        &[],
        &[
            "sh",
            "-c",
            script,
            path_str(&ready),
            path_str(&mark),
            path_str(&go),
        ],
    );
    let (got, watch) = scope(|s| {
        let run = s.spawn(|| h.js.with(&words, services));
        let watch = watch_pid(&fs::read_to_string(&ready).unwrap());
        fs::write(&go, "done\n").unwrap();
        (run.join().unwrap(), watch)
    });
    wait_gone(&watch);
    assert_eq!(got.code, 0, "{}", got.err);
    assert!(!mark.exists());
    assert_eq!(manual.instant(), vec![POLL]);
}
