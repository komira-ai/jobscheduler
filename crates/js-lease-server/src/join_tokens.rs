//! One-time join tokens: minted for one launched instance, used once to register one host.
//!
//! The pool's join tokens (`--agent-token-file`) are shared by every host of the pool. A
//! machine that a provider launches for the pool should not hold such a token: it is
//! delivered after boot, and whoever reads it can register hosts until the operator
//! rotates it. A one-time token is the narrower credential. The operator's tooling mints
//! one per launched instance (`POST /v1/agent/join-tokens`, behind the client token, or
//! [`crate::agents::Agents::mint_for`] in process), names an id the minting side chose
//! for the instance, and delivers the token to that instance only. The id need not be
//! the provider's own: the scaler chooses the host's name before the launch and binds the
//! token to `<pool>:<name>`.
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
//! answer is not sent, so [`JoinTokens::release_writing`] takes the credential back and
//! puts the token back as it was.
//!
//! The instance a hello names is the hello's own claim: the binding guards against
//! mix-ups (a token delivered to the wrong machine, a host id reused by another
//! instance), not against a holder of the token, who can name the instance too. The
//! client token can mint for any instance id.
//!
//! A hello that the registry then refuses gives the use back
//! ([`JoinTokens::release_writing`]): the token stays unused for the instance. The server
//! takes its hellos one at a time, from the use to the registry's answer and the release
//! ([`crate::agents::Agents::host_writes`]), so no other hello of the host is served in
//! between. The release still leaves the use standing when a hello of the same host was
//! admitted with the token since the use (the host may then be registered with it, and the
//! token stays its); that mark is kept in memory only, and a restart in between leaves the
//! use standing too, which errs on the side of one token, one host. When the release's
//! write fails the token stays tied to the host, which says hello again with it; if that
//! host cannot register, mint another.
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
//!
//! # Where the tokens are kept
//!
//! Without a store setting the tokens are kept in `<state file>.joins`, as above. With one
//! (`--store`, `server.store`) no file is written, and the file is read only by the first start
//! over an empty store, which imports it once (`crate::store`): each row is the key
//! `hosts/join/<digest>` of the state store ([`crate::host_keys`]), a JSON object with a
//! `version` and the fields of a file row (all but `hash`, which is the key), read once at
//! start. Fields of a value that this build does not know are kept and written back. A
//! change is committed as the file is written, before it is held: a mint, a use, a release
//! and a revoke each commit the rows they change in one transaction, and the use of a token
//! by a hello commits the host's binding in the same transaction
//! ([`JoinTokens::admit_writing`]), as does the release that gives it back. A renewal
//! makes no store call on a hello or a heartbeat: a credential time that comes due for a
//! write is committed by the next tick ([`JoinTokens::flush`]), and is moved in memory
//! only then.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::{HostCredential, InstanceIdentity};
use js_lease::Clock;
use js_registry::{Credential, SessionSource};
use js_state::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::error;

use crate::adapt::SharedClock;
use crate::agent_state::{read_if_present, replace};
use crate::error::StartupError;
use crate::host_keys::{self, Extra, JOINS, Writes};

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
    /// [`JoinTokens::release_writing`].
    replaces: Option<String>,
    /// The fields of the stored value that this build does not know (none from the file,
    /// whose reader drops them), written back with the row.
    extra: Extra,
}

/// The host a token registered and how long the token stays its credential.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Used {
    host: String,
    credential_expires_at_ms: u64,
    /// Whether a hello of `host` was served with the token after the hello that used it.
    /// Kept in memory only (a loaded token has it unset): it stops [`JoinTokens::release_writing`]
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

/// A row as the store holds it under `hosts/join/<digest>`: the fields of a file row but
/// the hash, and the fields this build does not know.
#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    version: u32,
    provider: String,
    instance_id: String,
    expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replaces: Option<String>,
    #[serde(flatten)]
    extra: Extra,
}

/// Where the tokens are kept: the file beside the lease file, or the state store.
enum Keeper {
    File(PathBuf),
    Store(Arc<dyn Store>),
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
    /// The file or the store could not be written; nothing was made.
    Unsaved,
}

/// Why no token was revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevokeError {
    /// The file or the store could not be written; nothing was dropped.
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
    keeper: Keeper,
    clock: SharedClock,
    random: Box<dyn SessionSource>,
    /// Refuse the first hello of an unused token that does not ask for a host credential.
    require_credential: bool,
    entries: Mutex<Entries>,
    /// With the store: the digests whose credential time a hello or a heartbeat found due
    /// for a write, which the next [`JoinTokens::flush`] commits. Taken after `entries`.
    due: Mutex<BTreeSet<String>>,
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
        Ok(Self::over(Keeper::File(path), entries, clock, random))
    }

    /// The tokens `store` holds under `hosts/join/`, read once at start. A value that cannot
    /// be read as written is a refusal, as a damaged file is.
    ///
    /// # Errors
    ///
    /// The store could not be read, or a value is not a row (the reason names its key).
    pub(crate) fn in_store(
        store: Arc<dyn Store>,
        clock: SharedClock,
        random: Box<dyn SessionSource>,
    ) -> Result<Self, String> {
        let mut entries = Entries::new();
        for (hash, bytes) in host_keys::scan(&*store, JOINS)? {
            let named = format!("{JOINS}{hash}");
            let stored: Stored = host_keys::decode(&named, &bytes)?;
            let row = Row {
                hash,
                provider: stored.provider,
                instance_id: stored.instance_id,
                expires_at_ms: stored.expires_at_ms,
                used_by: stored.used_by,
                credential_expires_at_ms: stored.credential_expires_at_ms,
                replaces: stored.replaces,
            };
            let (hash, entry) = entry_of(row).map_err(|reason| format!("{named}: {reason}"))?;
            let extra = stored.extra;
            entries.insert(hash, Entry { extra, ..entry });
        }
        Ok(Self::over(Keeper::Store(store), entries, clock, random))
    }

    fn over(
        keeper: Keeper,
        entries: Entries,
        clock: SharedClock,
        random: Box<dyn SessionSource>,
    ) -> Self {
        Self {
            keeper,
            clock,
            random,
            require_credential: false,
            entries: Mutex::new(entries),
            due: Mutex::new(BTreeSet::new()),
        }
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

    fn due(&self) -> MutexGuard<'_, BTreeSet<String>> {
        self.due.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the file hold `next`, or commits to the store the rows `next` changes from
    /// `held` with `extra` in one transaction.
    fn write(&self, held: &Entries, next: &Entries, extra: Writes) -> io::Result<()> {
        match &self.keeper {
            Keeper::File(path) => replace(path, render(next)?.as_bytes()),
            Keeper::Store(store) => {
                let mut writes = changes(held, next);
                writes.extend(extra);
                host_keys::commit(&**store, writes)
            }
        }
    }

    /// Commits `extra` alone to the store; nothing in file mode, which has no such writes.
    fn write_extra(&self, extra: Writes) -> io::Result<()> {
        match &self.keeper {
            Keeper::File(_) => Ok(()),
            Keeper::Store(store) => host_keys::commit(&**store, extra),
        }
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
            extra: Extra::new(),
        };
        next.insert(Credential::of_token(&token).to_hex(), entry);
        self.write(&entries, &next, Writes::new()).map_err(|err| {
            error!(error = %err, "cannot write the join tokens");
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

    /// [`JoinTokens::admit_writing`] with nothing else to commit.
    #[cfg(test)]
    pub(crate) fn admit_with(
        &self,
        credential: &Credential,
        host: &str,
        instance: Option<&InstanceIdentity>,
        issue: bool,
    ) -> Result<Admitted, JoinRefusal> {
        self.admit_writing(credential, host, instance, issue, Writes::new())
    }

    /// Lets the hello of `host`, naming `instance`, register with the token or host
    /// credential whose digest is `credential`, and consumes the token when it was unused.
    ///
    /// The token is accepted when it already registered this very host and the time of
    /// its credential has not passed (the agent says hello again; the time moves), or
    /// when it is unused, not expired, and `instance` is the one it is bound to. The
    /// check, the use and the write happen under one lock, so of two hellos that race for
    /// an unused token one wins. A caller whose hello is then refused elsewhere gives a
    /// [`Admission::Fresh`] use back with [`JoinTokens::release_writing`].
    ///
    /// With `issue`, the use of an unused token also retires it: in the same write the
    /// token's row is replaced by one for a new host credential, which is returned. The
    /// hello that loses the race for the token finds it gone, so one credential is made
    /// for one token. Nothing is made for a token that already registered its host, or
    /// for a host credential.
    ///
    /// `extra` is what the store commits with the use, in the same transaction: the host's
    /// binding (always empty in file mode). For a token that already registered its host it
    /// is committed alone, when there is any; a failed commit refuses the hello, as a failed
    /// write of the use does.
    pub(crate) fn admit_writing(
        &self,
        credential: &Credential,
        host: &str,
        instance: Option<&InstanceIdentity>,
        issue: bool,
        extra: Writes,
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
            self.write_extra(extra).map_err(unsaved)?;
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
        self.write(&entries, &next, extra).map_err(unsaved)?;
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

    /// [`JoinTokens::release_writing`] with nothing else to commit.
    #[cfg(test)]
    pub(crate) fn release(&self, credential: &Credential, host: &str) {
        self.release_writing(credential, host, Writes::new());
    }

    /// Gives back the use that [`JoinTokens::admit_writing`] just made for `host` with the
    /// token whose credential is `credential`: the registry refused the hello, and the token
    /// stays good for the instance it was minted for. A host credential the use issued is
    /// dropped and the token comes back unused: the answer that would have carried the
    /// credential was never sent.
    ///
    /// `extra` is committed in the same transaction: with the store, the write that puts
    /// the host's binding back as it was before the use. True when the use was given back
    /// and that was written. False, with nothing written, when a hello of `host` was admitted
    /// [`Admission::Again`] since the use: that hello may have been the one the registry
    /// accepted, so the token stays the host's, and its binding with it. False too when the
    /// write failed: memory then stays as the use left it, as the file or the store does
    /// (the token tied to `host`; with the store, the binding kept too), and the operator
    /// mints again if that host cannot register.
    pub(crate) fn release_writing(
        &self,
        credential: &Credential,
        host: &str,
        extra: Writes,
    ) -> bool {
        let mut entries = self.lock();
        let Some(next) = given_back(&entries, &credential.to_hex(), host) else {
            return false;
        };
        match self.write(&entries, &next, extra) {
            Ok(()) => {
                *entries = next;
                true
            }
            Err(err) => {
                error!(error = %err, "cannot write the join tokens");
                false
            }
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
        self.write(&entries, &next, Writes::new()).map_err(|err| {
            error!(error = %err, "cannot write the join tokens");
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
    /// failed write leaves the time as it was and the next call tries again. With the store
    /// nothing is written here: the key is left for the next [`JoinTokens::flush`].
    fn renew(&self, entries: &mut Entries, key: &str, now: u64) {
        let renewed = entries.get(key).and_then(|entry| entry.renewed(key, now));
        let Some(renewed) = renewed else {
            return;
        };
        if let Keeper::Store(_) = self.keeper {
            self.due().insert(key.to_owned());
            return;
        }
        let mut next = entries.clone();
        next.insert(key.to_owned(), renewed);
        match self.write(entries, &next, Writes::new()) {
            Ok(()) => *entries = next,
            Err(err) => error!(error = %err, "cannot write the join tokens"),
        }
    }

    /// Commits the credential times that hellos and heartbeats found due since the last
    /// flush, in one transaction, and moves them in memory once that succeeded: each is set
    /// [`CREDENTIAL_TTL`] after now, when that still gains its step (a row revoked or
    /// renewed meanwhile is left as it is). The tick calls it. Nothing is due in file mode,
    /// where a hello or a heartbeat writes its renewal itself. False when the commit failed:
    /// the times stay as they were, still due, and the next flush tries again.
    pub(crate) fn flush(&self) -> bool {
        let mut entries = self.lock();
        let mut due = self.due();
        if due.is_empty() {
            return true;
        }
        let now = self.clock.now().millis();
        let renewed = due.iter().filter_map(|key| {
            let entry = entries.get(key).and_then(|entry| entry.renewed(key, now));
            entry.map(|entry| (key.clone(), entry))
        });
        let mut next = entries.clone();
        next.extend(renewed);
        match self.write(&entries, &next, Writes::new()) {
            Ok(()) => {
                *entries = next;
                due.clear();
                true
            }
            Err(err) => {
                error!(error = %err, "cannot write the join tokens");
                false
            }
        }
    }
}

/// The refusal of a use that could not be written down.
fn unsaved(err: io::Error) -> JoinRefusal {
    error!(error = %err, "cannot write the join tokens");
    JoinRefusal::Unsaved
}

/// The writes that make the store hold `next` instead of `held`: a delete for each row gone
/// and a put for each row new or changed (the in-memory mark of a hello served again is not
/// stored, and changes nothing).
fn changes(held: &Entries, next: &Entries) -> Writes {
    let gone = held.keys().filter(|hash| !next.contains_key(*hash));
    let gone = gone.map(|hash| (format!("{JOINS}{hash}"), None));
    let changed = next.iter().filter_map(|(hash, entry)| {
        let value = stored_value(entry);
        let same = held.get(hash).map(stored_value) == Some(value.clone());
        (!same).then(|| (format!("{JOINS}{hash}"), Some(value)))
    });
    gone.chain(changed).collect()
}

/// The value of a row's key: the fields of the row and those it kept.
fn stored_value(entry: &Entry) -> Vec<u8> {
    let used = entry.used.as_ref();
    let mut fields = Map::new();
    let provider = entry.instance.provider.clone();
    fields.insert("provider".to_owned(), Value::from(provider));
    let instance_id = entry.instance.instance_id.clone();
    fields.insert("instance_id".to_owned(), Value::from(instance_id));
    fields.insert("expires_at_ms".to_owned(), Value::from(entry.expires_at_ms));
    host_keys::field(&mut fields, "used_by", used.map(|used| used.host.clone()));
    let until = used.map(|used| used.credential_expires_at_ms);
    host_keys::field(&mut fields, "credential_expires_at_ms", until);
    host_keys::field(&mut fields, "replaces", entry.replaces.clone());
    host_keys::encode(fields, &entry.extra)
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

/// The keys the first start over a store imports from the file at `path` (none when there is
/// no file): each row at `hosts/join/<digest>`, as the store keeps it. The file is read as at
/// a start without a store, so its refusals stand.
pub(crate) fn import(path: &Path) -> Result<Vec<(String, Vec<u8>)>, StartupError> {
    let entries = load(path)?;
    let value = |(hash, entry): (&String, &Entry)| (format!("{JOINS}{hash}"), stored_value(entry));
    Ok(entries.iter().map(value).collect())
}

/// The tokens a file's text holds, or what is wrong with it.
fn parse(text: &str) -> Result<Entries, String> {
    let document: Document = toml::from_str(text).map_err(|err| err.message().to_string())?;
    let mut entries = Entries::new();
    for row in document.join {
        let (hash, entry) = entry_of(row)?;
        if entries.insert(hash, entry).is_some() {
            return Err("a token hash is listed twice".to_string());
        }
    }
    Ok(entries)
}

/// The digest and the entry of a row, or what is wrong with it.
fn entry_of(row: Row) -> Result<(String, Entry), String> {
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
        extra: Extra::new(),
    };
    Ok((row.hash, entry))
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
