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
//! is left, so the results of jobs that end reach the server and are acknowledged. A run whose
//! post-job checks found the taint (`Blocked`) is not waited for: it has no container, and it
//! leaves only when the taint clears, which no wait brings about (see [`drain_waits`]). When
//! [`Drain::wait`] has passed it asks the worker to stop what still runs, and keeps
//! heartbeating for [`Drain::settle`] more. It ends, with status 0, when a heartbeat that
//! listed nothing has been sent, when that time has passed, or at a second stop request.
//! What is left then is not dropped: a run whose container still exists is adopted by the
//! next process of the agent, which lists it first, and its result is sent from there.
//!
//! # The handover
//!
//! A stop request that finds the handover file in the state root (the update hook writes it
//! before it stops the agent to restart it on another release, see
//! [`crate::lifecycle::handover`]) does not drain, in a session given the state root to look
//! in ([`Session::with_handover`]): the worker takes no new job and the next heartbeat says
//! `draining` at once, as in a drain; then, after each heartbeat, the worker is asked to park
//! ([`Worker::park`](crate::Worker::park)), which it does only once no start, no stop and no
//! finished job's cleanup is under way. Parked, it does nothing more, and the loop ends with
//! status 0, every run left as it is for the next process to adopt and every result not yet
//! acknowledged kept in the state root for it to send. Nothing is stopped and no result is
//! waited for. When [`Drain::wait`] passes before the worker parks, or at a second stop
//! request, the loop ends all the same and says so: what was under way is then the next
//! process's, as after a crash. A file that is there but not of its form is not a request:
//! the stop drains, and the log says why.
//!
//! # The other ends
//!
//! The agent never leaves a container without somebody to enforce its wall limit if it can
//! help it:
//!
//! - **The server refuses the host for good** ([`Exit::Fatal`], for example a revoked
//!   token): no heartbeat can be sent, so there is no drain. The worker is closed, asked to
//!   stop every run, and waited for until none runs or the worker has ended, for the stop
//!   grace and a minute; what it did not stop is stopped through the runtime directly, as
//!   below. The exit status is the
//!   refusal's. The results of the stopped runs are not sent and the server would not take
//!   them, so they may be lost.
//! - **The job worker is gone or stalled** ([`Exit::Failed`]): the worker cannot be asked,
//!   so the agent makes a runtime of its own, lists its containers by their label and stops
//!   them, each call bounded by the runner's limit and the stop grace, the whole bounded to
//!   ten minutes (or the stop grace and a minute, when that is longer); the worker is
//!   closed first, so a worker that wakes from a stall starts nothing more. A container that could not be stopped is named in the log. The agent then relies
//!   on its supervisor (for example systemd `Restart=`) to start a new process, which
//!   adopts what still exists.
//! - Without `--container-bin` there is no worker and both ends are as they were.

use std::io::Write;
use std::time::Duration;

use crate::hold_cordon::Keeper;
use crate::lifecycle::handover::Handover;
use crate::session::{Drain, Fatal, Note, Session, Step};
use crate::worker::{Parked, Snapshot};

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// The time passed.
    Elapsed,
    /// The process was asked to stop (SIGTERM or SIGINT).
    Stop,
    /// The bell rang: the job worker has news for the server ([`Sleeper::sleep_or_bell`]).
    Bell,
}

/// Lets time pass, and tells the loop when the process is asked to stop. The real one
/// waits on the signal thread's channel; a test counts the waits and decides when to stop.
pub trait Sleeper {
    /// Waits `duration`, or until a stop request arrives, whichever is first. A stop
    /// request that arrived earlier ends the wait at once.
    fn sleep(&mut self, duration: Duration) -> Wake;

    /// [`Sleeper::sleep`], also ended by the bell ([`Wake::Bell`]): rung since the last
    /// wait that it ended, it ends this one at once. A stop request comes before the bell.
    /// The default has no bell and only sleeps.
    fn sleep_or_bell(&mut self, duration: Duration) -> Wake {
        self.sleep(duration)
    }

    /// The bell [`Sleeper::sleep_or_bell`] wakes on, for the job worker to ring; none for a
    /// sleeper without one (the default).
    fn bell(&self) -> Option<crate::lifecycle::Wake> {
        None
    }
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
    drive_holding(session, sleeper, log, None)
}

/// [`drive`], stepping `keeper` (the cordon of a held host, see [`crate::hold_cordon`]) after
/// each exchange and before its wait, and telling it when the exchange was an accepted
/// hello. The keeper's lines go to `log` too.
pub fn drive_holding(
    session: &mut Session<'_>,
    sleeper: &mut dyn Sleeper,
    log: &mut dyn Write,
    mut keeper: Option<&mut Keeper<'_>>,
) -> Exit {
    loop {
        let step = session.tick();
        print(session, log);
        match step {
            Step::Fatal(fatal) => return refused(session, fatal, log),
            Step::Failed(why) => return failed(session, why, log),
            Step::Wait { after, note } => {
                note.iter().for_each(|note| {
                    let _ = writeln!(log, "js-agent: {note}; next in {}ms", after.as_millis());
                });
                let hello = matches!(note, Some(Note::Registered { .. }));
                let held = keeper.as_deref_mut().map(|keeper| keeper.step(hello));
                held.into_iter().flatten().for_each(|line| {
                    let _ = writeln!(log, "js-agent: {line}");
                });
                // The wait after an accepted heartbeat ends early when the worker has news
                // for the server (a job that runs, or ended); no other wait does.
                let woken = match session.beats_on_news() {
                    true => sleeper.sleep_or_bell(after),
                    false => sleeper.sleep(after),
                };
                if woken == Wake::Stop {
                    return stop(session, sleeper, log);
                }
            }
        }
    }
}

/// The end after a refusal for good: the jobs are stopped first, see the module
/// documentation.
fn refused(session: &Session<'_>, fatal: Fatal, log: &mut dyn Write) -> Exit {
    session.wind_down_jobs().iter().for_each(|line| {
        let _ = writeln!(log, "js-agent: {line}");
    });
    print(session, log);
    Exit::Fatal(fatal)
}

/// The end after the worker's end: the containers are stopped without the worker, see the
/// module documentation.
fn failed(session: &Session<'_>, why: String, log: &mut dyn Write) -> Exit {
    session.rescue_jobs().iter().for_each(|line| {
        let _ = writeln!(log, "js-agent: {line}");
    });
    Exit::Failed(why)
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
        Some(plan) => match session.handover() {
            Handover::Asked => hand_over(session, plan, sleeper, log),
            Handover::Absent => drain_jobs(session, plan, sleeper, log),
            Handover::Refused(why) => {
                let _ = writeln!(
                    log,
                    "js-agent: the handover file is not used ({why}): the agent drains"
                );
                drain_jobs(session, plan, sleeper, log)
            }
        },
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
        let idle = !session.drain_left();
        let after = match beat(session, log) {
            Ok(after) => after,
            Err(exit) => return exit,
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

/// The handover of a session with jobs, see the module documentation. Time passes only in
/// `sleeper`, and is counted as the waits asked for.
fn hand_over(
    session: &mut Session<'_>,
    plan: Drain,
    sleeper: &mut dyn Sleeper,
    log: &mut dyn Write,
) -> Exit {
    session.begin_drain();
    let _ = writeln!(
        log,
        "js-agent: handing over: no new job is taken, and the runs are left to the next \
         process once no start, stop or check is under way"
    );
    let mut waited = Duration::ZERO;
    loop {
        let after = match beat(session, log) {
            Ok(after) => after,
            Err(exit) => return exit,
        };
        // Asked after the heartbeat, so the worker has the reply's stops and acknowledgements
        // before it looks.
        if session.park_jobs() != Some(Parked::Busy) {
            let _ = writeln!(
                log,
                "js-agent: handed over: every run is left as it is to the next process"
            );
            return Exit::Stopped;
        }
        waited = waited.saturating_add(after);
        if waited >= plan.wait || sleeper.sleep(after) == Wake::Stop {
            let _ = writeln!(
                log,
                "js-agent: handing over: a start, a stop or a check is still under way and is \
                 left to the next process"
            );
            return Exit::Stopped;
        }
    }
}

/// One heartbeat of a drain or a handover, and the lines it makes: the wait it asks for, or the
/// end a refusal for good or the end of the worker makes (see the module documentation).
fn beat(session: &mut Session<'_>, log: &mut dyn Write) -> Result<Duration, Exit> {
    let step = session.tick();
    print(session, log);
    match step {
        Step::Fatal(fatal) => Err(refused(session, fatal, log)),
        Step::Failed(why) => Err(failed(session, why, log)),
        Step::Wait { after, note } => {
            note.iter().for_each(|note| {
                let _ = writeln!(log, "js-agent: {note}");
            });
            Ok(after)
        }
    }
}

/// The end of a drain, with a line when something is left.
fn leave(session: &Session<'_>, log: &mut dyn Write) -> Exit {
    if session.drain_left() {
        let _ = writeln!(
            log,
            "js-agent: draining: a run or a result is left; the next start adopts what \
             still exists"
        );
    } else if session.jobs_left() {
        let _ = writeln!(
            log,
            "js-agent: draining: only the runs whose checks found the taint are left; they \
             leave with the taint, which the next start reads from the marker again"
        );
    }
    Exit::Stopped
}

/// Whether a drain waits on what `snapshot` holds: a run or a job that is not `Blocked`
/// ([`Snapshot::blocked`]), a result or a retired job id the server has not acknowledged. A
/// `Blocked` run's container and scratch directory are gone and it leaves only when the taint
/// clears, which no wait of the drain brings about: a tainted agent that holds nothing else
/// leaves at once instead of heartbeating `draining` for the whole `--drain-wait`.
pub(crate) fn drain_waits(snapshot: &Snapshot) -> bool {
    let waits = |id: &String| !snapshot.blocked.contains(id);
    let run = snapshot.held.iter().any(|run| waits(&run.run_id));
    let job = snapshot
        .jobs
        .iter()
        .any(|job| job.result.is_some() || waits(&job.job_id));
    run || job || !snapshot.results.is_empty() || !snapshot.retired_jobs.is_empty()
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
