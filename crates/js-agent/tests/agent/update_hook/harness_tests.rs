//! The fake host's own parts, in this process: the hang guard and the watchdog, and the words that name its faults and defects
//! (tests/update_hook_fakes/harness.rs and watchdog.rs).

use std::cell::Cell;
use std::fs;
use std::time::Duration;

use js_testkit::dir::TestDir;

use super::fakes::harness::{self, Defect};
use super::fakes::watchdog::{self, Window};

/// A clock of the test's: a pause moves it on by what was asked.
struct TestClock(std::cell::Cell<u64>);

impl js_agent::update_hook::Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }

    fn pause(&self, span: std::time::Duration) {
        self.0
            .set(self.0.get() + u64::try_from(span.as_millis()).unwrap());
    }
}

/// The guard lets a step wait up to its declared bound and fires on the first pause past it,
/// with the total asked; the clock it guards moves only for the pauses it lets through.
#[test]
fn the_hang_guard_fires_on_the_first_wait_past_the_bound_and_not_before() {
    use js_agent::update_hook::Clock;
    use std::cell::RefCell;
    use std::time::Duration;
    let clock = TestClock(std::cell::Cell::new(0));
    let fired = RefCell::new(Vec::new());
    let fire = |asked: Duration| fired.borrow_mut().push(asked);
    let guard = harness::Guard::new(&clock, Duration::from_secs(30), &fire);
    for _ in 0..3 {
        guard.pause(Duration::from_secs(10));
    }
    assert_eq!(fired.borrow().as_slice(), &[] as &[Duration]);
    assert_eq!(
        (guard.now_ms(), guard.asked()),
        (30_000, Duration::from_secs(30))
    );
    guard.pause(Duration::from_secs(1));
    assert_eq!(fired.borrow().as_slice(), [Duration::from_secs(31)]);
    assert_eq!(
        guard.now_ms(),
        30_000,
        "the wait past the bound is not taken"
    );
}

/// On a clock that stands still the guard still counts what is asked: a step whose own
/// deadline never comes is stopped all the same.
#[test]
fn the_hang_guard_counts_the_waits_asked_even_when_the_clock_stands_still() {
    use js_agent::update_hook::Clock;
    use std::cell::Cell;
    use std::time::Duration;
    let clock = TestClock(Cell::new(5));
    let frozen = harness::Frozen(&clock);
    let fired = Cell::new(0);
    let fire = |_: Duration| fired.set(fired.get() + 1);
    let budget = harness::Guard::budget_of(js_agent::update_hook::Step::Verify);
    assert_eq!(budget, Duration::from_secs(615));
    let guard = harness::Guard::new(&frozen, budget, &fire);
    let mut pauses = 0;
    while fired.get() == 0 {
        guard.pause(Duration::from_secs(10));
        pauses += 1;
    }
    assert_eq!(pauses, 62);
    assert_eq!(guard.now_ms(), 5);
}

/// The faults each step of the hook can be made to end with, and the words of the faults and
/// the defects.
#[test]
fn the_faults_a_step_can_end_with_and_the_words_that_name_faults_and_defects() {
    use harness::Fault as F;
    use js_agent::update_hook::Step as S;
    let reachable: Vec<(S, F)> = [S::Prepare, S::Cutover, S::Verify, S::Rollback, S::Cleanup]
        .into_iter()
        .flat_map(|step| [F::Fail, F::Defer, F::Halt].map(|fault| (step, fault)))
        .filter(|(step, fault)| fault.reachable(*step))
        .collect();
    assert_eq!(
        reachable,
        [
            (S::Prepare, F::Fail),
            (S::Prepare, F::Defer),
            (S::Prepare, F::Halt),
            (S::Cutover, F::Fail),
            (S::Cutover, F::Defer),
            (S::Cutover, F::Halt),
            (S::Verify, F::Fail),
            (S::Verify, F::Halt),
            (S::Rollback, F::Fail),
            (S::Rollback, F::Halt),
            (S::Cleanup, F::Fail),
        ]
    );
    let on_first_install: Vec<(S, F)> = reachable
        .iter()
        .copied()
        .filter(|(step, fault)| fault.reachable_on_first_install(*step))
        .collect();
    // Nothing defers on a first install but `prepare`, when the join token file is not right.
    let without_defer: Vec<(S, F)> = reachable
        .iter()
        .copied()
        .filter(|(step, fault)| *fault != F::Defer || *step == S::Prepare)
        .collect();
    assert_eq!(on_first_install, without_defer);
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-faults");
    assert_eq!(harness::arranged(tmp.path(), S::Verify), None);
    for fault in [F::Fail, F::Defer, F::Halt] {
        harness::arrange(tmp.path(), S::Verify, fault);
        assert_eq!(harness::arranged(tmp.path(), S::Verify), Some(fault));
    }
    fs::write(tmp.path().join("faults/verify"), "melt").unwrap();
    assert_eq!(harness::arranged(tmp.path(), S::Verify), None);
    for defect in Defect::ALL {
        assert_eq!(Defect::from_word(Some(&defect.word().into())), Some(defect));
    }
    assert_eq!(Defect::from_word(Some(&"melt".into())), None);
    assert_eq!(Defect::from_word(None), None);
}

/// A step asking zero-length pauses in a loop asks no time, and is stopped all the same once it
/// has asked more pauses than its bound has seconds.
#[test]
fn the_hang_guard_stops_a_loop_of_zero_length_pauses() {
    use js_agent::update_hook::Clock;
    let clock = TestClock(Cell::new(0));
    let fired = Cell::new(0_u32);
    let fire = |_: Duration| fired.set(fired.get() + 1);
    let guard = harness::Guard::new(&clock, Duration::from_secs(3), &fire);
    for _ in 0..3 {
        guard.pause(Duration::ZERO);
    }
    assert_eq!(fired.get(), 0, "three pauses fit a bound of three seconds");
    guard.pause(Duration::ZERO);
    assert_eq!(fired.get(), 1);
    assert_eq!(guard.asked(), Duration::ZERO);
}

/// The watchdog fires on the first window in which no progress event came, and only then; it
/// stops without firing when the step is over. The windows are the test's: nothing waits.
#[test]
fn the_watchdog_fires_on_the_first_window_without_progress_and_never_before() {
    let events = Cell::new(0_u64);
    let fired = Cell::new(false);
    let fire = || fired.set(true);
    // Three windows with progress in each, then one without.
    let mut windows = 0;
    let mut wait = || {
        windows += 1;
        if windows <= 3 {
            events.set(events.get() + 1);
        }
        Window::Passed
    };
    assert!(watchdog::watch(&|| events.get(), &mut wait, &fire));
    assert!(fired.get());
    assert_eq!(windows, 4);

    // The step ends before a window without progress: no fire.
    let fired = Cell::new(false);
    let fire = || fired.set(true);
    let events = Cell::new(0_u64);
    let mut windows = 0;
    let mut wait = || {
        windows += 1;
        events.set(events.get() + 1);
        if windows == 2 {
            Window::Stopped
        } else {
            Window::Passed
        }
    };
    assert!(!watchdog::watch(&|| events.get(), &mut wait, &fire));
    assert!(!fired.get());

    // The first window already without progress fires at once.
    let fired = Cell::new(false);
    let fire = || fired.set(true);
    assert!(watchdog::watch(&|| 7, &mut || Window::Passed, &fire));
    assert!(fired.get());
}

/// Kill points and pauses are the progress events the watchdog watches.
#[test]
fn kill_points_and_pauses_are_progress_events() {
    use js_agent::update_hook::{Clock, Gate};
    let before = watchdog::events();
    harness::HarnessGate::new(None, None).point();
    let clock = TestClock(Cell::new(0));
    let fire = |_: Duration| {};
    harness::Guard::new(&clock, Duration::from_secs(60), &fire).pause(Duration::from_secs(1));
    // Other tests of this process count too: at least these two.
    assert!(watchdog::events() >= before + 2);
}

/// The watchdog's window from its variable: a span, the announcement with the default span or a
/// given one, and the default for no value or one that does not parse.
#[test]
fn the_watch_window_is_read_from_its_variable() {
    let default = Duration::from_millis(watchdog::DEFAULT_WINDOW_MS);
    assert_eq!(watchdog::parse_window(None), (false, default));
    assert_eq!(
        watchdog::parse_window(Some("250")),
        (false, Duration::from_millis(250))
    );
    assert_eq!(watchdog::parse_window(Some("on-stall")), (true, default));
    assert_eq!(
        watchdog::parse_window(Some("on-stall:300")),
        (true, Duration::from_millis(300))
    );
    assert_eq!(watchdog::parse_window(Some("on-stall:x")), (true, default));
    assert_eq!(watchdog::parse_window(Some("soon")), (false, default));
}
