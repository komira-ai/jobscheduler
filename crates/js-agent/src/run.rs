//! The loop around a [`Session`]: tick, log, wait, and when the wait is cut short by a
//! stop request, announce the drain and end.

use std::io::Write;
use std::time::Duration;

use crate::session::{Fatal, Session, Step};

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// The time passed.
    Elapsed,
    /// The process was asked to stop (SIGTERM or SIGINT).
    Stop,
}

/// Lets time pass, and tells the loop when the process is asked to stop. The real one
/// waits on the signal thread's channel; a test counts the waits and decides when to stop.
pub trait Sleeper {
    /// Waits `duration`, or until a stop request arrives, whichever is first. A stop
    /// request that arrived earlier ends the wait at once.
    fn sleep(&mut self, duration: Duration) -> Wake;
}

/// How the loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// A stop request: the host announced that it drains.
    Stopped,
    /// The server refused the host for good.
    Fatal(Fatal),
}

/// Runs `session` until the server refuses the host for good or `sleeper` reports a stop.
/// Every note goes to `log` as one line.
pub fn drive(session: &mut Session<'_>, sleeper: &mut dyn Sleeper, log: &mut dyn Write) -> Exit {
    loop {
        match session.tick() {
            Step::Fatal(fatal) => return Exit::Fatal(fatal),
            Step::Wait { after, note } => {
                note.iter().for_each(|note| {
                    let _ = writeln!(log, "js-agent: {note}; next in {}ms", after.as_millis());
                });
                if sleeper.sleep(after) == Wake::Stop {
                    session.drain().iter().for_each(|note| {
                        let _ = writeln!(log, "js-agent: {note}");
                    });
                    return Exit::Stopped;
                }
            }
        }
    }
}
