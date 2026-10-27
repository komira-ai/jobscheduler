//! `lease run`: hold a lease for as long as a command runs.

use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};

use crate::api::{Freed, Lease};
use crate::cli::RunArgs;
use crate::client::Client;
use crate::events::{Event, Events, Services, Waiter};
use crate::failure::{Failure, exit};
use crate::leases::obtain;
use crate::render::Render;

/// The server's default ttl, a third of which is the renewal period when `--ttl` is not
/// given.
const DEFAULT_TTL: Duration = Duration::from_secs(600);

/// Watches for signals, acquires the lease, runs the command with `JS_LEASE_ID` and
/// `JS_LEASE_HOST` in its environment, renews until it exits, releases the lease however
/// it ended and returns the command's status.
///
/// The signal handlers are installed before the lease is asked for, so a signal at any
/// point finds a handler: while the start is pending it releases the lease and ends the
/// run, and once the lease is held it is handed to the command like the others. The
/// lease is also released, and the command killed, when `js` itself panics.
pub(crate) fn run(
    client: &Client,
    render: Render,
    args: &RunArgs,
    services: Services,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let events = Events::new();
    (services.signals)(events.signaller())
        .map_err(|why| Failure::Server(format!("cannot watch for signals: {why}")))?;
    let interrupt = || events.interrupted();
    let lease = obtain(client, &args.acquire, &*services.waiter, &interrupt)?;
    let _ = err.write_all(render.held(&lease).as_bytes());
    let held = Held {
        client,
        id: &lease.id,
        armed: true,
    };
    let outcome = supervise(client, args, services.waiter, &events, &lease, err);
    held.give_back(&outcome, err);
    outcome
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
    /// says on stderr when that did not free it.
    fn give_back(mut self, outcome: &Result<u8, Failure>, err: &mut dyn Write) {
        self.armed = false;
        if !matches!(outcome, Err(Failure::LeaseLost(_))) {
            give_back(self.client, self.id, err);
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

/// The command, running in a process group of its own so that a signal reaches what it
/// started as well. Dropping it while it still runs (an unwind) kills the group and
/// reaps the command.
struct Running {
    child: Child,
    group: Pid,
}

impl Running {
    /// Starts `args.command` with the lease in its environment.
    fn start(args: &RunArgs, lease: &Lease) -> Result<Self, Failure> {
        let program = args.command.first().cloned().unwrap_or_default();
        let spawned = Command::new(&program)
            .args(args.command.iter().skip(1))
            .env("JS_LEASE_ID", &lease.id)
            .env("JS_LEASE_HOST", &lease.host)
            .process_group(0)
            .spawn();
        let child = spawned.map_err(|why| Failure::NotStarted {
            program: program.to_string_lossy().into_owned(),
            why: why.to_string(),
        })?;
        let group = Pid::from_child(&child);
        Ok(Self { child, group })
    }

    /// Sends `signal` to the command's whole process group.
    fn signal(&self, signal: Signal) {
        let _ = kill_process_group(self.group, signal);
    }

    /// Reaps the command and turns how it ended into the status `js` exits with: its
    /// own, or 128 plus the signal that ended it.
    fn reap(&mut self) -> u8 {
        self.child
            .wait()
            .map_or(exit::SERVER, |status| status_code(&status))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.signal(Signal::KILL);
            let _ = self.child.wait();
        }
    }
}

/// Starts the command and watches it, the renewal period and the signals until it ends.
///
/// A lost lease (a renewal finds it gone, or no renewal has succeeded for as long as its
/// ttl, counted in the periods the waiter was asked for) sends the command SIGTERM and
/// starts the grace period; the loop goes on forwarding signals, and when the grace
/// period passes with the command still running it is sent SIGKILL.
fn supervise(
    client: &Client,
    args: &RunArgs,
    waiter: Arc<dyn Waiter>,
    events: &Events,
    lease: &Lease,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let mut running = Running::start(args, lease)?;
    watch_exit(running.group, events.sender());
    let ttl = args.acquire.ttl.unwrap_or(DEFAULT_TTL);
    let every = args.renew_every.unwrap_or(ttl / 3);
    let renewed = start_renewals(Arc::clone(&waiter), every, events.sender());
    let mut unrenewed = Duration::ZERO;
    let mut lost = None;
    loop {
        match events.next() {
            Event::Exited => {
                let status = running.reap();
                return lost.map_or(Ok(status), Err);
            }
            Event::Signal(signal) => running.signal(signal.os_signal()),
            Event::GraceOver => running.signal(Signal::KILL),
            Event::Renew => {
                let renewal = client.renew(&lease.id, args.acquire.ttl);
                match renewal_verdict(renewal, &mut unrenewed, every, ttl, &lease.id, err) {
                    None => {
                        let _ = renewed.send(());
                    }
                    Some(failure) => {
                        // The renewal clock is not answered, so it renews no more.
                        lost = Some(failure);
                        running.signal(Signal::TERM);
                        start_grace(Arc::clone(&waiter), args.stop_grace, events.sender());
                    }
                }
            }
        }
    }
}

/// What a renewal means for the lease: `None` while it is good (a failure that is not
/// the server saying the lease is gone is reported and tried again), the failure that
/// ends the run when the server has dropped it or `ttl` has gone by, in periods of
/// `every`, since a renewal last succeeded.
fn renewal_verdict(
    renewal: Result<u64, Failure>,
    unrenewed: &mut Duration,
    every: Duration,
    ttl: Duration,
    id: &str,
    err: &mut dyn Write,
) -> Option<Failure> {
    match renewal {
        Ok(_) => {
            *unrenewed = Duration::ZERO;
            None
        }
        Err(Failure::Refused(_)) => Some(Failure::LeaseLost(id.to_owned())),
        Err(other) => {
            *unrenewed += every;
            let _ = writeln!(err, "js: could not renew lease {id}: {other}");
            (*unrenewed >= ttl).then(|| Failure::LeaseUnrenewed(id.to_owned()))
        }
    }
}

fn status_code(status: &ExitStatus) -> u8 {
    let by_signal = status.signal().map(|signal| 128 + signal);
    u8::try_from(status.code().or(by_signal).unwrap_or(1)).unwrap_or(1)
}

/// Reports `Exited` once the command has exited, without reaping it: the status stays
/// for `reap`, and the process id cannot be reused before it is read.
fn watch_exit(pid: Pid, events: Sender<Event>) {
    thread::spawn(move || {
        let _ = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        );
        let _ = events.send(Event::Exited);
    });
}

/// Starts the renewal clock: every `every` through the waiter it reports `Renew`, and it
/// starts the next period only when the loop answers on the returned sender that the
/// renewal is done, so periods are counted between renewals and never pile up. The
/// thread ends when the loop is gone; it is detached, so a waiter that is still
/// waiting then is left to the end of the process.
fn start_renewals(waiter: Arc<dyn Waiter>, every: Duration, events: Sender<Event>) -> Sender<()> {
    let (done, renewed) = channel::<()>();
    thread::spawn(move || {
        std::iter::repeat_with(|| {
            waiter.wait(every);
            events
                .send(Event::Renew)
                .ok()
                .and_then(|()| renewed.recv().ok())
        })
        .map_while(|answered| answered)
        .for_each(drop);
    });
    done
}

/// Starts the grace period of a command told to stop: after `grace` through the waiter
/// it reports `GraceOver`. Detached like the renewal clock.
fn start_grace(waiter: Arc<dyn Waiter>, grace: Duration, events: Sender<Event>) {
    thread::spawn(move || {
        waiter.wait(grace);
        let _ = events.send(Event::GraceOver);
    });
}

/// Releases the lease after the command, and says on stderr when that did not free it.
fn give_back(client: &Client, id: &str, err: &mut dyn Write) {
    let said = match client.release(id) {
        Ok(Freed::Freed) => return,
        Ok(Freed::Releasing) => {
            format!("js: lease {id} is releasing; the server retries the stop\n")
        }
        Err(why) => format!("js: could not release lease {id}: {why}\n"),
    };
    let _ = err.write_all(said.as_bytes());
}
