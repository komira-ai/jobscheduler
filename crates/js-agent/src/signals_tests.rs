//! The line the loop waits on: stop requests and the job worker's bell, in the order a wait
//! takes them. The long waits are guards against a hang, never a time a test expects to
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
