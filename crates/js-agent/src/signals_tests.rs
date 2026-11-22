//! The line the loop waits on: stop and replace requests and the job worker's bell, in the order
//! a wait takes them. The long waits are guards against a hang, never a time a test expects to
//! pass: every wait below that is not given zero ends because something is on the line.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::{Line, SystemSleeper};
use crate::run::{Sleeper, Wake};

const GUARD: Duration = Duration::from_secs(60);

fn sleeper() -> SystemSleeper {
    SystemSleeper {
        line: Arc::new(Line::default()),
    }
}

fn ask_stop(sleeper: &SystemSleeper) {
    sleeper.line.call(|calls| calls.stops += 1);
}

fn ask_replace(sleeper: &SystemSleeper) {
    sleeper.line.call(|calls| calls.replaces += 1);
}

#[test]
fn a_rung_bell_ends_one_wait_that_may_end_early() {
    let mut sleeper = sleeper();
    let bell = sleeper.bell().expect("the system sleeper has a bell");
    bell.wake();
    assert_eq!(sleeper.sleep_or_bell(GUARD), Wake::Bell);
    // Taken: the next wait runs its course.
    assert_eq!(sleeper.sleep_or_bell(Duration::ZERO), Wake::Elapsed);
}

#[test]
fn a_plain_sleep_runs_its_course_past_the_bell_and_leaves_it_rung() {
    let mut sleeper = sleeper();
    let bell = sleeper.bell().expect("the system sleeper has a bell");
    bell.wake();
    assert_eq!(sleeper.sleep(Duration::ZERO), Wake::Elapsed);
    assert_eq!(sleeper.sleep_or_bell(GUARD), Wake::Bell);
}

#[test]
fn a_stop_request_comes_before_the_bell_and_each_is_taken_once() {
    let mut sleeper = sleeper();
    let bell = sleeper.bell().expect("the system sleeper has a bell");
    ask_stop(&sleeper);
    ask_stop(&sleeper);
    bell.wake();
    assert_eq!(sleeper.sleep_or_bell(GUARD), Wake::Stop);
    assert_eq!(sleeper.sleep(GUARD), Wake::Stop);
    assert_eq!(sleeper.sleep_or_bell(GUARD), Wake::Bell);
    assert_eq!(sleeper.sleep_or_bell(Duration::ZERO), Wake::Elapsed);
}

#[test]
fn a_bell_rung_from_another_thread_ends_the_wait() {
    let mut sleeper = sleeper();
    let bell = sleeper.bell().expect("the system sleeper has a bell");
    let ringer = thread::spawn(move || bell.wake());
    assert_eq!(sleeper.sleep_or_bell(GUARD), Wake::Bell);
    ringer.join().expect("the ringer ended");
}

#[test]
fn a_stop_request_from_another_thread_ends_a_plain_sleep() {
    let mut sleeper = sleeper();
    let line = Arc::clone(&sleeper.line);
    let asker = thread::spawn(move || line.call(|calls| calls.stops += 1));
    assert_eq!(sleeper.sleep(GUARD), Wake::Stop);
    asker.join().expect("the asker ended");
}

#[test]
fn a_replace_request_ends_a_wait_as_a_replace() {
    let mut sleeper = sleeper();
    ask_replace(&sleeper);
    assert_eq!(sleeper.sleep(GUARD), Wake::Replace);
    // Taken: the next wait runs its course.
    assert_eq!(sleeper.sleep(Duration::ZERO), Wake::Elapsed);
}

#[test]
fn a_stop_request_is_taken_before_a_replace_request_and_a_replace_before_the_bell() {
    let mut sleeper = sleeper();
    let bell = sleeper.bell().expect("the system sleeper has a bell");
    bell.wake();
    ask_replace(&sleeper);
    ask_stop(&sleeper);
    assert_eq!(sleeper.sleep_or_bell(GUARD), Wake::Stop);
    assert_eq!(sleeper.sleep_or_bell(GUARD), Wake::Replace);
    assert_eq!(sleeper.sleep_or_bell(GUARD), Wake::Bell);
    assert_eq!(sleeper.sleep_or_bell(Duration::ZERO), Wake::Elapsed);
}

#[test]
fn a_replace_request_from_another_thread_ends_the_wait() {
    let mut sleeper = sleeper();
    let line = Arc::clone(&sleeper.line);
    let asker = thread::spawn(move || line.call(|calls| calls.replaces += 1));
    assert_eq!(sleeper.sleep(GUARD), Wake::Replace);
    asker.join().expect("the asker ended");
}

/// Whether this process ignores `signal`, as `sigaction(2)` reports its disposition (read on
/// Linux and macOS alike: macOS has no `/proc`).
fn ignores(signal: i32) -> bool {
    let mut now = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: a null new action makes the call read the disposition and change nothing;
    // `now` is a writable `sigaction` the call fills.
    let answer = unsafe { libc::sigaction(signal, std::ptr::null(), now.as_mut_ptr()) };
    assert_eq!(answer, 0, "the disposition of signal {signal}");
    // SAFETY: the call succeeded, so it wrote the whole `sigaction` (and it was zeroed).
    unsafe { now.assume_init() }.sa_sigaction == libc::SIG_IGN
}

/// Asserts that the system sleeper readies an exec: SIGHUP ignored (a disposition the exec
/// keeps) and SIGTERM not, and that a stop request on the line then wins over the exec while a
/// replace request does not. It catches a next program that a repeated SIGHUP ends before it
/// installs its handler, and a stop the handlers took that the exec would drop.
#[test]
fn readying_an_exec_ignores_sighup_and_a_stop_on_the_line_wins() {
    let mut sleeper = sleeper();
    ask_replace(&sleeper);
    assert!(sleeper.ready_to_run_again());
    assert!(ignores(libc::SIGHUP));
    assert!(!ignores(libc::SIGTERM));
    assert!(!ignores(libc::SIGINT));
    ask_stop(&sleeper);
    assert!(!sleeper.ready_to_run_again());
}
