//! What a command waits on: the injected waiter, the signals and the events the
//! supervising loop of `lease run` reads.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use rustix::process::Signal;

use crate::signals;

/// How the client lets time pass. Nothing in `js` reads a clock or sleeps on its own: a
/// poll of a pending start and the pause between two renewals both go through a waiter,
/// so a test drives them and nothing in it waits for real.
pub trait Waiter: Send + Sync + 'static {
    /// Returns once `interval` has passed.
    fn wait(&self, interval: Duration);
}

/// The waiter of the program: the thread sleeps.
#[derive(Debug, Clone, Copy)]
pub struct ThreadWaiter;

impl Waiter for ThreadWaiter {
    fn wait(&self, interval: Duration) {
        std::thread::sleep(interval);
    }
}

/// A signal `lease run` hands on to its command.
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
    pub(crate) fn os_signal(self) -> Signal {
        match self {
            Self::Interrupt => Signal::INT,
            Self::Terminate => Signal::TERM,
            Self::Hangup => Signal::HUP,
        }
    }

    /// The signal's number, which `js` adds to 128 for its own exit status when a signal
    /// ends it before the command starts.
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

/// What the supervising loop of `lease run` reads.
#[derive(Debug)]
pub(crate) enum Event {
    /// The renewal period has passed.
    Renew,
    /// The command has exited (it is not reaped yet).
    Exited,
    /// `js` itself was sent a signal.
    Signal(ForwardedSignal),
    /// The command was told to stop because its lease is lost, and its grace period to
    /// end on its own has passed.
    GraceOver,
}

/// The inbox of the supervising loop.
pub(crate) struct Events {
    sender: Sender<Event>,
    receiver: Receiver<Event>,
}

impl Events {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = channel();
        Self { sender, receiver }
    }

    pub(crate) fn sender(&self) -> Sender<Event> {
        self.sender.clone()
    }

    /// The handle a signal source reports to.
    pub(crate) fn signaller(&self) -> Signaller {
        Signaller(self.sender())
    }

    /// The signal `js` was sent, if one has arrived, without blocking. Only the signal
    /// source reports to the inbox until the command is started, so a delivered event is
    /// a signal.
    pub(crate) fn interrupted(&self) -> Option<ForwardedSignal> {
        match self.receiver.try_recv() {
            Ok(Event::Signal(signal)) => Some(signal),
            _ => None,
        }
    }

    /// Blocks for the next event. This inbox holds a sender itself, so the channel never
    /// closes; the fallback only keeps the loop from spinning on an impossible error.
    pub(crate) fn next(&self) -> Event {
        self.receiver.recv().unwrap_or(Event::Exited)
    }
}

/// Where a signal source reports the signals `js` receives.
#[derive(Debug, Clone)]
pub struct Signaller(Sender<Event>);

impl Signaller {
    /// Reports `signal`; the supervising loop hands it on to the command. A report after
    /// the command has ended is dropped.
    pub fn deliver(&self, signal: ForwardedSignal) {
        let _ = self.0.send(Event::Signal(signal));
    }
}

/// How `lease run` learns of SIGINT, SIGTERM and SIGHUP: called once, before the lease
/// is asked for, with the handle to report them to. The program's source installs the
/// handlers (and only for `lease run`: the other commands keep the signals' default
/// behaviour); a signal that arrives while the lease is being obtained is held until
/// the run can act on it.
pub type SignalSource = Box<dyn FnOnce(Signaller) -> io::Result<()> + Send>;

/// The effects a command has beyond HTTP, injected so a test drives them.
pub struct Services {
    pub(crate) waiter: Arc<dyn Waiter>,
    pub(crate) signals: SignalSource,
}

impl Services {
    /// Services over this waiter and signal source.
    pub fn new(waiter: Arc<dyn Waiter>, signals: SignalSource) -> Self {
        Self { waiter, signals }
    }

    /// The program's own: a sleeping thread, and tokio's signal handlers.
    pub fn system() -> Self {
        Self::new(Arc::new(ThreadWaiter), Box::new(signals::install))
    }
}
