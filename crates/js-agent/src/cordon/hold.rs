//! The window: cordon, renew, watch for idle, lift. Everything outside the program (the
//! server, time, the stop request, standard output) is given, so the whole of it runs in a
//! test with no wait.
//!
//! Every call names the holder of the verb, so the server says whose the cordon is and the
//! verb keeps no clock of its own to guess it: the first call places the cordon, or finds
//! another holder's and leaves it (`kept ... reason=already_cordoned`, the cordon is not
//! extended), or with a take-over makes the cordon in force the verb's. A renewal that finds
//! another holder's cordon (ours lapsed and another took its place, or an operator took it
//! over) says `kept ... reason=taken_over`; one that places the cordon again says `cordoned`
//! again. The lift names the holder too, so it ends only the verb's own cordon.

use std::io::Write;
use std::time::Duration;

use super::Pace;
use crate::Wake;
use crate::own_host::{HeldLease, HostReading, LeaseError};
use js_types::CordonHolder;

/// How often the host is read while it is not yet idle, so that a script waiting for the
/// `idle` line is told within this long of the last lease ending, whatever the renewal
/// interval is.
pub const IDLE_POLL: Duration = Duration::from_secs(5);

/// Codes that mean the server did not accept the host certificate as the host's, as opposed to
/// the request: `host-certificate-required` (none was presented), `certificate-host-mismatch`
/// (the certificate names another host), and the bare `http-401` of an answer with no code. The
/// update hook's first-install `rollback` reads them too: a host whose certificate the server
/// does not take as the host's is given no work.
pub(crate) const CREDENTIAL_CODES: [&str; 3] = [
    "host-certificate-required",
    "certificate-host-mismatch",
    "http-401",
];

/// How the text of a failure the server's refusal of the credential caused begins.
pub(super) const CREDENTIAL_ERROR: &str = "credential error: ";

/// What to hold and for how long.
#[derive(Clone, Copy, Debug)]
pub struct Plan<'a> {
    /// The host to cordon. Every reply of the server must name it.
    pub host: &'a str,
    /// The length of each cordon request, in seconds (1 to `MAX_CORDON_SECS`).
    pub lease_secs: u64,
    /// How long to hold the cordon.
    pub window: Duration,
    /// The holder every call names: the cordon is this verb's when the server says it is
    /// held by it.
    pub holder: &'a CordonHolder,
    /// Take the cordon in force over on the first call, whoever holds it, and hold and lift
    /// it as the verb's own.
    pub take_over: bool,
}

/// How the window ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The cordon was held to the end of the window or a stop request, and lifted (or had
    /// lapsed).
    Done,
    /// The cordon in force is another holder's, found when the verb asked or found again at a
    /// renewal or the lift: it is left as it is, and not extended.
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
    /// The `kept` line to print when the cordon in force turned out to be another holder's;
    /// the window then ends as another holder's cordon.
    kept: Option<String>,
}

/// Whether the host reads as one that nothing runs on and nothing is about to: no lease, not
/// releasing, and not set aside. It is what a script's drain gate checks on `GET /v1/hosts`,
/// and what the update hook's `drain` waits for.
pub(crate) fn idle(host: &HostReading) -> bool {
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
            format!("{CREDENTIAL_ERROR}{what}: the server refused the host certificate ({code})")
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
        kept: None,
    }
}

/// The holder a reading names for its cordon, or `-` when it names none.
fn holder_of(host: &HostReading) -> &str {
    host.cordon
        .as_ref()
        .and_then(|cordon| cordon.holder.as_deref())
        .unwrap_or("-")
}

/// The state of one window.
struct Window<'a> {
    lease: &'a dyn HeldLease,
    plan: &'a Plan<'a>,
    out: &'a mut dyn Write,
    said_idle: bool,
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
                kept: None,
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
            "cordoned host={} holder={} expires_in_ms={} leases={}",
            self.plan.host,
            self.plan.holder,
            left.unwrap_or(0),
            host.leases
        ));
    }

    /// The `kept` line for a cordon that turned out to be another holder's, as `host` reads it:
    /// `reason` is `taken_over`, or `already_cordoned` (which also says when it lapses).
    fn kept_line(&self, reason: &str, host: &HostReading) -> String {
        let mut line = format!(
            "kept host={} reason={reason} holder={}",
            self.plan.host,
            holder_of(host)
        );
        if reason == ALREADY {
            let left = host.cordon.as_ref().and_then(|cordon| cordon.expires_in_ms);
            line.push_str(&format!(" expires_in_ms={}", left.unwrap_or(0)));
        }
        line
    }

    /// The trouble of a cordon that is another holder's now: nothing is lifted.
    fn taken_over(&self, host: &HostReading) -> Trouble {
        Trouble {
            text: format!(
                "the cordon of host {:?} is held by another holder; nothing is lifted",
                self.plan.host
            ),
            identity: true,
            kept: Some(self.kept_line(TAKEN_OVER, host)),
        }
    }

    /// Renews the cordon, naming the holder and never taking over. The reply says whose the
    /// cordon is: another holder's (ours lapsed and another's took its place, or an operator
    /// took it over) is kept; one the renewal placed again (ours lapsed and nobody took its
    /// place) is the verb's, and is said again.
    fn renew(&mut self) -> Result<(), Trouble> {
        let renewed = self
            .lease
            .cordon_held(self.plan.lease_secs, self.plan.holder, false);
        let renewed = renewed.map_err(|why| trouble("cannot renew the cordon", &why))?;
        self.named(&renewed.host)?;
        if renewed.held_by_other {
            return Err(self.taken_over(&renewed.host));
        }
        if !renewed.already_cordoned {
            self.say_cordoned(&renewed.host);
        }
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
                self.renew()?;
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

    /// Lifts the cordon this verb holds: the lift names the holder, so the server leaves
    /// another holder's cordon in force and says so.
    fn lift(&mut self) -> Result<(), Trouble> {
        let done = self.lease.uncordon_held(self.plan.holder);
        let done = done.map_err(|why| trouble("cannot lift the cordon", &why))?;
        self.named(&done.host)?;
        if !done.lifted && done.held_by_other {
            return Err(self.taken_over(&done.host));
        }
        let word = match done.lifted {
            true => "lifted",
            false => "lapsed",
        };
        self.say(&format!("{word} host={}", self.plan.host));
        Ok(())
    }
}

/// The reason of a `kept` line for a cordon that was in force when the verb asked.
const ALREADY: &str = "already_cordoned";

/// The reason of a `kept` line for a cordon that became another holder's while the verb held
/// its own.
const TAKEN_OVER: &str = "taken_over";

/// Cordons the host as `plan.holder`, holds the cordon for the window and lifts it, and
/// prints a line for each state change on `out`.
///
/// The server says whose the cordon is. When the first reply says the cordon in force is
/// another holder's, it is left as it is (not extended, not lifted) and the outcome is
/// [`Outcome::OtherHolder`]; with `plan.take_over` the cordon in force becomes the verb's
/// instead. Every call names the holder, so a lift ends only the verb's own cordon, and
/// every reply must name the host that was asked for. Time passes only in `pace`, which also
/// tells of a stop request; a stop lifts the cordon like the end of the window does.
pub fn hold(
    lease: &dyn HeldLease,
    pace: &mut dyn Pace,
    plan: &Plan<'_>,
    out: &mut dyn Write,
) -> Outcome {
    let mut window = Window {
        lease,
        plan,
        out,
        said_idle: false,
    };
    let first = lease.cordon_held(plan.lease_secs, plan.holder, plan.take_over);
    let first = match first {
        Ok(first) => first,
        Err(why) => return Outcome::Failed(explain("cannot cordon the host", &why)),
    };
    if let Err(trouble) = window.named(&first.host) {
        return Outcome::Failed(trouble.text);
    }
    if first.held_by_other {
        let line = window.kept_line(ALREADY, &first.host);
        window.say(&line);
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
        Err(Trouble {
            kept: Some(line), ..
        }) => {
            window.say(&line);
            Outcome::OtherHolder
        }
        Err(trouble) => Outcome::Failed(trouble.text),
    }
}
