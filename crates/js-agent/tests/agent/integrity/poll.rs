//! The process check asked as a step of the worker ([`CheckHook::poll`]): the first look that
//! sees a process that would be a job's leftover answers the settle wait instead of waiting,
//! and the next call for the same run takes the second look and judges. The host is the
//! in-memory fake: nothing waits, and the host changes between the calls as it would during
//! the wait.

use std::time::Duration;

use js_agent::integrity::{FakeHost, Integrity, code, input};
use js_agent::lifecycle::{CheckFinding, CheckHook as _, CheckNote, Checked, Finished};

use super::{JOB_UID, mount, process, said, started};

/// The wait of these tests.
const WAIT: Duration = Duration::from_millis(300);

/// The control group of the agent's own service, below which a job's leftover can be.
const LEFT: &str = "0::/system.slice/agent.service/left";

/// What the checks answer when asked as a step after the run `run_id`, with no other
/// container on the host.
fn polled(checks: &mut Integrity, run_id: &str) -> Checked {
    checks.poll(&Finished {
        run_id,
        live: &[],
        slot: None,
        live_slots: &[],
        held_mounts: &[],
    })
}

/// Asserts that a process of the job users' range at the first look answers the wait, with
/// no wait on the caller's thread (the view is never asked to settle), and that the next
/// call passes when the process has ended meanwhile; and that a process still there at the
/// next call is the `process-as-job-user` it always was, named in full. It catches a poll
/// that waits on the worker's thread, a first look that judges a process that is ending,
/// and a second look that lets a real leftover go.
#[test]
fn a_leftover_at_the_first_look_answers_the_wait_and_the_next_call_judges_what_is_left() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    assert_eq!(polled(&mut checks, "r1"), Checked::Wait(WAIT));
    assert!(host.settled().is_empty(), "the caller's thread did not wait");
    host.end_process(300);
    assert_eq!(polled(&mut checks, "r1"), Checked::Done(Ok(())));
    assert!(checks.notes().is_empty());

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    assert_eq!(polled(&mut checks, "r1"), Checked::Wait(WAIT));
    let Checked::Done(Err(found)) = polled(&mut checks, "r1") else {
        panic!("the second call judges");
    };
    assert_eq!(
        said(&found),
        (
            true,
            code::PROCESS_AS_JOB_USER,
            "pid 300 (stray, state S, cgroup 0::/system.slice/agent.service/left, owner 65534) is running after job r1".to_owned()
        )
    );
    assert!(host.settled().is_empty());
}

/// Asserts that a call with nothing to wait for gives the verdict at once: a clean host
/// passes, a host whose process table cannot be read is a check that could not run, and
/// checks given no wait judge a leftover at the first look. It catches a wait answered
/// where the first look already decides.
#[test]
fn a_call_with_nothing_to_wait_for_gives_the_verdict_at_once() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    assert_eq!(polled(&mut checks, "r1"), Checked::Done(Ok(())));

    host.fail(input::PROCESSES);
    let Checked::Done(Err(found)) = polled(&mut checks, "r2") else {
        panic!("a blind first look judges");
    };
    assert!(matches!(found, CheckFinding::Unchecked { .. }), "{found:?}");
    assert_eq!(found.code(), code::PROCESSES_UNREADABLE);

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    let Checked::Done(Err(found)) = polled(&mut checks, "r1") else {
        panic!("checks with no wait judge at once");
    };
    assert_eq!(found.code(), code::PROCESS_AS_JOB_USER);
    assert!(host.settled().is_empty());
}

/// Asserts that a call for another run than the one whose first look waits takes a first
/// look of its own: after r1 answered the wait, the call for r2 answers the wait too (a
/// second look would judge), and the call for r1 that follows looks first again and passes
/// once the process has ended. It catches the second look of one run judged as another's.
#[test]
fn a_call_for_another_run_looks_first() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    assert_eq!(polled(&mut checks, "r1"), Checked::Wait(WAIT));
    assert_eq!(polled(&mut checks, "r2"), Checked::Wait(WAIT));
    host.end_process(300);
    assert_eq!(polled(&mut checks, "r1"), Checked::Done(Ok(())));
}

/// Asserts that a process table that cannot be read at the second look is a check that
/// could not run, not a pass. It catches a failed second look read as an empty host.
#[test]
fn a_second_look_that_cannot_read_the_processes_is_unchecked() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    assert_eq!(polled(&mut checks, "r1"), Checked::Wait(WAIT));
    host.fail(input::PROCESSES);
    let Checked::Done(Err(found)) = polled(&mut checks, "r1") else {
        panic!("the second call judges");
    };
    assert!(matches!(found, CheckFinding::Unchecked { .. }), "{found:?}");
    assert_eq!(found.code(), code::PROCESSES_UNREADABLE);
}

/// Asserts that the second look is the host that is compared: a process that starts
/// between the two calls is not judged by this check (the next check judges it, with a
/// wait of its own), and a mount made between them is this check's note, given once. It
/// catches a second look that judges processes the first did not see, and notes taken
/// from the first look.
#[test]
fn what_changes_between_the_calls_is_part_of_the_second_look() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]).with_settle(WAIT);
    host.start_process(process(300, 100, JOB_UID, 900, LEFT));
    assert_eq!(polled(&mut checks, "r1"), Checked::Wait(WAIT));
    host.end_process(300);
    host.start_process(process(301, 100, JOB_UID, 950, LEFT));
    host.mount(mount("/mnt/late", "/dev/sdc"));
    assert_eq!(polled(&mut checks, "r1"), Checked::Done(Ok(())));
    assert_eq!(
        checks.notes(),
        [CheckNote::new(
            code::NEW_MOUNT,
            "/mnt/late (ext4) from /dev/sdc"
        )]
    );
    assert_eq!(polled(&mut checks, "r2"), Checked::Wait(WAIT));
    assert!(checks.notes().is_empty(), "a wait gives no notes");
}
