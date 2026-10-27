//! The loop around a [`Session`]: tick, log, wait, and when the wait is cut short by a
//! stop request, drain and end.
//!
//! # The drain
//!
//! A stop request (SIGTERM or SIGINT) ends a session with no jobs with one heartbeat of
//! the phase `draining`, so the server leases the host no more.
//!
//! A session with jobs drains like this: the worker takes no new job from then on; the
//! next heartbeat says `draining` at once (the server then sends no start and books
//! nothing more on the host) and carries what the worker holds; the loop goes on
//! heartbeating, every interval, while a run or a result the server has not acknowledged
//! is left, so the results of jobs that end reach the server and are acknowledged. When
//! [`Drain::wait`] has passed it asks the worker to stop what still runs, and keeps
//! heartbeating for [`Drain::settle`] more. It ends, with status 0, when a heartbeat that
//! listed nothing has been sent, when that time has passed, or at a second stop request.
//! What is left then is not dropped: a run whose container still exists is adopted by the
//! next process of the agent, which lists it first, and its result is sent from there.

use std::io::Write;
use std::time::Duration;

use crate::session::{Drain, Fatal, Session, Step};

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
    /// The agent cannot go on: its job worker is gone.
    Failed(String),
}

/// Runs `session` until the server refuses the host for good or `sleeper` reports a stop.
/// Every note, and every line the session's worker wants printed, goes to `log` as one
/// line.
pub fn drive(session: &mut Session<'_>, sleeper: &mut dyn Sleeper, log: &mut dyn Write) -> Exit {
    loop {
        let step = session.tick();
        print(session, log);
        match step {
            Step::Fatal(fatal) => return Exit::Fatal(fatal),
            Step::Failed(why) => return Exit::Failed(why),
            Step::Wait { after, note } => {
                note.iter().for_each(|note| {
                    let _ = writeln!(log, "js-agent: {note}; next in {}ms", after.as_millis());
                });
                if sleeper.sleep(after) == Wake::Stop {
                    return stop(session, sleeper, log);
                }
            }
        }
    }
}

/// The lines the session's worker wants printed.
fn print(session: &Session<'_>, log: &mut dyn Write) {
    session.job_lines().iter().for_each(|line| {
        let _ = writeln!(log, "js-agent: {line}");
    });
}

/// The end after a stop request.
fn stop(session: &mut Session<'_>, sleeper: &mut dyn Sleeper, log: &mut dyn Write) -> Exit {
    match session.drain_plan() {
        Some(plan) => drain_jobs(session, plan, sleeper, log),
        None => {
            session.drain().iter().for_each(|note| {
                let _ = writeln!(log, "js-agent: {note}");
            });
            Exit::Stopped
        }
    }
}

/// The drain of a session with jobs, see the module documentation. Time passes only in
/// `sleeper`, and is counted as the waits asked for.
fn drain_jobs(
    session: &mut Session<'_>,
    plan: Drain,
    sleeper: &mut dyn Sleeper,
    log: &mut dyn Write,
) -> Exit {
    session.begin_drain();
    let end = plan.wait.saturating_add(plan.settle);
    let mut waited = Duration::ZERO;
    loop {
        // What this heartbeat lists is what the server then believes: the drain is done
        // when a heartbeat that listed nothing has been sent.
        let idle = !session.jobs_left();
        let step = session.tick();
        print(session, log);
        let after = match step {
            Step::Fatal(fatal) => return Exit::Fatal(fatal),
            Step::Failed(why) => return Exit::Failed(why),
            Step::Wait { after, note } => {
                note.iter().for_each(|note| {
                    let _ = writeln!(log, "js-agent: {note}");
                });
                after
            }
        };
        waited = waited.saturating_add(after);
        if idle || waited >= end {
            return leave(session, log);
        }
        if waited >= plan.wait {
            session.stop_jobs();
        }
        if sleeper.sleep(after) == Wake::Stop {
            return leave(session, log);
        }
    }
}

/// The end of a drain, with a line when something is left.
fn leave(session: &Session<'_>, log: &mut dyn Write) -> Exit {
    if session.jobs_left() {
        let _ = writeln!(
            log,
            "js-agent: draining: a run or a result is left; the next start adopts what \
             still exists"
        );
    }
    Exit::Stopped
}
