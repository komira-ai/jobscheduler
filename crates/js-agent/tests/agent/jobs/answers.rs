//! The rules of the worker's whole-node answers, driven over a lifecycle of the test's own
//! so that orders arrive in exactly the order a test says, which a real worker only does by
//! chance: a repeat of an answered question, a lease that ends and is granted again, a lag
//! behind a switch of lease, and a lease taken over after a restart.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::lifecycle::{Quiet, Restored};
use js_agent::{HookRunner, HookWindow, LeaseAnswers, Ran, ReadinessHook, WorkerHook};
use js_agent_wire::{HeartbeatReply, JobSpec};

use serde_json::json;

use crate::lifecycle::{Harness, job, reply_json};

const LEASE: &str = "whole-1";
const OTHER: &str = "whole-2";

/// A hook runner that exits 0 and counts its runs.
struct Runs(Arc<AtomicUsize>);

impl HookRunner for Runs {
    fn execute(&self, _program: &str, _args: &[&str], _env: &[(&str, &str)]) -> Ran {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ran::Exited {
            code: Some(0),
            text: "exit status: 0".to_owned(),
            stdout: String::new(),
        }
    }
}

/// A worker hook that counts its runs in the returned counter.
fn counting() -> (WorkerHook, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let program = ReadinessHook::new("/bin/sh", &[]).unwrap();
    let bound = Duration::from_secs(1);
    let hook = WorkerHook::new(program, Box::new(Runs(Arc::clone(&runs))), bound);
    (hook, runs)
}

/// Told of each hook run (the worker moves its watchdog's limit by it).
fn at_hook(_window: HookWindow) {}

/// The reply that names `lease`, or none.
fn naming(h: &mut Harness, lease: Option<&str>) {
    let reply = HeartbeatReply {
        exclusive: lease.map(str::to_owned),
        ..HeartbeatReply::default()
    };
    h.life.apply(&reply);
}

/// How many times the post-job checks ran for the restore of a lease.
fn restore_checks(h: &Harness) -> usize {
    let log = h.log.borrow();
    log.iter()
        .filter(|line| line.starts_with("check restore "))
        .count()
}

/// Asserts a restore that is `Done` for a lease stays `Done` when a second restore order
/// for it is taken after a job was started: the second order runs no check (the count of
/// the checks stays one) and the published answer is not turned into `Busy`, which would
/// keep the session in `restoring` until the job ends. It catches a repeat that overwrites
/// the answer the session has not read yet.
#[test]
fn a_second_restore_after_done_leaves_done() {
    let mut h = Harness::new("answers-restore-twice");
    let mut answers = LeaseAnswers::default();
    let mut events = Vec::new();
    naming(&mut h, None);
    answers.restore(&mut h.life, LEASE.to_owned(), &mut events);
    assert_eq!(
        answers.restored(),
        Some(&(LEASE.to_owned(), Restored::Done))
    );

    h.start(job("a"));
    answers.restore(&mut h.life, LEASE.to_owned(), &mut events);
    assert_eq!(
        answers.restored(),
        Some(&(LEASE.to_owned(), Restored::Done))
    );
    assert_eq!(restore_checks(&h), 1);
}

/// Asserts the hook does not run for a lease taken over after a restart: runs of the
/// lease were found when the agent started, so the lifecycle says ready at once, and the
/// answer is ready with the hook run zero times. It catches a hook that refuses a host the
/// consumer is using, which would end a live lease.
#[test]
fn the_hook_does_not_run_for_a_taken_over_lease() {
    let mut h = Harness::new("answers-adopted");
    let own = JobSpec {
        lease_id: LEASE.to_owned(),
        ..job("a")
    };
    h.start(own);
    h.restart().unwrap();
    naming(&mut h, Some(LEASE));
    let (hook, runs) = counting();
    let mut answers = LeaseAnswers::default();
    answers.prepare(
        &mut h.life,
        Some(&hook),
        LEASE.to_owned(),
        &mut Vec::new(),
        &at_hook,
    );

    assert_eq!(answers.quiet(), Some(&(LEASE.to_owned(), Quiet::Ready)));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

/// Asserts the other half of the adoption rule: the agent restarts with a run of `LEASE`
/// adopted, and the first reply that names a lease names `OTHER`, a fresh one. `OTHER` is not
/// taken over, so it is prepared like any grant: busy while the adopted run is held, and,
/// once the run has ended and its result is acknowledged, ready only after the hook has run
/// (once). It catches an adoption signal that is "some lease was adopted" instead of "this
/// lease was adopted", which would skip the hook for a fresh lease and report it ready
/// without measuring the host.
#[test]
fn the_hook_runs_for_a_fresh_lease_after_a_restart_that_adopted_another() {
    let mut h = Harness::new("answers-adopted-other");
    let own = JobSpec {
        lease_id: LEASE.to_owned(),
        ..job("a")
    };
    h.start(own);
    h.restart().unwrap();
    naming(&mut h, Some(OTHER));
    let (hook, runs) = counting();
    let mut answers = LeaseAnswers::default();
    let mut events = Vec::new();

    answers.prepare(
        &mut h.life,
        Some(&hook),
        OTHER.to_owned(),
        &mut events,
        &at_hook,
    );
    assert_eq!(answers.quiet(), Some(&(OTHER.to_owned(), Quiet::Busy)));
    assert_eq!(runs.load(Ordering::SeqCst), 0, "the adopted run is held");

    h.fake.exit("a", 0);
    h.life.step();
    // Acknowledged under a reply that still names `OTHER`: a reply naming no lease would end
    // the hold, and the prepare would then answer for a lease no longer followed.
    h.life.apply(&reply_json(&json!({
        "exclusive": OTHER,
        "result_acks": [{ "run_id": "a", "ack": "ACCEPTED" }]
    })));
    answers.prepare(
        &mut h.life,
        Some(&hook),
        OTHER.to_owned(),
        &mut events,
        &at_hook,
    );
    assert_eq!(answers.quiet(), Some(&(OTHER.to_owned(), Quiet::Ready)));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the fresh lease was vetted");
}

/// A hook runner that exits with a code and notes `run` in a log shared with the watchdog
/// seam.
struct Noting(Arc<Mutex<Vec<String>>>, i32);

impl HookRunner for Noting {
    fn execute(&self, _program: &str, _args: &[&str], _env: &[(&str, &str)]) -> Ran {
        self.0.lock().unwrap().push("run".to_owned());
        Ran::Exited {
            code: Some(self.1),
            text: format!("exit status: {}", self.1),
            stdout: String::new(),
        }
    }
}

/// What the watchdog seam heard, and the hook's runs, in order, while `LEASE` is prepared
/// on `h` with a hook that exits with `code`.
fn watched(h: &mut Harness, code: i32) -> (Quiet, Vec<String>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let program = ReadinessHook::new("/bin/sh", &[]).unwrap();
    let bound = Duration::from_secs(1);
    let hook = WorkerHook::new(program, Box::new(Noting(Arc::clone(&log), code)), bound);
    let heard = Arc::clone(&log);
    let watch = move |window: HookWindow| heard.lock().unwrap().push(format!("{window:?}"));
    let mut answers = LeaseAnswers::default();
    answers.prepare(
        &mut h.life,
        Some(&hook),
        LEASE.to_owned(),
        &mut Vec::new(),
        &watch,
    );
    let (_, quiet) = answers.quiet().cloned().unwrap();
    let heard = log.lock().unwrap().clone();
    (quiet, heard)
}

/// Asserts the watchdog is told of the hook's run, and of nothing else: for a ready host it
/// hears the window open with the hook's bound, the run, and the same window close; a hook
/// that refuses closes the window as well; a prepare answered busy (a run held) or ready by
/// adoption (the hook is skipped) hears nothing. It catches an allowance that is never taken
/// back (no `Closes`), one given to a prepare that does not reach the hook, and one that is
/// not given before the hook.
#[test]
fn the_watchdog_hears_of_the_hook_run_and_of_nothing_else() {
    let mut h = Harness::new("answers-window");
    naming(&mut h, Some(LEASE));
    let (quiet, heard) = watched(&mut h, 0);
    assert_eq!(quiet, Quiet::Ready);
    assert_eq!(heard, ["Opens(1s)", "run", "Closes(1s)"]);

    let mut h = Harness::new("answers-window-refused");
    naming(&mut h, Some(LEASE));
    let (quiet, heard) = watched(&mut h, js_agent::EXIT_NOT_QUIET);
    assert!(matches!(quiet, Quiet::Refused { .. }));
    assert_eq!(heard, ["Opens(1s)", "run", "Closes(1s)"]);

    let mut h = Harness::new("answers-window-busy");
    h.start(job("a"));
    naming(&mut h, Some(LEASE));
    let (quiet, heard) = watched(&mut h, 0);
    assert_eq!(quiet, Quiet::Busy);
    assert_eq!(heard, Vec::<String>::new(), "a run is held: no hook");

    let mut h = Harness::new("answers-window-adopted");
    h.start(JobSpec {
        lease_id: LEASE.to_owned(),
        ..job("a")
    });
    h.restart().unwrap();
    naming(&mut h, Some(LEASE));
    let (quiet, heard) = watched(&mut h, 0);
    assert_eq!(quiet, Quiet::Ready);
    assert_eq!(heard, Vec::<String>::new(), "taken over: no hook");
}

/// Asserts the limit of the watchdog around a hook run that outlasts the plain stall bound,
/// with the numbers of the real agent (milliseconds): the order began at 0 with a plain
/// limit of 661_001, its checks took 1_000, the hook's bound is 1_200_000 and the hook took
/// all of it. Opening gives the bound; closing gives the plain limit moved later by the
/// time the hook really took (1_861_001), which is not in the past at the hook's end
/// (1_201_000), so a worker that goes on to step and publish is not called stalled. Also:
/// a clock that did not move takes nothing, and the sums do not wrap.
#[test]
fn the_limit_after_the_hook_is_the_plain_limit_moved_by_the_hooks_own_time() {
    let bound = Duration::from_millis(1_200_000);
    let (plain, opened, ended) = (661_001, 1_000, 1_201_000);

    let during = HookWindow::Opens(bound).moves(plain, plain, opened, opened);
    assert_eq!(during, plain + 1_200_000);

    let after = HookWindow::Closes(bound).moves(during, plain, opened, ended);
    assert_eq!(after, 1_861_001);
    assert!(ended < after, "the worker is not stalled at the hook's end");

    let quick = HookWindow::Closes(bound).moves(during, plain, opened, opened);
    assert_eq!(
        quick, plain,
        "a hook that took no time leaves the plain limit"
    );
    let back = HookWindow::Closes(bound).moves(during, plain, ended, opened);
    assert_eq!(back, plain, "a clock that went back takes nothing");
    let huge = HookWindow::Opens(bound).moves(u64::MAX, plain, opened, opened);
    assert_eq!(huge, u64::MAX);
    let edge = HookWindow::Closes(bound).moves(during, u64::MAX, opened, ended);
    assert_eq!(edge, u64::MAX);
}

/// Asserts that a lease that ends and is granted again is prepared afresh: the ready
/// of the first grant is dropped when the lease ends, so the second grant runs the checks
/// and the hook again (two runs), and the restored answer of the first end is dropped when
/// the lease is granted again, so the second end is cleaned up afresh (two sets of
/// checks). It catches a stale answer that skips the checks or the hook for a lease whose
/// id came back.
#[test]
fn a_lease_granted_again_is_prepared_and_restored_afresh() {
    let mut h = Harness::new("answers-regrant");
    let (hook, runs) = counting();
    let mut answers = LeaseAnswers::default();
    let mut events = Vec::new();

    naming(&mut h, Some(LEASE));
    answers.prepare(
        &mut h.life,
        Some(&hook),
        LEASE.to_owned(),
        &mut events,
        &at_hook,
    );
    assert_eq!(answers.quiet(), Some(&(LEASE.to_owned(), Quiet::Ready)));
    naming(&mut h, None);
    answers.follow(h.life.exclusive());
    assert_eq!(answers.quiet(), None, "dropped when the lease ended");
    answers.restore(&mut h.life, LEASE.to_owned(), &mut events);
    assert_eq!(restore_checks(&h), 1);

    naming(&mut h, Some(LEASE));
    answers.follow(h.life.exclusive());
    assert_eq!(answers.restored(), None, "dropped when a lease is followed");
    answers.prepare(
        &mut h.life,
        Some(&hook),
        LEASE.to_owned(),
        &mut events,
        &at_hook,
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "the hook ran for each grant"
    );
    naming(&mut h, None);
    answers.follow(h.life.exclusive());
    answers.restore(&mut h.life, LEASE.to_owned(), &mut events);
    assert_eq!(restore_checks(&h), 2);
}

/// Asserts that a question that lags behind a switch of lease is answered busy and the
/// answer is dropped: a prepare for the old lease and a restore for the old lease, taken
/// after the lifecycle follows the new one, publish nothing the session could read. It
/// catches an answer for a lease that has been replaced.
#[test]
fn a_question_that_lags_behind_a_switch_of_lease_is_dropped() {
    let mut h = Harness::new("answers-lag");
    let mut answers = LeaseAnswers::default();
    let mut events = Vec::new();
    naming(&mut h, Some(OTHER));

    answers.prepare(&mut h.life, None, LEASE.to_owned(), &mut events, &at_hook);
    assert_eq!(answers.quiet(), Some(&(LEASE.to_owned(), Quiet::Busy)));
    answers.restore(&mut h.life, LEASE.to_owned(), &mut events);
    assert_eq!(
        answers.restored(),
        Some(&(LEASE.to_owned(), Restored::Busy))
    );
    answers.follow(h.life.exclusive());
    assert_eq!(answers.quiet(), None);
    assert_eq!(answers.restored(), None);
}
