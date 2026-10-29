//! One-time join tokens: minted for one launched instance, used once to register one host.
//!
//! The pool's join tokens (`--agent-token-file`) are shared by every host of the pool. A
//! machine that a provider launches for the pool should not hold such a token: it is
//! delivered after boot, and whoever reads it can register hosts until the operator
//! rotates it. A one-time token is the narrower credential. The operator's tooling mints
//! one per launched instance (`POST /v1/agent/join-tokens`, behind the client token),
//! names the provider's instance id, and delivers the token to that instance only.
//!
//! A token is bound to the instance (provider and instance id) and to a time. The first
//! hello that presents it, names that instance and arrives before the expiry registers the
//! host and consumes the token: it is then tied to the host id of that hello. After that
//! the token is the credential of that one host. It carries the host's later hellos (the
//! agent says hello again after a lost connection) and heartbeats, and nothing else: a
//! hello for another host id, or a heartbeat for one, is refused, and so is a hello of a
//! token that was never used once its time has passed.
//!
//! A used token is a credential only for as long as its host keeps using it. Its first
//! use sets `credential_expires_at_ms`, [`CREDENTIAL_TTL`] (24 hours, the registry's
//! default retention of a lost host) after the hello, and every hello and heartbeat it
//! serves moves that time to the same distance from then. Past it the token is refused
//! like an expired one (hello and heartbeat both, 403 `join-token-expired`) and the next
//! mint drops it, so a copy that leaked from a terminated instance stops being a way to
//! register that host id once the host has been silent for the bound. The server does
//! not learn when the registry forgets a host (the registry keeps no hook for it), so the
//! bound is what ends a token whose host is gone. A host silent for longer than the bound
//! needs a new token. A moved time is written only when it gains a step over the one in
//! the file, so a heartbeat does not write the file every time; a restart can therefore
//! lose at most that step of the time. The step is [`REFRESH_STEP`] (an hour) plus a part
//! of up to [`REFRESH_SPREAD`] that each token has of its own, taken from its digest: hosts
//! first used in the same minute do not all come due, and rewrite the whole file under the
//! lock, at the same heartbeat.
//!
//! The host credential replaces the token at the first hello. A hello that says it keeps
//! one ([`js_agent_wire::Hello::accepts_host_credential`]) and registers its host with an
//! unused token is answered with a credential of 256 random bits, made here
//! ([`Admitted::issued`]). In the one write that registers the host the token is retired:
//! its row is dropped, and a row for the credential takes its place, bound to the same
//! instance and host, with the same silence bound (the credential's own digest is its key,
//! and `replaces` names the token's digest, which is what the registry knows the host by,
//! [`JoinTokens::registered_as`]). A retired token is an unknown one: its hello and its
//! heartbeat get the 401 of a token that never existed, so the copy that stays in a
//! launch request's user data opens nothing. A credential serves the host's later hellos
//! and heartbeats exactly as the token did, is renewed by them, and is dropped by
//! [`JoinTokens::revoke`] with the rest of the instance's rows. A hello that does not say
//! it keeps a credential (an older agent) leaves the token as it was.
//!
//! If the answer carrying the credential is lost, the host holds nothing that
//! authenticates: it cannot register again, and whoever launched it releases it when its
//! boot time is up. If the registry refuses the hello that issued the credential, the
//! answer is not sent, so [`JoinTokens::release`] takes the credential back and puts the
//! token back as it was.
//!
//! The instance a hello names is the hello's own claim: the binding guards against
//! mix-ups (a token delivered to the wrong machine, a host id reused by another
//! instance), not against a holder of the token, who can name the instance too. The
//! client token can mint for any instance id.
//!
//! A hello that the registry then refuses gives the use back ([`JoinTokens::release`]):
//! the token stays unused for the instance, unless a hello of the same host was served
//! with it in between (the host is then registered with it, and the token stays its).
//! That mark is kept in memory only: a restart in between leaves the use standing, which
//! errs on the side of one token, one host. When that write fails the token stays tied to
//! the host, which says hello again with it; if that host cannot register, mint another.
//!
//! The scheduler's terminate path revokes the token of the instance it terminates
//! (`DELETE /v1/agent/join-tokens`, see [`crate::join_api`]): [`JoinTokens::revoke`] drops
//! every token bound to the instance, used or not, in one whole-file write under the
//! lock. A revoked token is unknown from then on, so its hello and heartbeat get the
//! same 401 as a token that never existed. This closes the window of a used token (up to
//! [`CREDENTIAL_TTL`] of silence) for the instances the scheduler terminates itself; an
//! instance that dies without a terminate call still falls back to that bound.
//!
//! Only the SHA-256 digest of a token is kept, in memory and in `<state file>.joins`, which
//! is written whole through a temporary file and renamed, like the file of bindings, and
//! is owner-only:
//!
//! ```toml
//! version = 1
//!
//! [[join]]
//! hash = "<64 hex digits: SHA-256 of the token>"
//! provider = "lambdalabs"
//! instance_id = "i-0abc"
//! expires_at_ms = 1700000000000
//! used_by = "host-7"          # once the token has registered a host,
//! credential_expires_at_ms = 1700086400000   # with the time its use ends
//! replaces = "<64 hex digits>"  # only on the row of a host credential: the digest of the
//!                               # token it replaced; the row's hash is the credential's
//! ```
//!
//! The lookup of a presented token is by its digest. The token is 256 random bits, so
//! the time a lookup takes tells nothing that could be turned into a guess.
//!
//! Every change is made on a copy, written, and kept only when the write succeeded: a
//! token is never consumed in memory alone, because a restart would give it back.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::{HostCredential, InstanceIdentity};
use js_lease::Clock;
use js_registry::{Credential, SessionSource};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::adapt::SharedClock;
use crate::agent_state::{read_if_present, replace};
use crate::error::StartupError;

/// The flag the refusals name: the file lives beside the lease file.
const FLAG: &str = "--state-file";

/// The version this build writes.
const VERSION: u32 = 1;

/// How long a token lasts when the request does not say.
pub(crate) const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// The longest a token may be asked to last.
pub(crate) const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a used token stays a credential after the last hello or heartbeat it served.
/// It equals the registry's default retention of a lost host (24 hours).
pub(crate) const CREDENTIAL_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How much later than the time in the file a moved credential time must be to be written.
pub(crate) const REFRESH_STEP: Duration = Duration::from_secs(60 * 60);

/// The most that is added to [`REFRESH_STEP`] for one token. The part a token has is
/// taken from its digest, so it is the same on every call and after a restart.
pub(crate) const REFRESH_SPREAD: Duration = Duration::from_secs(30 * 60);

/// The most tokens that may wait to be used at once.
pub(crate) const MAX_WAITING: usize = 256;

/// What the server keeps of one token. The key is the digest of the token.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    instance: InstanceIdentity,
    expires_at_ms: u64,
    used: Option<Used>,
    /// On the entry of a host credential: the digest of the one-time token it replaced.
    /// `expires_at_ms` is then that token's expiry, which gives the token back to a
    /// [`JoinTokens::release`].
    replaces: Option<String>,
}

/// The host a token registered and how long the token stays its credential.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Used {
    host: String,
    credential_expires_at_ms: u64,
    /// Whether a hello of `host` was served with the token after the hello that used it.
    /// Kept in memory only (a loaded token has it unset): it stops [`JoinTokens::release`]
    /// from freeing a token whose host the registry accepted by that later hello.
    again_served: bool,
}

/// What a granted [`JoinTokens::admit`] did to the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// The token was unused and now is tied to the host.
    Fresh,
    /// The token already was the host's.
    Again,
}

/// What a granted admission did, and the credential it made, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Admitted {
    pub(crate) admission: Admission,
    /// Set only with [`Admission::Fresh`], when the hello asked for a credential: the token
    /// is then retired and this is the host's credential, shown here once.
    pub(crate) issued: Option<HostCredential>,
}

/// The tokens by digest, as 64 lower-case hex digits.
type Entries = BTreeMap<String, Entry>;

#[derive(Debug, Serialize, Deserialize)]
struct Document {
    version: u32,
    #[serde(default)]
    join: Vec<Row>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Row {
    hash: String,
    provider: String,
    instance_id: String,
    expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replaces: Option<String>,
}

/// A token just made. The text is shown once, in the answer to the request that made it.
pub(crate) struct Minted {
    pub(crate) token: String,
    pub(crate) expires_at_ms: u64,
}

/// Why no token was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MintError {
    /// The operating system gave no random bytes.
    NoRandom,
    /// [`MAX_WAITING`] tokens already wait to be used.
    Full,
    /// The file could not be written; nothing was made.
    Unsaved,
}

/// Why no token was revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevokeError {
    /// The file could not be written; nothing was dropped.
    Unsaved,
}

/// Why a presented token was not accepted. Nothing of the token is in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinRefusal {
    /// The token's time has passed: never used and past its expiry, or used and not heard
    /// from for [`CREDENTIAL_TTL`] (or it was dropped after that).
    Expired,
    /// The token registered another host.
    Used,
    /// The hello names no instance, or one other than the token is bound to.
    WrongInstance,
    /// A heartbeat of a host the token did not register.
    NotAdmitted,
    /// The use could not be written down, so it was not allowed.
    Unsaved,
    /// The hello asked for a credential and the operating system gave no random bytes for
    /// it; nothing was used, and the host may say hello again.
    Unavailable,
    /// The server is set to require a host credential and the first hello of an unused
    /// token did not ask for one. Nothing was used; the token stays good for a hello that
    /// asks.
    CredentialRequired,
}

impl JoinRefusal {
    /// The code of the answer.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Expired => "join-token-expired",
            Self::Used => "join-token-used",
            Self::WrongInstance => "join-token-instance",
            Self::NotAdmitted => "join-token-not-admitted",
            Self::Unsaved => "join-token-unsaved",
            Self::Unavailable => "join-token-unavailable",
            Self::CredentialRequired => "join-token-credential-required",
        }
    }
}

/// The one-time tokens of the server.
pub(crate) struct JoinTokens {
    path: PathBuf,
    clock: SharedClock,
    random: Box<dyn SessionSource>,
    /// Refuse the first hello of an unused token that does not ask for a host credential.
    require_credential: bool,
    entries: Mutex<Entries>,
}

impl JoinTokens {
    /// The tokens the file at `path` holds (none when there is no file). `random` is the
    /// source of the bytes of a token.
    pub(crate) fn open(
        path: PathBuf,
        clock: SharedClock,
        random: Box<dyn SessionSource>,
    ) -> Result<Self, StartupError> {
        let entries = load(&path)?;
        Ok(Self {
            path,
            clock,
            random,
            require_credential: false,
            entries: Mutex::new(entries),
        })
    }

    /// These tokens with the first hello of an unused one required to ask for a host
    /// credential: a hello that does not is refused ([`JoinRefusal::CredentialRequired`])
    /// and leaves the token unused. A token that already registered its host is not
    /// affected, and pool tokens are not join tokens.
    pub(crate) fn requiring_credential(mut self, require: bool) -> Self {
        self.require_credential = require;
        self
    }

    fn lock(&self) -> MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the file hold `entries`.
    fn write(&self, entries: &Entries) -> io::Result<()> {
        replace(&self.path, render(entries)?.as_bytes())
    }

    /// The credential of `given` when it is a token of this set, used or not.
    pub(crate) fn identify(&self, given: &str) -> Option<Credential> {
        let credential = Credential::of_token(given);
        let known = self.lock().contains_key(&credential.to_hex());
        known.then_some(credential)
    }

    /// Makes a token for `instance` that lasts `ttl` from now.
    ///
    /// Tokens that passed their time unused are dropped first, and so is every unused
    /// token of the same instance: a second mint for an instance (the delivery of the
    /// first failed) replaces the first instead of leaving two ways in. A token that was
    /// used stays until the time of its credential has passed.
    pub(crate) fn mint(
        &self,
        instance: InstanceIdentity,
        ttl: Duration,
    ) -> Result<Minted, MintError> {
        let bytes = self.random.fresh().ok_or(MintError::NoRandom)?;
        let token = hex(&bytes);
        let now = self.clock.now();
        let expires_at_ms = now.plus(ttl).millis();
        let mut entries = self.lock();
        let mut next = entries.clone();
        next.retain(|_, entry| match &entry.used {
            Some(used) => used.credential_expires_at_ms > now.millis(),
            None => entry.expires_at_ms > now.millis() && entry.instance != instance,
        });
        let waiting = next.values().filter(|entry| entry.used.is_none()).count();
        if waiting >= MAX_WAITING {
            return Err(MintError::Full);
        }
        let entry = Entry {
            instance,
            expires_at_ms,
            used: None,
            replaces: None,
        };
        next.insert(Credential::of_token(&token).to_hex(), entry);
        self.write(&next).map_err(|err| {
            error!(error = %err, "cannot write the join token file");
            MintError::Unsaved
        })?;
        *entries = next;
        Ok(Minted {
            token,
            expires_at_ms,
        })
    }

    /// [`JoinTokens::admit_with`] for a hello that asks for no credential, and so leaves a
    /// token what it was.
    #[cfg(test)]
    pub(crate) fn admit(
        &self,
        credential: &Credential,
        host: &str,
        instance: Option<&InstanceIdentity>,
    ) -> Result<Admission, JoinRefusal> {
        self.admit_with(credential, host, instance, false)
            .map(|admitted| admitted.admission)
    }

    /// Lets the hello of `host`, naming `instance`, register with the token or host
    /// credential whose digest is `credential`, and consumes the token when it was unused.
    ///
    /// The token is accepted when it already registered this very host and the time of
    /// its credential has not passed (the agent says hello again; the time moves), or
    /// when it is unused, not expired, and `instance` is the one it is bound to. The
    /// check, the use and the write happen under one lock, so of two hellos that race for
    /// an unused token one wins. A caller whose hello is then refused elsewhere gives a
    /// [`Admission::Fresh`] use back with [`JoinTokens::release`].
    ///
    /// With `issue`, the use of an unused token also retires it: in the same write the
    /// token's row is replaced by one for a new host credential, which is returned. The
    /// hello that loses the race for the token finds it gone, so one credential is made
    /// for one token. Nothing is made for a token that already registered its host, or
    /// for a host credential.
    pub(crate) fn admit_with(
        &self,
        credential: &Credential,
        host: &str,
        instance: Option<&InstanceIdentity>,
        issue: bool,
    ) -> Result<Admitted, JoinRefusal> {
        let now = self.clock.now().millis();
        let key = credential.to_hex();
        let mut entries = self.lock();
        let entry = entries.get(&key).ok_or(JoinRefusal::Expired)?;
        if let Some(used) = &entry.used {
            if used.credential_expires_at_ms <= now {
                return Err(JoinRefusal::Expired);
            }
            if used.host != host {
                return Err(JoinRefusal::Used);
            }
            let held = entries.get_mut(&key).and_then(|entry| entry.used.as_mut());
            held.into_iter().for_each(|used| used.again_served = true);
            self.renew(&mut entries, &key, now);
            return Ok(Admitted {
                admission: Admission::Again,
                issued: None,
            });
        }
        if entry.expires_at_ms <= now {
            return Err(JoinRefusal::Expired);
        }
        if instance != Some(&entry.instance) {
            return Err(JoinRefusal::WrongInstance);
        }
        if self.require_credential && !issue {
            return Err(JoinRefusal::CredentialRequired);
        }
        let issued = self.credential_if(issue)?;
        let used = Entry {
            used: Some(Used {
                host: host.to_owned(),
                credential_expires_at_ms: credential_expiry(now),
                again_served: false,
            }),
            ..entry.clone()
        };
        let mut next = entries.clone();
        match &issued {
            None => next.insert(key, used),
            Some(issued) => {
                next.remove(&key);
                let replaces = Some(key);
                next.insert(
                    Credential::of_token(issued.expose()).to_hex(),
                    Entry { replaces, ..used },
                )
            }
        };
        self.write(&next).map_err(|err| {
            error!(error = %err, "cannot write the join token file");
            JoinRefusal::Unsaved
        })?;
        *entries = next;
        Ok(Admitted {
            admission: Admission::Fresh,
            issued,
        })
    }

    /// A new host credential when `issue`, drawn from the operating system's random bytes;
    /// none when not.
    fn credential_if(&self, issue: bool) -> Result<Option<HostCredential>, JoinRefusal> {
        if !issue {
            return Ok(None);
        }
        let bytes = self.random.fresh().ok_or(JoinRefusal::Unavailable)?;
        Ok(Some(HostCredential::from_bytes(&bytes)))
    }

    /// The credential the registry knows the host of `presented` by: the digest of the
    /// token that registered it when `presented` is a host credential, and `presented`
    /// itself otherwise. The registry binds a host id to the first credential it was
    /// admitted with, so a host that went on with its host credential must still present
    /// that one to it.
    pub(crate) fn registered_as(&self, presented: &Credential) -> Credential {
        let replaced = self
            .lock()
            .get(&presented.to_hex())
            .and_then(|entry| entry.replaces.clone());
        replaced
            .and_then(|hex| Credential::from_hex(&hex))
            .unwrap_or_else(|| presented.clone())
    }

    /// Gives back the use that [`JoinTokens::admit_with`] just made for `host` with the
    /// token whose credential is `credential`: the registry refused the hello, and the token
    /// stays good for the instance it was minted for. It does nothing when a hello of
    /// `host` was admitted [`Admission::Again`] since the use: that hello may have been
    /// the one the registry accepted, and the token is then the host's. A host credential
    /// the use issued is dropped and the token comes back unused: the answer that would have
    /// carried the credential was never sent. Written like the use; when the write fails the
    /// token stays tied to `host` (a restart agrees with the memory), and the operator mints
    /// again if that host cannot register.
    pub(crate) fn release(&self, credential: &Credential, host: &str) {
        let mut entries = self.lock();
        let Some(next) = given_back(&entries, &credential.to_hex(), host) else {
            return;
        };
        match self.write(&next) {
            Ok(()) => *entries = next,
            Err(err) => error!(error = %err, "cannot write the join token file"),
        }
    }

    /// Drops every token bound to `instance`, used or not, and writes the file; kept only
    /// when the write succeeded. Returns how many were dropped. An instance with no token
    /// drops none and writes nothing, so a repeated revoke is a no-op that succeeds.
    pub(crate) fn revoke(&self, instance: &InstanceIdentity) -> Result<usize, RevokeError> {
        let mut entries = self.lock();
        let mut next = entries.clone();
        next.retain(|_, entry| entry.instance != *instance);
        let dropped = entries.len() - next.len();
        if dropped == 0 {
            return Ok(0);
        }
        self.write(&next).map_err(|err| {
            error!(error = %err, "cannot write the join token file");
            RevokeError::Unsaved
        })?;
        *entries = next;
        Ok(dropped)
    }

    /// Whether the token whose credential is `credential` registered `host` and its
    /// credential has not run out: the only host it may heartbeat for. A heartbeat moves
    /// the time of the credential.
    pub(crate) fn serves(&self, credential: &Credential, host: &str) -> Result<(), JoinRefusal> {
        let now = self.clock.now().millis();
        let key = credential.to_hex();
        let mut entries = self.lock();
        let used = entries
            .get(&key)
            .and_then(|entry| entry.used.as_ref())
            .filter(|used| used.host == host)
            .ok_or(JoinRefusal::NotAdmitted)?;
        if used.credential_expires_at_ms <= now {
            return Err(JoinRefusal::Expired);
        }
        self.renew(&mut entries, &key, now);
        Ok(())
    }

    /// Moves the time of the credential at `key` to [`CREDENTIAL_TTL`] after `now`, when
    /// that gains [`REFRESH_STEP`] over the time held, and only when the file took it: a
    /// failed write leaves the time as it was and the next call tries again.
    fn renew(&self, entries: &mut Entries, key: &str, now: u64) {
        let renewed = entries.get(key).and_then(|entry| entry.renewed(key, now));
        let Some(renewed) = renewed else {
            return;
        };
        let mut next = entries.clone();
        next.insert(key.to_owned(), renewed);
        match self.write(&next) {
            Ok(()) => *entries = next,
            Err(err) => error!(error = %err, "cannot write the join token file"),
        }
    }
}

impl Entry {
    /// This entry with the time of its credential moved to [`CREDENTIAL_TTL`] after
    /// `now`, when that gains the step of the token at `key` over the time it holds.
    fn renewed(&self, key: &str, now: u64) -> Option<Entry> {
        let renewed = credential_expiry(now);
        let step = refresh_step_ms(key);
        let used = self.used.as_ref();
        let due = used.filter(|used| renewed >= used.credential_expires_at_ms.saturating_add(step));
        due.map(|used| Entry {
            used: Some(Used {
                host: used.host.clone(),
                credential_expires_at_ms: renewed,
                again_served: used.again_served,
            }),
            ..self.clone()
        })
    }
}

/// `entries` with the use of the token at `key` by `host` undone, or `None` when there is
/// no such use to undo: either the token itself is tied to `host` and no later hello of
/// the host was served with it, or a host credential of `host` replaced the token (the
/// token is then put back unused, the credential dropped).
fn given_back(entries: &Entries, key: &str, host: &str) -> Option<Entries> {
    let mut next = entries.clone();
    let tied = entries.get(key).filter(|entry| {
        let held = entry.used.as_ref();
        held.is_some_and(|used| used.host == host && !used.again_served)
    });
    if let Some(entry) = tied {
        next.insert(
            key.to_owned(),
            Entry {
                used: None,
                ..entry.clone()
            },
        );
        return Some(next);
    }
    let issued = entries.iter().find(|(_, entry)| {
        let held = entry.used.as_ref();
        entry.replaces.as_deref() == Some(key) && held.is_some_and(|used| used.host == host)
    });
    let (credential, entry) = issued?;
    next.remove(credential);
    next.insert(
        key.to_owned(),
        Entry {
            used: None,
            replaces: None,
            ..entry.clone()
        },
    );
    Some(next)
}

/// The gain over the file that makes a renewal of the token at `key` worth a write:
/// [`REFRESH_STEP`] and a part below [`REFRESH_SPREAD`] taken from the digest, with no
/// random source, so tokens first used together come due at different heartbeats.
fn refresh_step_ms(key: &str) -> u64 {
    let seed = key
        .bytes()
        .take(6)
        .fold(0u64, |seed, byte| seed * 257 + u64::from(byte));
    duration_ms(REFRESH_STEP) + seed % duration_ms(REFRESH_SPREAD)
}

/// The time a credential used at `now` lasts to.
fn credential_expiry(now: u64) -> u64 {
    now.saturating_add(duration_ms(CREDENTIAL_TTL))
}

fn duration_ms(span: Duration) -> u64 {
    u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
}

/// `bytes` in lower-case hex.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    text
}

/// Reads the file at `path`: no tokens when there is no file, a refusal to start when it
/// is not owner-only or cannot be read as written (a start with no tokens would give back
/// the ones that were used).
fn load(path: &Path) -> Result<Entries, StartupError> {
    let Some(bytes) = read_if_present(path)? else {
        return Ok(Entries::new());
    };
    let text = String::from_utf8(bytes).map_err(|_| "it is not text".to_string());
    text.and_then(|text| parse(&text)).map_err(|reason| {
        let named = format!(
            "{} is not a valid join token file: {reason}",
            path.display()
        );
        StartupError::new(FLAG, named)
    })
}

/// The tokens a file's text holds, or what is wrong with it.
fn parse(text: &str) -> Result<Entries, String> {
    let document: Document = toml::from_str(text).map_err(|err| err.message().to_string())?;
    let mut entries = Entries::new();
    for row in document.join {
        if Credential::from_hex(&row.hash).is_none() {
            return Err("a token hash is not 64 lower-case hex digits".to_string());
        }
        let used = match (row.used_by, row.credential_expires_at_ms) {
            (None, None) => None,
            (Some(host), Some(credential_expires_at_ms)) => Some(Used {
                host,
                credential_expires_at_ms,
                again_served: false,
            }),
            _ => {
                let both = "a token has only one of used_by and credential_expires_at_ms";
                return Err(both.to_string());
            }
        };
        let replaces = row.replaces;
        if replaces
            .as_deref()
            .is_some_and(|hash| Credential::from_hex(hash).is_none())
        {
            return Err("a replaced token hash is not 64 lower-case hex digits".to_string());
        }
        if replaces.is_some() && used.is_none() {
            return Err("a host credential has no host".to_string());
        }
        let entry = Entry {
            instance: InstanceIdentity {
                provider: row.provider,
                instance_id: row.instance_id,
            },
            expires_at_ms: row.expires_at_ms,
            used,
            replaces,
        };
        if entries.insert(row.hash, entry).is_some() {
            return Err("a token hash is listed twice".to_string());
        }
    }
    Ok(entries)
}

/// The text of the file that holds `entries`.
fn render(entries: &Entries) -> io::Result<String> {
    let join = entries
        .iter()
        .map(|(hash, entry)| Row {
            hash: hash.clone(),
            provider: entry.instance.provider.clone(),
            instance_id: entry.instance.instance_id.clone(),
            expires_at_ms: entry.expires_at_ms,
            used_by: entry.used.as_ref().map(|used| used.host.clone()),
            credential_expires_at_ms: entry
                .used
                .as_ref()
                .map(|used| used.credential_expires_at_ms),
            replaces: entry.replaces.clone(),
        })
        .collect();
    let document = Document {
        version: VERSION,
        join,
    };
    toml::to_string(&document).map_err(io::Error::other)
}

#[cfg(test)]
#[path = "join_tokens_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "join_credential_tests.rs"]
mod credential_tests;
