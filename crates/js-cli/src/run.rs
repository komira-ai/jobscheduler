//! `lease run`: hold a lease for as long as a command runs.

use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use js_client::Client;
use rustix::process::Signal;

use crate::api::{Freed, Lease, Routes};
use crate::cli::RunArgs;
use crate::command::{Running, watch_exit};
use crate::events::{Clock, Event, Events, Services, Waiter};
use crate::failure::{Failure, exit};
use crate::leases::{obtain, recover};
use crate::render::Render;
use crate::renewals::{self, Pause, Schedule};
use crate::tag::RunTag;

/// How many times a release is tried before it is called unconfirmed.
const RELEASE_ATTEMPTS: u32 = 3;

/// How long, through the waiter, between two tries of a release.
const RELEASE_PAUSE: Duration = Duration::from_secs(3);

/// Watches for signals, acquires the lease, runs the command with `JS_LEASE_ID` and
/// `JS_LEASE_HOST` in its environment, renews until it exits, ends what the command left
/// running, releases the lease however it ended and returns the command's status.
///
/// The signal handlers are installed before the lease is asked for, so a signal at any
/// point finds a handler: while the start is pending it releases the lease and ends the
/// run, and once the lease is held it is handed to the command like the others. The
/// lease is also released, and the command killed, when `js` itself panics. The start
/// carries a label no other start has; when its answer is lost, the list is searched
/// for that label and the lease it finds is released.
pub(crate) fn run(
    client: &Client,
    render: Render,
    args: &RunArgs,
    services: Services,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let Services {
        waiter,
        clock,
        signals,
    } = services;
    let events = Events::new();
    signals(events.signaller())
        .map_err(|why| Failure::Server(format!("cannot watch for signals: {why}")))?;
    let tag = RunTag::fresh()?;
    let interrupt = || events.interrupted();
    let started = obtain(client, &args.acquire, &*waiter, &interrupt, Some(&tag));
    let lease = started.inspect_err(|failure| look_for_lost(client, &tag, failure, err))?;
    let _ = err.write_all(render.held(&lease).as_bytes());
    let held = Held {
        client,
        id: &lease.id,
        armed: true,
    };
    let outcome = supervise(client, args, &waiter, &*clock, &events, &lease, err);
    let confirmed = held.give_back(&outcome, &*waiter, err);
    match (outcome, confirmed) {
        (Ok(status), false) => {
            let _ = writeln!(
                err,
                "js: the command ended with status {status}; exiting {} because the lease may still be held",
                exit::RELEASE_UNCONFIRMED
            );
            Ok(exit::RELEASE_UNCONFIRMED)
        }
        (outcome, _) => outcome,
    }
}

/// After a failed start: when its answer was lost, finds the lease it may have created
/// by this run's label and releases it, and says on stderr what came of it. Any other
/// failure of the start created nothing.
fn look_for_lost(client: &Client, tag: &RunTag, failure: &Failure, err: &mut dyn Write) {
    let Failure::Unconfirmed(_) = failure else {
        return;
    };
    let said = match recover(client, tag) {
        Ok(ids) => match ids.is_empty() {
            true => "js: no lease carries this run's label; the start created none\n".to_owned(),
            false => format!(
                "js: released lease {}, which the lost answer left behind\n",
                ids.join(", ")
            ),
        },
        Err(why) => {
            format!("js: could not look for the lease the lost answer may have left: {why}\n")
        }
    };
    let _ = err.write_all(said.as_bytes());
}

/// A lease this process holds and owes the server a release for. Dropping it before
/// [`Held::give_back`] (an unwind) releases the lease without a word: the output
/// streams may be what failed.
struct Held<'a> {
    client: &'a Client,
    id: &'a str,
    armed: bool,
}

impl Held<'_> {
    /// Releases the lease after the command, unless the server already dropped it, and
    /// says on stderr when that did not free it. Returns whether the lease is known to be
    /// free or in the server's hands: false when every try failed.
    fn give_back(
        mut self,
        outcome: &Result<u8, Failure>,
        waiter: &dyn Waiter,
        err: &mut dyn Write,
    ) -> bool {
        self.armed = false;
        match outcome {
            Err(Failure::LeaseLost(_)) => true,
            _ => release(self.client, self.id, waiter, err, RELEASE_ATTEMPTS),
        }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.client.release(self.id);
        }
    }
}

/// Starts the command and watches it, the renewals and the signals until it ends.
///
/// The renewals are timed by [`Schedule`] from the expiry of each answer of the server:
/// a renewal is tried at a third of the time left, and the lease is lost at the deadline,
/// one such period short of the expiry, when none has succeeded; a renewal that failed
/// without a refusal is tried again after a sixth of the period (at least a second) until
/// one succeeds or the deadline comes. A renewal runs on a
/// thread of its own, so a slow one blocks neither a signal nor the command's exit. A
/// lost lease (the server says it is gone, or the deadline comes) sends the command's
/// group SIGTERM and starts the grace period; the loop goes on forwarding signals, and
/// when the grace period passes with the command still running its group is sent
/// SIGKILL. Whatever ended the command, what it left running is ended before this
/// returns.
fn supervise(
    client: &Client,
    args: &RunArgs,
    waiter: &Arc<dyn Waiter>,
    clock: &dyn Clock,
    events: &Events,
    lease: &Lease,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let now = clock.now_ms();
    let mut schedule = Schedule::new(lease.expires_at_ms, now, args.renew_every);
    if schedule.lapsed(now) {
        return Err(Failure::Server(format!(
            "lease {} expires at {} ms, too soon after this machine's clock ({now} ms) to hold it: are the clocks of this machine and the server in step?",
            lease.id, lease.expires_at_ms
        )));
    }
    let mut running = Running::start(args, lease)?;
    watch_exit(running.group(), events.sender());
    let (renewing, id, ttl) = (client.clone(), lease.id.clone(), args.acquire.ttl);
    let answers = renewals::start(
        Arc::clone(waiter),
        schedule.pause(now),
        events.sender(),
        move |limit| renewing.renew_within(&id, ttl, limit),
    );
    let mut lost = None;
    loop {
        match events.next() {
            Event::Exited => {
                let status = running.reap();
                running.sweep(args.stop_grace, &**waiter, events);
                return lost.map_or(Ok(status), Err);
            }
            Event::Signal(signal) => running.signal(signal.os_signal()),
            Event::GraceOver => running.signal(Signal::KILL),
            Event::Renewed(result) => {
                match verdict(result, &mut schedule, clock.now_ms(), &lease.id, err) {
                    Verdict::Next(pause) => {
                        let _ = answers.send(pause);
                    }
                    Verdict::Lost(failure) => {
                        lost = Some(failure);
                        stop(&running, waiter, args.stop_grace, events.sender());
                    }
                }
            }
            Event::Deadline => {
                lost = Some(Failure::LeaseUnrenewed(lease.id.clone()));
                stop(&running, waiter, args.stop_grace, events.sender());
            }
        }
    }
}

/// What a renewal means for the lease.
enum Verdict {
    /// The lease is good, or worth trying again: wait like this.
    Next(Pause),
    /// The lease is lost: this ends the run.
    Lost(Failure),
}

/// What a renewal's result means at `now` (the injected clock, in milliseconds): the
/// next wait after a renewal that worked, or one that failed for a reason that is not the
/// server saying the lease is gone (it is reported and tried again after a short retry
/// interval, [`Schedule::retry`], unless the deadline
/// has come); the failure that ends the run when the server has dropped the lease or the
/// deadline has come.
fn verdict(
    result: Result<u64, Failure>,
    schedule: &mut Schedule,
    now: u64,
    id: &str,
    err: &mut dyn Write,
) -> Verdict {
    match result {
        Ok(expires_at_ms) => {
            schedule.renewed(expires_at_ms, now);
            Verdict::Next(schedule.pause(now))
        }
        Err(Failure::Refused(_)) => Verdict::Lost(Failure::LeaseLost(id.to_owned())),
        Err(other) => {
            let _ = writeln!(err, "js: could not renew lease {id}: {other}");
            match schedule.lapsed(now) {
                true => Verdict::Lost(Failure::LeaseUnrenewed(id.to_owned())),
                false => Verdict::Next(schedule.retry(now)),
            }
        }
    }
}

/// Tells the command's group to stop (SIGTERM) and starts the grace period, after which
/// the loop kills it.
fn stop(running: &Running, waiter: &Arc<dyn Waiter>, grace: Duration, events: Sender<Event>) {
    running.signal(Signal::TERM);
    start_grace(Arc::clone(waiter), grace, events);
}

/// Starts the grace period of a command told to stop: after `grace` through the waiter
/// it reports `GraceOver`. Detached like the renewing thread.
fn start_grace(waiter: Arc<dyn Waiter>, grace: Duration, events: Sender<Event>) {
    thread::spawn(move || {
        waiter.wait(grace);
        let _ = events.send(Event::GraceOver);
    });
}

/// Releases the lease after the command, trying again (after [`RELEASE_PAUSE`] through
/// the waiter) up to `left` times in all when the server cannot be reached or fails, and
/// says on stderr when that did not free it. True when the lease is free or the server
/// holds the stop (it retries it) or says it is not held; false when no try was answered.
fn release(client: &Client, id: &str, waiter: &dyn Waiter, err: &mut dyn Write, left: u32) -> bool {
    let said = match client.release(id) {
        Ok(Freed::Freed) => return true,
        Ok(Freed::Releasing) => {
            format!("js: lease {id} is releasing; the server retries the stop\n")
        }
        Err(why @ Failure::Refused(_)) => format!("js: could not release lease {id}: {why}\n"),
        Err(why) => match left > 1 {
            true => {
                waiter.wait(RELEASE_PAUSE);
                return release(client, id, waiter, err, left - 1);
            }
            false => {
                let said = format!(
                    "js: could not release lease {id}: {why}; gave up after {RELEASE_ATTEMPTS} tries\n"
                );
                let _ = err.write_all(said.as_bytes());
                return false;
            }
        },
    };
    let _ = err.write_all(said.as_bytes());
    true
}
