//! `lease run`: hold a lease for as long as a command runs.

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid};

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

/// Acquires the lease, runs the command with `JS_LEASE_ID` and `JS_LEASE_HOST` in its
/// environment, renews until it exits, releases the lease however it ended and returns
/// the command's status.
pub(crate) fn run(
    client: &Client,
    render: Render,
    args: &RunArgs,
    services: Services,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let lease = obtain(client, &args.acquire, &*services.waiter)?;
    let _ = err.write_all(render.held(&lease).as_bytes());
    let events = Events::new();
    let watching = (services.signals)(events.signaller());
    let outcome = watching
        .map_err(|why| Failure::Server(format!("cannot watch for signals: {why}")))
        .and_then(|()| supervise(client, args, services.waiter, &events, &lease, err));
    // A lease the server no longer holds needs no release; every other end gives it back.
    if !matches!(outcome, Err(Failure::LeaseLost(_))) {
        give_back(client, &lease.id, err);
    }
    outcome
}

/// Starts the command and watches it, the renewal period and the signals until it ends.
fn supervise(
    client: &Client,
    args: &RunArgs,
    waiter: Arc<dyn Waiter>,
    events: &Events,
    lease: &Lease,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let program = args.command.first().cloned().unwrap_or_default();
    let spawned = Command::new(&program)
        .args(args.command.iter().skip(1))
        .env("JS_LEASE_ID", &lease.id)
        .env("JS_LEASE_HOST", &lease.host)
        .spawn();
    let mut child = spawned.map_err(|why| Failure::NotStarted {
        program: program.to_string_lossy().into_owned(),
        why: why.to_string(),
    })?;
    let pid = Pid::from_child(&child);
    watch_exit(pid, events.sender());
    let every = args
        .renew_every
        .unwrap_or(args.acquire.ttl.unwrap_or(DEFAULT_TTL) / 3);
    let renewed = start_renewals(waiter, every, events.sender());
    loop {
        match events.next() {
            Event::Exited => return Ok(finish(&mut child)),
            Event::Signal(signal) => {
                let _ = kill_process(pid, signal.os_signal());
            }
            Event::Renew => {
                let renewal = client.renew(&lease.id, args.acquire.ttl);
                let _ = renewed.send(());
                match renewal {
                    Ok(_) => {}
                    Err(Failure::Refused(_)) => {
                        let _ = kill_process(pid, Signal::TERM);
                        let _ = child.wait();
                        return Err(Failure::LeaseLost(lease.id.clone()));
                    }
                    Err(other) => {
                        let _ = writeln!(err, "js: could not renew lease {}: {other}", lease.id);
                    }
                }
            }
        }
    }
}

/// Reaps the command and turns how it ended into the status `js` exits with: its own,
/// or 128 plus the signal that ended it.
fn finish(child: &mut Child) -> u8 {
    child
        .wait()
        .map_or(exit::SERVER, |status| status_code(&status))
}

fn status_code(status: &ExitStatus) -> u8 {
    let by_signal = status.signal().map(|signal| 128 + signal);
    u8::try_from(status.code().or(by_signal).unwrap_or(1)).unwrap_or(1)
}

/// Reports `Exited` once the command has exited, without reaping it: the status stays
/// for `finish`, and the process id cannot be reused before it is read.
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
