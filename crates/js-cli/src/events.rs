//! What a command waits on: the injected waiters and the signals.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use js_client::jobs::{Dialer, TokioWaiter};
use tokio::sync::mpsc::UnboundedSender;

use crate::signals;

/// How the client lets time pass. Nothing in `js` reads a clock or sleeps on its own: the
/// poll of a pending start goes through a waiter, so a test drives it and nothing in it
/// waits for real.
pub trait Waiter: Send + Sync + 'static {
    /// Returns once `interval` has passed.
    fn wait(&self, interval: Duration);
}

/// The waiter of the program: the thread sleeps.
#[derive(Debug, Clone, Copy)]
pub struct ThreadWaiter;

impl Waiter for ThreadWaiter {
    fn wait(&self, interval: Duration) {
        forward(interval, std::thread::sleep);
    }
}

/// Lets `interval` pass with `sleep`: the interval the waiter was given, unchanged.
fn forward(interval: Duration, sleep: impl FnOnce(Duration)) {
    sleep(interval);
}

/// A signal `job run` acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardedSignal {
    /// SIGINT.
    Interrupt,
    /// SIGTERM.
    Terminate,
    /// SIGHUP, which a terminal that closes sends.
    Hangup,
}

impl ForwardedSignal {
    /// The signal's number, which `js` adds to 128 for its own exit status when a signal
    /// ends `job run` a second time.
    pub(crate) fn number(self) -> u8 {
        match self {
            Self::Hangup => 1,
            Self::Interrupt => 2,
            Self::Terminate => 15,
        }
    }

    /// The signal's name, as a message shows it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }
}

/// Where a signal source reports the signals `js` receives: to the `job run` that reads
/// them as it waits.
#[derive(Debug, Clone)]
pub struct Signaller(UnboundedSender<ForwardedSignal>);

impl Signaller {
    /// The handle that reports to `job run` through `to`.
    pub(crate) fn job(to: UnboundedSender<ForwardedSignal>) -> Self {
        Self(to)
    }

    /// Reports `signal`: `job run` cancels its job (or, the second time, stops waiting). A
    /// report after the run has ended is dropped.
    pub fn deliver(&self, signal: ForwardedSignal) {
        let _ = self.0.send(signal);
    }
}

/// How `job run` learns of SIGINT, SIGTERM and SIGHUP: called once, before the job is
/// submitted, with the handle to report them to. The program's source installs the
/// handlers (and only for `job run`: the other commands keep the signals' default
/// behaviour); a signal that arrives before the run can act on it is held until it can.
pub type SignalSource = Box<dyn FnOnce(Signaller) -> io::Result<()> + Send>;

/// The effects a command has beyond HTTP, injected so a test drives them.
pub struct Services {
    pub(crate) waiter: Arc<dyn Waiter>,
    /// The pauses of the job commands' loops (`job wait`, `job follow`, `job run`): tokio's
    /// timer unless a test hands in another.
    pub(crate) job_waiter: Arc<dyn js_client::jobs::Waiter>,
    pub(crate) signals: SignalSource,
    /// How the job commands open their connection: TCP unless a test hands in another.
    pub(crate) dialer: Dialer,
}

impl Services {
    /// Services over this waiter and signal source; the job commands dial TCP and pause on
    /// tokio's timer.
    pub fn new(waiter: Arc<dyn Waiter>, signals: SignalSource) -> Self {
        Self {
            waiter,
            job_waiter: Arc::new(TokioWaiter),
            signals,
            dialer: Dialer::tcp(),
        }
    }

    /// These services with the job commands opening their connection through `dialer`.
    #[must_use]
    pub fn with_dialer(self, dialer: Dialer) -> Self {
        Self { dialer, ..self }
    }

    /// These services with the job commands' loops pausing on `job_waiter`.
    #[must_use]
    pub fn with_job_waiter(self, job_waiter: Arc<dyn js_client::jobs::Waiter>) -> Self {
        Self { job_waiter, ..self }
    }

    /// The program's own: a sleeping thread and tokio's signal handlers.
    pub fn system() -> Self {
        Self::new(Arc::new(ThreadWaiter), Box::new(signals::install))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the number and the name of each signal: they are what `job run` adds to 128
    /// for its exit status and prints when it stops waiting. It catches a number or a name
    /// swapped between signals.
    #[test]
    fn each_signal_has_its_number_and_its_name() {
        let all = [
            (ForwardedSignal::Hangup, 1, "SIGHUP"),
            (ForwardedSignal::Interrupt, 2, "SIGINT"),
            (ForwardedSignal::Terminate, 15, "SIGTERM"),
        ];
        for (signal, number, name) in all {
            assert_eq!((signal.number(), signal.name()), (number, name));
        }
    }

    /// Asserts that the program's waiter sleeps for exactly the interval it is given: with
    /// the sleep replaced by one that records what it was asked, each interval (zero, below
    /// a second, the two-second poll, minutes) reaches the sleep unchanged and once. It
    /// catches a waiter that ignores, shortens, rounds or repeats the interval. The real
    /// sleep is then called through the program's own waiter and services with a zero
    /// interval, so nothing waits and nothing is timed.
    #[test]
    fn the_thread_waiter_sleeps_for_the_interval_it_is_given() {
        let asked = [
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_secs(2),
            Duration::from_secs(90),
        ];
        for interval in asked {
            let mut slept = Vec::new();
            forward(interval, |got| slept.push(got));
            assert_eq!(slept, vec![interval]);
        }
        ThreadWaiter.wait(Duration::ZERO);
        Services::system().waiter.wait(Duration::ZERO);
    }
}
