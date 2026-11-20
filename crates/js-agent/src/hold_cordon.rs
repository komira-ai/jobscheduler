//! The cordon of a held host: while the state root holds the hold file (see
//! [`crate::lifecycle::hold`]), the agent keeps its host cordoned through the lease
//! server's own-host route (`POST /v1/hosts/{id}:cordon`, see [`crate::own_host`]).
//!
//! A cordon is a lease of at most [`MAX_CORDON_SECS`] that lapses unless it is renewed, and
//! the step that drained the host (an update's drain) is not there to renew it after a
//! reboot, a restart of the agent or a halt of the update. So the agent does, for as long
//! as the hold file is there:
//!
//! - **It cordons as soon as its hello has made the host known.** The route answers
//!   `unknown-host` (409) until the server has heard the host's hello, so the [`Keeper`]
//!   asks nothing before the first hello of the process is accepted, and asks at once after
//!   each one (the hello of a host the server forgot, after its restart, included). A start
//!   placed on the host before the cordon is in force is refused `host-held` by the
//!   lifecycle.
//! - **It renews the cordon every [`HOLD_RENEW`]**, a third of the [`HOLD_LEASE_SECS`] it
//!   asks for each time, so two renewals may fail before the cordon lapses. A request that
//!   fails is asked again after [`HOLD_RETRY`] (or at the next hello, if that is sooner).
//! - **It names no holder.** A request that names none extends the cordon in force,
//!   whoever placed it, and never shortens it (the rule of `js_types::cordon`): the cordon
//!   of the drain, of an operator or of `js-agent cordon` is kept in force, not left to
//!   lapse as another holder's would be. The step that puts the host back removes the hold
//!   file and lifts the cordon.
//! - **It lifts nothing.** When the hold file is gone the agent stops renewing, and says
//!   so once; a cordon it renewed lapses at its end unless the step that removed the file
//!   lifted it.
//!
//! The keeper runs on the loop's thread, between two exchanges with the server (see
//! `drive_holding`); each request is bounded as one exchange is ([`RequestLimit::DEFAULT`]),
//! and goes to the same server. Time is read from the clock it is given, so a test moves it
//! by hand. Every line it has to say is returned to the loop, which logs it.
//!
//! The credential is the one the agent registered with: its host certificate, otherwise the
//! host credential in the state root (the own-host routes refuse the shared pool token),
//! otherwise the token file ([`login`]).
//!
//! [`RequestLimit::DEFAULT`]: crate::own_host::RequestLimit::DEFAULT

use std::path::Path;
use std::time::Duration;

use crate::credential::CredentialFile;
use crate::lifecycle::Clock;
use crate::lifecycle::hold;
use crate::lifecycle::taint::StateRoot;
use crate::login::Login;
use crate::own_host::{Cordoned, HttpLease, Lease, LeaseError, MAX_CORDON_SECS, RequestLimit};

/// The length of each cordon request, in seconds: the longest the server grants.
pub const HOLD_LEASE_SECS: u64 = MAX_CORDON_SECS;

/// The time from one cordon request that succeeded to the next: a third of
/// [`HOLD_LEASE_SECS`].
pub const HOLD_RENEW: Duration = Duration::from_secs(HOLD_LEASE_SECS / 3);

/// The time from a cordon request that failed to the next, when no hello comes first.
pub const HOLD_RETRY: Duration = Duration::from_secs(RETRY_SECS);

/// [`HOLD_RETRY`] in seconds.
const RETRY_SECS: u64 = 15;

/// [`HOLD_RENEW`] in milliseconds.
const RENEW_MS: u64 = HOLD_LEASE_SECS * 1000 / 3;

/// [`HOLD_RETRY`] in milliseconds.
const RETRY_MS: u64 = RETRY_SECS * 1000;

/// Connects to the lease server's own-host routes as this host, with the state root at the
/// path given; the error is the text a person is told.
pub type Connect<'a> = dyn Fn(&Path) -> Result<Box<dyn Lease>, String> + 'a;

/// What the keeper reads and asks.
pub struct Seams<'a> {
    /// The state root the hold file is in.
    pub state: &'a StateRoot,
    /// The user the hold file must belong to: the agent's.
    pub owner: u32,
    /// The host the agent registers, which every reply must name.
    pub host: &'a str,
    /// Connects to the lease server.
    pub connect: &'a Connect<'a>,
    /// The time, in milliseconds on a clock that does not go back.
    pub clock: &'a dyn Clock,
}

/// The state of the cordon of a held host, moved one step after each exchange with the
/// server ([`Keeper::step`]).
pub struct Keeper<'a> {
    seams: Seams<'a>,
    /// The connection, once made; dropped after a request that failed for another reason
    /// than an unknown host, so the next one connects again (with the credential the state
    /// root holds by then).
    lease: Option<Box<dyn Lease>>,
    /// A hello of this process has been accepted: the server knows the host.
    heard: bool,
    /// When the next request is due, on the clock; `None` when it is due at once.
    due_ms: Option<u64>,
    /// The hold file was there at the last step.
    holding: bool,
    /// The last request succeeded.
    placed: bool,
    /// The failure said last, so a failure that repeats is said once.
    told: Option<String>,
}

impl<'a> Keeper<'a> {
    /// A keeper that has heard no hello and holds no connection.
    #[must_use]
    pub fn new(seams: Seams<'a>) -> Self {
        Self {
            seams,
            lease: None,
            heard: false,
            due_ms: None,
            holding: false,
            placed: false,
            told: None,
        }
    }

    /// One step, after an exchange with the server; `hello` says the exchange was a hello the
    /// server accepted. It reads the hold file and, when the host is held, the server knows
    /// it and a request is due, asks for the cordon. The lines are what the agent says,
    /// without its `js-agent: ` prefix: a hold that begins or ends, a cordon placed (again),
    /// and a failure (once while it repeats).
    pub fn step(&mut self, hello: bool) -> Vec<String> {
        if hello {
            self.heard = true;
            self.due_ms = None;
        }
        let found = hold::read(self.seams.state, self.seams.owner);
        let mut lines = Vec::new();
        let Some(why) = found.why() else {
            if std::mem::replace(&mut self.holding, false) {
                lines.push(
                    "hold: the hold file is gone: the cordon of this host is no longer renewed"
                        .to_owned(),
                );
            }
            (self.due_ms, self.placed, self.told) = (None, false, None);
            return lines;
        };
        if !std::mem::replace(&mut self.holding, true) {
            lines.push(format!(
                "hold: {why}: it starts no work and keeps itself cordoned"
            ));
        }
        let now = self.seams.clock.now_ms();
        let due = self.due_ms.is_none_or(|due| now >= due);
        if self.heard && due {
            lines.extend(self.ask(now));
        }
        lines
    }

    /// Asks for the cordon at `now` and says what came of it.
    fn ask(&mut self, now: u64) -> Option<String> {
        match self.cordon() {
            Ok(cordoned) => {
                self.due_ms = Some(now.saturating_add(RENEW_MS));
                self.told = None;
                let said = !std::mem::replace(&mut self.placed, true) || !cordoned.already_cordoned;
                let left = cordoned.host.cordon.and_then(|cordon| cordon.expires_in_ms);
                said.then(|| {
                    format!(
                        "hold: cordoned host={} expires_in_ms={}",
                        self.seams.host,
                        left.unwrap_or(0)
                    )
                })
            }
            Err(why) => {
                self.due_ms = Some(now.saturating_add(RETRY_MS));
                self.placed = false;
                let line = format!(
                    "hold: cannot cordon host {}: {why}; asked again in {}s or at the next hello",
                    self.seams.host, RETRY_SECS
                );
                (self.told.as_ref() != Some(&line)).then(|| {
                    self.told = Some(line.clone());
                    line
                })
            }
        }
    }

    /// One cordon request, on the connection kept or a new one. A connection whose request
    /// failed for another reason than an unknown host is dropped.
    fn cordon(&mut self) -> Result<Cordoned, String> {
        let lease = match self.lease.take() {
            Some(lease) => lease,
            None => (self.seams.connect)(self.seams.state.path())?,
        };
        let answer = lease.cordon(HOLD_LEASE_SECS);
        let named = answer.and_then(|cordoned| match cordoned.host.id == self.seams.host {
            true => Ok(cordoned),
            false => Err(LeaseError::Unreachable(format!(
                "the reply names host {:?}",
                cordoned.host.id
            ))),
        });
        if matches!(named, Ok(_) | Err(LeaseError::UnknownHost)) {
            self.lease = Some(lease);
        }
        named.map_err(|why| why.to_string())
    }
}

/// The credential the keeper presents, chosen from what the agent registered with
/// (`registered`): the host certificate when the agent presents one; otherwise the host
/// credential the state root at `root` holds, when it holds one that can be read; otherwise
/// what the agent registered with (its token file).
#[must_use]
pub fn login(registered: &Login, root: &Path) -> Login {
    if matches!(registered, Login::HostCertificate(_)) {
        return registered.clone();
    }
    let file = CredentialFile::in_dir(root);
    match file.load() {
        Ok(Some(_)) => Login::Token(file.path()),
        _ => registered.clone(),
    }
}

/// The daemon's [`Connect`]: the own-host routes of `server` (trusting `ca_cert`) as `host`,
/// with the credential [`login`] chooses from what the agent registered with
/// (`registered`) and the state root at `root`, each call bounded by
/// [`RequestLimit::DEFAULT`]. The error is what `js-client` says of the flag or file it
/// could not use.
///
/// # Errors
///
/// The text of the client's error: an unusable server address, credential file or
/// certificate.
pub fn connect_http(
    server: &str,
    registered: &Login,
    ca_cert: Option<&Path>,
    host: &str,
    root: &Path,
) -> Result<Box<dyn Lease>, String> {
    let chosen = login(registered, root);
    HttpLease::connect(server, &chosen, ca_cert, host, RequestLimit::DEFAULT)
        .map(|lease| Box::new(lease) as Box<dyn Lease>)
        .map_err(|why| why.to_string())
}
