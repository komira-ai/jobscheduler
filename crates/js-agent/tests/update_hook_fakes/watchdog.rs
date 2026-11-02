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
//! What a window is, is injected ([`watch`]'s `wait`): the binary waits a span of real time,
//! and only ever to end a step that has stopped making progress. A test of the watchdog itself
//! gives it windows that end when the test says.

use std::sync::atomic::{AtomicU64, Ordering};

/// Progress events of this process: kill points and pauses.
static PROGRESS: AtomicU64 = AtomicU64::new(0);

/// Names the watch window of the binary, in milliseconds (default [`DEFAULT_WINDOW_MS`]).
pub const WINDOW_VAR: &str = "UPDATE_HOOK_FAKE_WATCH_MS";

/// The window the binary waits for a progress event when none is named: far longer than any step
/// of the fakes takes between two events, which is a few file operations.
pub const DEFAULT_WINDOW_MS: u64 = 60_000;

/// One progress event of the step.
pub fn progress() {
    PROGRESS.fetch_add(1, Ordering::SeqCst);
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
