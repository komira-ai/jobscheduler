//! The window: cordon, renew, watch for idle, lift. Everything outside the program (the
//! server, time, the stop request, standard output) is given, so the whole of it runs in a
//! test with no wait.

use std::io::Write;
use std::time::Duration;

use super::Pace;
use crate::Wake;
use crate::own_host::{HostReading, Lease, LeaseError};

/// How often the host is read while it is not yet idle, so that a script waiting for the
/// `idle` line is told within this long of the last lease ending, whatever the renewal
/// interval is.
pub const IDLE_POLL: Duration = Duration::from_secs(5);

/// Codes that mean the server did not accept the credential, as opposed to the request.
const CREDENTIAL_CODES: [&str; 4] = [
    "host-credential-required",
    "join-token-not-admitted",
    "join-token-expired",
    "http-401",
];

/// What to hold and for how long.
#[derive(Clone, Copy, Debug)]
pub struct Plan<'a> {
    /// The host to cordon. Every reply of the server must name it.
    pub host: &'a str,
    /// The length of each cordon request, in seconds (1 to `MAX_CORDON_SECS`).
    pub lease_secs: u64,
    /// How long to hold the cordon.
    pub window: Duration,
}

/// How the window ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The cordon was held to the end of the window or a stop request, and lifted (or had
    /// lapsed).
    Done,
    /// The host was cordoned already when the verb asked: it is another holder's, and it is
    /// left as it is.
    OtherHolder,
    /// The server or the credential failed; the text says which, and never holds the
    /// credential.
    Failed(String),
}

/// What went wrong after the cordon was placed.
struct Trouble {
    /// What a person is told.
    text: String,
    /// A reply named another host: nothing is lifted. Any other trouble is a server that did
    /// not answer as asked, and the cordon is lifted if it can be.
    identity: bool,
    /// The verb's cordon lapsed and another holder's took its place: nothing is lifted, and
    /// the window ends as another holder's cordon.
    other_holder: bool,
}

/// Whether the host reads as one that nothing runs on and nothing is about to: no lease, not
/// releasing, and not set aside. It is what a script's drain gate checks on `GET /v1/hosts`.
fn idle(host: &HostReading) -> bool {
    [
        host.lease.is_none(),
        host.leases == 0,
        !host.releasing,
        host.mark.is_none(),
    ]
    .into_iter()
    .all(|ok| ok)
}

/// What a person is told of a failed call: whose fault it is, and the server's word.
fn explain(what: &str, why: &LeaseError) -> String {
    match why {
        LeaseError::UnknownHost => format!(
            "server error: {what}: the server holds no such host (it has not said hello since the server started)"
        ),
        LeaseError::Refused(code) if CREDENTIAL_CODES.contains(&code.as_str()) => {
            format!("credential error: {what}: the server refused the credential ({code})")
        }
        LeaseError::Refused(code) => format!("server error: {what}: the server refused ({code})"),
        LeaseError::Unreachable(text) => format!("server error: {what}: {text}"),
    }
}

/// A server that did not answer as asked, for `what` it was asked.
fn trouble(what: &str, why: &LeaseError) -> Trouble {
    Trouble {
        text: explain(what, why),
        identity: false,
        other_holder: false,
    }
}

/// The state of one window.
struct Window<'a> {
    lease: &'a dyn Lease,
    plan: &'a Plan<'a>,
    out: &'a mut dyn Write,
    said_idle: bool,
    /// Until when, on the verb's clock, its own cordon surely holds: the time its last cordon
    /// request was sent plus `lease_secs`. A renewal sent later may find another holder's.
    held_until: Duration,
}

impl Window<'_> {
    /// Prints one line. A closed standard output cannot be reported anywhere.
    fn say(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
    }

    /// Whether `reading` is of the host that was cordoned.
    fn named(&self, reading: &HostReading) -> Result<(), Trouble> {
        match reading.id == self.plan.host {
            true => Ok(()),
            false => Err(Trouble {
                text: format!(
                    "server error: a reply names host {:?}, not {:?}; nothing is lifted",
                    reading.id, self.plan.host
                ),
                identity: true,
                other_holder: false,
            }),
        }
    }

    /// Takes what a reply says of the host: it must be ours, and the first time it reads idle
    /// the idle line is printed.
    fn observe(&mut self, reading: &HostReading) -> Result<(), Trouble> {
        self.named(reading)?;
        if idle(reading) && !self.said_idle {
            self.said_idle = true;
            self.say(&format!("idle host={}", self.plan.host));
        }
        Ok(())
    }

    /// Says the cordon this verb holds, from a reply that placed or renewed it.
    fn say_cordoned(&mut self, host: &HostReading) {
        let left = host.cordon.as_ref().and_then(|cordon| cordon.expires_in_ms);
        self.say(&format!(
            "cordoned host={} expires_in_ms={} leases={}",
            self.plan.host,
            left.unwrap_or(0),
            host.leases
        ));
    }

    /// Renews the cordon, with the request sent at `asked` on the verb's clock. If the verb's
    /// cordon has lapsed by then (the process was stopped for longer than `lease_secs`), the
    /// reply says again whose the cordon is: one that was there already is another holder's,
    /// and is kept; one the renewal placed is the verb's again.
    fn renew(&mut self, asked: Duration) -> Result<(), Trouble> {
        let lapsed = asked >= self.held_until;
        let renewed = self.lease.cordon(self.plan.lease_secs);
        let renewed = renewed.map_err(|why| trouble("cannot renew the cordon", &why))?;
        self.named(&renewed.host)?;
        if lapsed {
            if renewed.already_cordoned {
                return Err(Trouble {
                    text: format!(
                        "the cordon of host {:?} lapsed and another holder cordoned it; nothing is lifted",
                        self.plan.host
                    ),
                    identity: true,
                    other_holder: true,
                });
            }
            self.say_cordoned(&renewed.host);
        }
        self.held_until = asked + Duration::from_secs(self.plan.lease_secs);
        self.observe(&renewed.host)
    }

    /// Reads the host.
    fn poll(&mut self) -> Result<(), Trouble> {
        let read = self.lease.host();
        let read = read.map_err(|why| trouble("cannot read the host", &why))?;
        self.observe(&read)
    }

    /// Holds the cordon from `begun` until the window has passed or a stop request comes.
    fn hold(&mut self, pace: &mut dyn Pace, begun: Duration) -> Result<(), Trouble> {
        let end = begun + self.plan.window;
        let every = Duration::from_millis(self.plan.lease_secs * 1000 / 3);
        let (mut renew_at, mut poll_at) = (begun + every, begun + IDLE_POLL);
        loop {
            let now = pace.now();
            if now >= end {
                return Ok(());
            }
            if now >= renew_at {
                self.renew(now)?;
                renew_at = now + every;
                continue;
            }
            if !self.said_idle && now >= poll_at {
                self.poll()?;
                poll_at = now + IDLE_POLL;
                continue;
            }
            let next = match self.said_idle {
                true => renew_at,
                false => renew_at.min(poll_at),
            };
            if pace.wait(next.min(end).saturating_sub(now)) == Wake::Stop {
                return Ok(());
            }
        }
    }

    /// Lifts the cordon this verb placed.
    fn lift(&mut self) -> Result<(), Trouble> {
        let done = self.lease.uncordon();
        let done = done.map_err(|why| trouble("cannot lift the cordon", &why))?;
        self.named(&done.host)?;
        let word = match done.lifted {
            true => "lifted",
            false => "lapsed",
        };
        self.say(&format!("{word} host={}", self.plan.host));
        Ok(())
    }
}

/// Cordons the host, holds the cordon for the window and lifts it, and prints a line for
/// each state change on `out`.
///
/// The cordon is lifted only if this call placed it (the first reply said `already_cordoned:
/// false`) and every reply named the host that was asked for: the server cannot say who
/// placed a cordon, and an uncordon lifts any. Time passes only in `pace`, which also tells
/// of a stop request; a stop lifts the cordon like the end of the window does.
pub fn hold(
    lease: &dyn Lease,
    pace: &mut dyn Pace,
    plan: &Plan<'_>,
    out: &mut dyn Write,
) -> Outcome {
    let asked = pace.now();
    let mut window = Window {
        lease,
        plan,
        out,
        said_idle: false,
        held_until: asked + Duration::from_secs(plan.lease_secs),
    };
    let first = match lease.cordon(plan.lease_secs) {
        Ok(first) => first,
        Err(why) => return Outcome::Failed(explain("cannot cordon the host", &why)),
    };
    if let Err(trouble) = window.named(&first.host) {
        return Outcome::Failed(trouble.text);
    }
    if first.already_cordoned {
        window.say(&format!("kept host={} reason=already_cordoned", plan.host));
        return Outcome::OtherHolder;
    }
    window.say_cordoned(&first.host);
    let begun = pace.now();
    let held = window
        .observe(&first.host)
        .and_then(|()| window.hold(pace, begun));
    let lifted = match &held {
        Err(trouble) if trouble.identity => Ok(()),
        _ => window.lift(),
    };
    match held.and(lifted) {
        Ok(()) => Outcome::Done,
        Err(trouble) if trouble.other_holder => {
            window.say(&format!("kept host={} reason=taken_after_lapse", plan.host));
            Outcome::OtherHolder
        }
        Err(trouble) => Outcome::Failed(trouble.text),
    }
}
