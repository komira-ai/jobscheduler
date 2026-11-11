//! The helper's table of runs: `watch`, `stop` and a run's own deadline, on the fake host's
//! clock, which a test moves itself.

use std::time::Duration;

use crate::helper::tests::{Rig, fds, rig};
use crate::helper::{NO_RUN, Outcome};
use crate::lease::parse_lease;
use crate::proto::{Reply, Request, Session};
use crate::testing::act_argv;

/// The pid the table names for runs that no process stands for.
const PID: u32 = 4242;
/// A grace or a deadline no test waits out on the real clock.
const HOUR: Duration = Duration::from_secs(3600);
/// The most looks a test takes at the log for a step another thread makes.
const LOOKS: usize = 50_000_000;

/// The fake host's log so far, kept.
fn seen(rig: &Rig) -> Vec<String> {
    rig.host.state().log.clone()
}

/// Waits until the log holds `line`, which another thread writes.
fn until_logged(rig: &Rig, line: &str) {
    for _ in 0..LOOKS {
        if seen(rig).iter().any(|seen| seen == line) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("{line} never came: {:?}", seen(rig));
}

/// Waits until the fake host's clock has been read `reads` times in all, by another thread.
fn until_read(rig: &Rig, reads: usize) {
    for _ in 0..LOOKS {
        if rig.host.state().clock_reads >= reads {
            return;
        }
        std::thread::yield_now();
    }
    let read = rig.host.state().clock_reads;
    panic!("the clock was read {read} times, never {reads}");
}

/// Moves the fake clock by `by` and wakes the waits on the table.
fn advance(rig: &Rig, by: Duration) {
    rig.host.state().advanced += by;
    rig.helper.clock_moved();
}

/// A rig whose lease `7.1` has a user (the test's own uid) and a run of pid [`PID`] in the
/// table, still running.
fn running(name: &str) -> Rig {
    let rig = rig(name, 2);
    rig.helper.user_create("7.1", None).unwrap();
    let lease = parse_lease("7.1").unwrap();
    rig.helper.runs.started(lease, PID, None);
    rig.host.state().log.clear();
    rig
}

fn kill_all_logged(rig: &Rig) -> bool {
    seen(rig).iter().any(|line| line.starts_with("kill_all"))
}

/// Catches: a `watch` that does not find the run a `run` started, or reports its end
/// differently from the `run` connection; one that finds a run for a lease that has none,
/// or for a lease whose user was deleted.
#[test]
fn watch_finds_the_last_run_and_reports_its_end() {
    let rig = rig("watch", 2);
    rig.helper.user_create("7.1", None).unwrap();
    let error = rig.helper.watch("7.1").unwrap_err();
    assert_eq!(error, format!("lease 7.1 {NO_RUN}"));
    assert!(rig.helper.watch("7.x").is_err());
    assert_eq!(rig.helper.wait_watched("7.1"), None);
    assert_eq!(rig.helper.wait_watched("7.x"), None);
    let run = Request::Run {
        lease: "7.1".to_owned(),
        argv: act_argv("exit 3"),
        env: Vec::new(),
        session: Session::Background,
        limits: None,
        deadline_ms: None,
    };
    let Outcome::Running(mut child) = rig.helper.handle(run, fds(rig.dir.path())) else {
        panic!("the run did not start");
    };
    let Outcome::Watching(lease, pid) = rig.helper.handle(
        Request::Watch {
            lease: "7.1".to_owned(),
        },
        Vec::new(),
    ) else {
        panic!("watch did not find the run");
    };
    assert_eq!((lease.as_str(), pid), ("7.1", child.id()));
    assert_eq!(rig.helper.wait(&mut child).unwrap().code(), Some(3));
    assert_eq!(rig.helper.wait_watched("7.1"), Some((Some(3), None)));
    let deleted = rig.helper.handle(
        Request::UserDelete {
            lease: "7.1".to_owned(),
        },
        Vec::new(),
    );
    assert!(matches!(deleted, Outcome::Reply(Reply::Deleted { .. })));
    assert!(rig.helper.watch("7.1").is_err(), "forgotten with its user");
}

/// Catches: a `stop` that kills before the grace has passed, or never sends the `SIGTERM`;
/// the clock is the host's, moved by the test. The stopper reads the clock for its grace,
/// then again under the table's lock before it waits; the test moves the clock only after
/// both reads, so the stopper waits on the table with the grace still ahead.
#[test]
fn stop_sends_sigterm_and_kills_only_after_the_grace() {
    let rig = running("stop-grace");
    let reads = rig.host.state().clock_reads;
    std::thread::scope(|scope| {
        let stopping = scope.spawn(|| rig.helper.stop("7.1", HOUR));
        until_logged(&rig, &format!("term_group {PID}"));
        until_read(&rig, reads + 2);
        assert!(!kill_all_logged(&rig), "killed before the grace");
        advance(&rig, HOUR - Duration::from_secs(1));
        until_read(&rig, reads + 3);
        assert!(!kill_all_logged(&rig), "killed before the grace");
        advance(&rig, Duration::from_secs(1));
        assert_eq!(stopping.join().unwrap(), Ok(()));
    });
    let log = seen(&rig);
    let term = log.iter().position(|line| line.starts_with("term_group"));
    let kill = log.iter().position(|line| line.starts_with("kill_all"));
    assert!(term < kill, "{log:?}");
}

/// Catches: a run that ends on its `SIGTERM` killed anyway, and the stop through the wire.
#[test]
fn a_run_that_ends_on_sigterm_is_never_killed() {
    let rig = running("stop-soft");
    std::thread::scope(|scope| {
        let stopping = scope.spawn(|| {
            let request = Request::Stop {
                lease: "7.1".to_owned(),
                grace_ms: 3_600_000,
            };
            rig.helper.handle(request, Vec::new())
        });
        until_logged(&rig, &format!("term_group {PID}"));
        rig.helper.runs.ended(PID, (None, Some(15)));
        let answer = stopping.join().unwrap();
        assert!(
            matches!(answer, Outcome::Reply(Reply::Killed)),
            "{answer:?}"
        );
    });
    assert!(!kill_all_logged(&rig), "{:?}", seen(&rig));
}

/// Catches: a `stop` that signals nothing it can name and so never kills (no run in the
/// table, or one that ended), one that acts on a lease with no user, and a failed signal
/// taken for a sent one.
#[test]
fn stop_without_a_running_process_kills_at_once_and_refuses_what_it_cannot_do() {
    let rig = rig("stop-plain", 2);
    rig.helper.user_create("7.1", None).unwrap();
    rig.host.state().log.clear();
    assert_eq!(rig.helper.stop("7.1", HOUR), Ok(()));
    assert!(kill_all_logged(&rig));
    assert!(!seen(&rig).iter().any(|line| line.starts_with("term_group")));
    let lease = parse_lease("7.1").unwrap();
    rig.helper.runs.started(lease, PID, None);
    rig.helper.runs.ended(PID, (Some(0), None));
    rig.host.state().log.clear();
    assert_eq!(rig.helper.stop("7.1", HOUR), Ok(()));
    assert!(kill_all_logged(&rig));
    let error = rig.helper.stop("7.2", HOUR).unwrap_err();
    assert_eq!(error, "lease 7.2 has no user");
    assert!(rig.helper.stop("7.x", HOUR).is_err());
    rig.helper.runs.started(lease, PID, None);
    rig.host.state().fail.insert("term_group");
    let error = rig.helper.stop("7.1", HOUR).unwrap_err();
    assert_eq!(
        error,
        format!("signalling process group {PID}: term_group failed")
    );
}

/// Catches: a deadline not enforced once the host's clock passes it (the run outlives a
/// dead agent with no bound), one enforced early, and a run that ended before its deadline
/// killed anyway. The holder waits on the table with the deadline still ahead each time
/// the test moves the clock: the test moves it only once the holder has read it, and the
/// holder reads it under the table's lock, which [`advance`] takes to wake it.
#[test]
fn the_helper_ends_a_run_when_its_deadline_passes() {
    let rig = running("deadline");
    let lease = parse_lease("7.1").unwrap();
    let (at, reads) = {
        let state = rig.host.state();
        (state.advanced, state.clock_reads)
    };
    rig.helper
        .runs
        .started(lease, PID, Some(crate::testing::epoch() + at + HOUR));
    std::thread::scope(|scope| {
        let holding = scope.spawn(|| rig.helper.hold_deadline(PID));
        until_read(&rig, reads + 1);
        advance(&rig, HOUR - Duration::from_secs(1));
        until_read(&rig, reads + 2);
        assert!(!kill_all_logged(&rig), "killed before the deadline");
        advance(&rig, Duration::from_secs(1));
        holding.join().unwrap();
    });
    assert!(kill_all_logged(&rig), "{:?}", seen(&rig));
    // Ended before its deadline: nothing is killed. No deadline, or no such run: nothing.
    rig.host.state().log.clear();
    let later = crate::testing::epoch() + rig.host.state().advanced + HOUR;
    rig.helper.runs.started(lease, PID, Some(later));
    rig.helper.runs.ended(PID, (Some(0), None));
    rig.helper.hold_deadline(PID);
    rig.helper.runs.started(lease, PID, None);
    rig.helper.hold_deadline(PID);
    rig.helper.hold_deadline(PID + 1);
    assert!(!kill_all_logged(&rig), "{:?}", seen(&rig));
}

/// Catches: a kill at the deadline that fails taken for one that worked (it is only logged:
/// the waiting connection still reports the end), and a deadline in the past enforced at
/// once.
#[test]
fn a_kill_at_the_deadline_that_fails_is_logged() {
    let rig = running("deadline-fails");
    let lease = parse_lease("7.1").unwrap();
    rig.host.state().fail.insert("kill_all");
    rig.helper
        .runs
        .started(lease, PID, Some(crate::testing::epoch()));
    rig.helper.hold_deadline(PID);
    assert!(kill_all_logged(&rig));
}

/// Catches: `run`'s `deadline_ms` not reaching the table (the run would have no bound once
/// its agent is gone), and a waited run whose end is not recorded.
#[test]
fn a_run_with_a_deadline_is_held_to_it_and_its_end_recorded() {
    let rig = rig("run-deadline", 2);
    rig.helper.user_create("7.1", None).unwrap();
    let run = Request::Run {
        lease: "7.1".to_owned(),
        argv: act_argv("exit 0"),
        env: Vec::new(),
        session: Session::Background,
        limits: None,
        deadline_ms: Some(60_000),
    };
    let Outcome::Running(mut child) = rig.helper.handle(run, fds(rig.dir.path())) else {
        panic!("the run did not start");
    };
    let lease = parse_lease("7.1").unwrap();
    let entry = rig.helper.runs.lock().get(&lease).copied().unwrap();
    let want = crate::testing::epoch() + rig.host.state().advanced + Duration::from_secs(60);
    assert_eq!(entry.deadline, Some(want));
    assert!(rig.helper.wait(&mut child).unwrap().success());
    assert_eq!(rig.helper.wait_watched("7.1"), Some((Some(0), None)));
    assert!(!kill_all_logged(&rig));
}

/// Catches: the end of a run recorded on another lease's entry that holds the same pid from
/// an earlier, ended run (pids are reused), which would leave the new run's `watch` and
/// `stop` waiting for an end already reported.
#[test]
fn an_end_is_recorded_on_the_running_entry_of_its_pid() {
    let rig = rig("pid-reuse", 3);
    let (first, second) = (parse_lease("7.1").unwrap(), parse_lease("7.2").unwrap());
    rig.helper.runs.started(first, PID, None);
    rig.helper.runs.ended(PID, (Some(0), None));
    rig.helper.runs.started(second, PID, None);
    rig.helper.runs.ended(PID, (Some(5), None));
    assert_eq!(rig.helper.wait_watched("7.2"), Some((Some(5), None)));
    assert_eq!(rig.helper.wait_watched("7.1"), Some((Some(0), None)));
}

/// Catches: the end of a process the table does not hold (a `role-run`'s) recorded on some
/// entry, and a `watch` that answers before the run it watches has ended.
#[test]
fn a_watch_waits_for_the_end_and_an_unknown_end_changes_nothing() {
    let rig = rig("watch-wait", 2);
    let lease = parse_lease("7.1").unwrap();
    rig.helper.runs.ended(PID, (Some(1), None));
    rig.helper.runs.started(lease, PID, None);
    rig.helper.runs.ended(PID + 1, (Some(1), None));
    std::thread::scope(|scope| {
        let watching = scope.spawn(|| rig.helper.wait_watched("7.1"));
        // The watch may be waiting already or not yet: either way it sees this end.
        rig.helper.runs.ended(PID, (None, Some(15)));
        assert_eq!(watching.join().unwrap(), Some((None, Some(15))));
    });
}
