//! The watchdog of `js-update-hook-fake`: a bound on the whole step process that does not
//! depend on the step calling its clock.
//!
//! The hang guard (`harness::Guard`) holds a step's waits to its declared bound, but it sees
//! only waits the step asks for. A step that blocks (a lock taken with a wait, a read that never
//! returns) or spins without pausing never asks. The watchdog runs on a thread of its own and
//! watches the step's progress events: a kill point or a pause ([`progress`]). When no event
//! comes in one watch window, it kills the step's whole process group with SIGKILL, as the
//! updater kills a step past its timeout. The harness started the step as the leader of a group
//! of its own, so the step and anything it started die, every pipe it held closes, and the
//! harness reaps it and reports a step that died of a signal.
//!
//! It bounds the steps run through `js-update-hook-fake`, the hook's own step code over the
//! fakes; it is no bound on an arbitrary hook binary the harness might start, whose waits the
//! harness itself does not bound.
//!
//! What a window is, is injected ([`watch`]'s `wait`): the binary waits a span of real time
//! ([`wait_for`]), and only ever to end a step that has stopped making progress; with
//! [`ON_STALL`] a window also ends at the step's own announcement that it is stuck
//! ([`announce_stall`], [`wait_announced`]), so a test of a stuck step need not wait out a
//! window. The span of real time stays the fail-safe in both modes: a step that blocks without
//! announcing it is ended at the end of its window all the same. A test of the watchdog itself
//! gives it windows that end when the test says.
//!
//! Progress cannot keep a step alive for ever: a step that reaches more than [`MAX_POINTS`]
//! kill points is killed as well ([`point`]). A loop that changes things without pausing is
//! bounded by its kill points, one that pauses by the hang guard, one that does neither by the
//! window.
//!
//! The kill is of the step's process group only when the step leads that group, as the harness
//! makes it ([`kill_step`]); otherwise only the step itself is killed, so a caller that started
//! the binary in its own group is never signalled.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Progress events of this process: kill points and pauses.
static PROGRESS: AtomicU64 = AtomicU64::new(0);

/// Names the watch window of the binary, in milliseconds (default [`DEFAULT_WINDOW_MS`]).
pub const WINDOW_VAR: &str = "UPDATE_HOOK_FAKE_WATCH_MS";

/// The window the binary waits for a progress event when none is named: far longer than any step
/// of the fakes takes between two events, which is a few file operations.
pub const DEFAULT_WINDOW_MS: u64 = 60_000;

/// The value of [`WINDOW_VAR`] that makes a window end at the step's own announcement that it
/// is stuck, or at the end of its span, whichever comes first: `on-stall` (the default span) or
/// `on-stall:<ms>`.
pub const ON_STALL: &str = "on-stall";

/// The watchdog's mode and span for a value of [`WINDOW_VAR`]: `<ms>` is a timed window of that
/// span; `on-stall` and `on-stall:<ms>` end a window at the announcement too. Anything else, or
/// no value, is a timed window of [`DEFAULT_WINDOW_MS`].
#[must_use]
pub fn parse_window(value: Option<&str>) -> (bool, Duration) {
    let default = Duration::from_millis(DEFAULT_WINDOW_MS);
    let Some(value) = value else {
        return (false, default);
    };
    let (on_stall, ms) = match value.strip_prefix(ON_STALL) {
        Some("") => return (true, default),
        Some(rest) => (true, rest.strip_prefix(':')),
        None => (false, Some(value)),
    };
    let span = ms
        .and_then(|ms| ms.parse().ok())
        .map_or(default, Duration::from_millis);
    (on_stall, span)
}

/// The most kill points one step may reach; far more than any step of the hook reaches (its
/// waits renew a cordon a few dozen times at most).
pub const MAX_POINTS: u64 = 10_000;

/// Kill points of this process.
static POINTS: AtomicU64 = AtomicU64::new(0);

/// Whether the step is stuck, and whether it is over.
static STATE: Mutex<(bool, bool)> = Mutex::new((false, false));
static CHANGED: Condvar = Condvar::new();

/// One progress event of the step.
pub fn progress() {
    PROGRESS.fetch_add(1, Ordering::SeqCst);
}

/// One kill point: a progress event, and `true` once the step has reached more than
/// [`MAX_POINTS`] of them.
pub fn point() -> bool {
    progress();
    POINTS.fetch_add(1, Ordering::SeqCst) + 1 > MAX_POINTS
}

/// The step is stuck in a call that will not return ([`ON_STALL`]).
pub fn announce_stall() {
    let mut state = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.0 = true;
    CHANGED.notify_all();
}

/// The step is over: the watchdog stops at its next window.
pub fn finish() {
    let mut state = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.1 = true;
    CHANGED.notify_all();
}

/// A window of `span` of real time, ended early when the step is over.
pub fn wait_for(span: Duration) -> Window {
    let state = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (state, _) = CHANGED
        .wait_timeout_while(state, span, |state| !state.1)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.1 {
        Window::Stopped
    } else {
        Window::Passed
    }
}

/// A window that ends when the step announces it is stuck, when it is over, or at the end of
/// `span`, the fail-safe for a step that blocks without announcing it.
pub fn wait_announced(span: Duration) -> Window {
    let state = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (state, _) = CHANGED
        .wait_timeout_while(state, span, |state| !state.0 && !state.1)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.1 {
        Window::Stopped
    } else {
        Window::Passed
    }
}

/// Ends the step as the updater ends one past its timeout, with SIGKILL: its whole process
/// group when it leads one (the harness starts it so), else only itself.
pub fn kill_step(why: &str) -> ! {
    use rustix::process::{Signal, getpgrp, getpid, kill_current_process_group, kill_process};
    eprintln!("js-update-hook-fake: {why}: stopped");
    if getpgrp() == getpid() {
        let _ = kill_current_process_group(Signal::KILL);
    } else {
        let _ = kill_process(getpid(), Signal::KILL);
    }
    std::process::exit(1)
}

/// The events counted so far.
#[must_use]
pub fn events() -> u64 {
    PROGRESS.load(Ordering::SeqCst)
}

/// How a window ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// The window passed.
    Passed,
    /// The step is over: the watchdog stops.
    Stopped,
}

/// Watches `events`: after each window that `wait` ends, if no event came during it, calls
/// `fire` and returns `true`; returns `false` when `wait` says the step is over.
pub fn watch(events: &dyn Fn() -> u64, wait: &mut dyn FnMut() -> Window, fire: &dyn Fn()) -> bool {
    let mut seen = events();
    loop {
        if wait() == Window::Stopped {
            return false;
        }
        let now = events();
        if now == seen {
            fire();
            return true;
        }
        seen = now;
    }
}
