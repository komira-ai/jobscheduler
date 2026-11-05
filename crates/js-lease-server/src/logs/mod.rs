//! The host credentials of live logs (`docs/design/live-logs.md`, sections 3.1, 4 and 5): one
//! temporary upload credential per agent (host), minted, delivered in heartbeat replies and
//! renewed by [`LogGrants`], and the sink each job's start carries.
//!
//! **One credential per host, not per job.** One agent runs every job on its host and does all
//! the uploading; the job's container never sees a credential; and an agent that is compromised
//! already controls every job on its host. A credential per job would buy nothing against that
//! agent and would cost one `AssumeRole` call per start. The credential may put and delete under
//! the configured log prefix and nothing else ([`js_logmint`]).
//!
//! **What is in memory, and what is stored.** [`LogGrants`] holds, in memory only, the current
//! credential of each agent that has one, by agent id, with what its next mint waits for. None
//! of it is written to the store, a file, a log line, the status page or an error: a log line
//! names the host and the failure's kind, never a key id, a secret or a session token. A
//! restart forgets every credential, and the next beat of each agent that uploads mints again.
//! What is stored is the job's sink, which holds no secret, in the start the agent is sent, and
//! the stream lengths the agent reports, with the job's result (`js_jobs::LogLengths`).
//!
//! **When a credential is minted.** Only for an agent whose hello reported
//! [`js_agent_wire::LOG_UPLOAD_CAPABILITY`]: when it registers ([`LogGrants::on_hello`]), on
//! its first beat when no credential is held for it (after a server restart, or once a mint
//! failed), when the one held has less than half of `credential_ttl` left (at a tick,
//! [`LogGrants::tick`]), and when the agent reports `stale` while it holds the very credential
//! held here (the store refused it; at most once per host per tick). An agent that reports
//! `stale` with an older credential, or none, is sent the one held here instead.
//!
//! **Where a mint runs.** Never in a request, under a lock or in a tick's critical section: a
//! mint is queued, and up to [`MAX_MINTS_IN_FLIGHT`] workers on the server's pool for blocking
//! calls take the queued hosts one at a time, call the minter with no lock held, and record the
//! answer. A heartbeat and a hello only queue (they never wait for a mint and never call the
//! minter), so a slow STS never makes a host look silent.
//!
//! **Delivery and renewal.** A reply to the heartbeat of an uploading agent carries the
//! credential held here ([`LogGrants::on_beat`]) when the agent's `log_grant_state.until_ms` is
//! absent or earlier than its expiry: a new host's first reply after the mint, and every reply
//! after a renewal, until a heartbeat reports that expiry. So a lost reply never loses a
//! renewal, and a delivered credential is not sent again. A credential minted for a `stale`
//! report may expire at the same instant as the one it replaces (a frozen clock, or an STS
//! that gives expiries in whole seconds), so the expiry cannot tell them apart: it is also
//! sent while the agent reports `stale` and the key it holds is not the one its report was
//! about; the agent stops reporting `stale` once it takes it. An expired credential is never
//! sent.
//!
//! **Mint failures** (`Unavailable` and `Refused` alike: a refused mint is a configuration
//! the operator can repair) are retried with a capped exponential backoff on the injected clock,
//! [`MINT_RETRY_FIRST`] doubling to [`MINT_RETRY_CAP`], at most one attempt in flight per host,
//! and never given up. A host keeps the credential it holds until it expires (it lasts at least
//! half of `credential_ttl` past the renewal's first attempt), so a store that is down for a
//! while at renewal costs nothing yet. While a host holds none its jobs run as always, without
//! live upload. The first failure of an episode is logged once per host at `warn`; every
//! failure is counted ([`LogCounters`], on the status page).
//!
//! **Forgetting.** A host's credential is forgotten when it leaves the registry (removed, or
//! silent past its grace) or says hello without the capability; the end of one of its jobs
//! never forgets it, since the host's other jobs still use it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use js_agent_wire::{HeartbeatReply, LOG_UPLOAD_CAPABILITY, LogGrant, LogGrantState, LogSink};
use js_lease::Clock as _;
use js_logmint::{Bucket, LogPrefix, MintError, MintRequest, Minter, Refusal};
use js_logstore::Credential;
use js_registry::{HostState, Registry};
use serde::Serialize;
use tracing::{info, warn};

use crate::adapt::SharedClock;

mod setup;
mod sink;

#[cfg(test)]
pub(crate) use setup::StoreClock;
pub(crate) use setup::from_config;
pub(crate) use sink::SinkTemplate;

/// The most mints that run at once, for all hosts together.
pub(crate) const MAX_MINTS_IN_FLIGHT: usize = 8;

/// The wait before the first retry of a failed mint.
pub(crate) const MINT_RETRY_FIRST: Duration = Duration::from_secs(5);

/// The longest wait between two attempts of a host's mint.
pub(crate) const MINT_RETRY_CAP: Duration = Duration::from_secs(5 * 60);

/// Why a minted credential is refused here: it cannot be carried to an agent (an expiry before
/// the epoch, or a field the wire refuses).
const UNUSABLE: &str = "the credential cannot be carried to an agent";

/// What every host credential is minted for: the bucket, the log prefix, and the lifetime asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scope {
    /// The bucket of the log store.
    pub(crate) bucket: Bucket,
    /// The configured log prefix.
    pub(crate) prefix: LogPrefix,
    /// `credential_ttl`.
    pub(crate) ttl: Duration,
}

/// The host credentials and the job sinks of a server with `sink = "object_store"`.
pub(crate) struct LogGrants {
    template: SinkTemplate,
    scope: Scope,
    minter: Arc<dyn Minter>,
    clock: SharedClock,
    /// Taken after the registry's lock has been given back and never held over a mint: a leaf.
    state: Mutex<State>,
    /// Signalled whenever a worker takes a host or ends.
    changed: Condvar,
    minted: AtomicU64,
    failed: AtomicU64,
}

/// What [`LogGrants`] holds behind its lock.
#[derive(Default)]
struct State {
    hosts: BTreeMap<String, Host>,
    /// The workers running.
    workers: usize,
    /// How many ticks have run: a host's `stale` is acted on at most once per tick.
    tick: u64,
}

/// One uploading host.
#[derive(Default)]
struct Host {
    /// The credential held for it.
    held: Option<LogGrant>,
    /// A mint is queued.
    wanted: bool,
    /// A worker is minting for it.
    minting: bool,
    /// The failed mints since its last success.
    failures: u32,
    /// The time, in milliseconds since the epoch, before which no mint is tried.
    retry_at_ms: u64,
    /// The tick in which a `stale` report last queued a mint.
    stale_tick: Option<u64>,
    /// The access key id of the credential the last `stale` report that queued a mint was
    /// about: one held that differs from it is the replacement, sent while the agent asks.
    refused_key: Option<String>,
}

impl Host {
    /// The credential held, unless it has expired at `now`.
    fn valid(&self, now: u64) -> Option<&LogGrant> {
        self.held.as_ref().filter(|grant| grant.expires_at_ms > now)
    }

    /// Queues a mint unless one is queued or running, or a credential is held that `keeps`.
    fn want_unless(&mut self, now: u64, keeps: impl Fn(&LogGrant) -> bool) {
        let held = self.valid(now).is_some_and(keeps);
        self.wanted |= !held & !self.minting;
    }

    /// Whether a worker may mint for it at `now`.
    fn due(&self, now: u64) -> bool {
        self.wanted & !self.minting & (self.retry_at_ms <= now)
    }
}

/// What the status page shows of the host credentials. It holds no credential.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct LogCounters {
    /// Mints that succeeded since the server started.
    pub(crate) minted: u64,
    /// Mints that failed since the server started.
    pub(crate) failed: u64,
    /// Hosts that hold a credential that has not expired.
    pub(crate) hosts_with_credential: usize,
    /// Hosts whose last mint failed.
    pub(crate) hosts_failing: usize,
}

/// Whether the capabilities of a hello say that the agent uploads live logs.
pub(crate) fn uploads(capabilities: &BTreeMap<String, String>) -> bool {
    capabilities
        .get(LOG_UPLOAD_CAPABILITY)
        .is_some_and(|value| value == "true")
}

/// The hosts that may hold a credential: those the registry holds as live whose last hello
/// reported the capability.
pub(crate) fn uploading_hosts(registry: &Registry) -> BTreeSet<String> {
    let hosts = registry.hosts().into_iter().filter(|view| {
        let reported = view
            .claim
            .as_ref()
            .is_some_and(|claim| uploads(claim.capabilities()));
        (view.state == HostState::Live) & reported
    });
    hosts.map(|view| view.id.to_string()).collect()
}

impl LogGrants {
    /// Grants that give jobs sinks under `template` and mint for `scope` with `minter`, at the
    /// times `clock` reads.
    pub(crate) fn new(
        template: SinkTemplate,
        scope: Scope,
        minter: Arc<dyn Minter>,
        clock: SharedClock,
    ) -> Self {
        Self {
            template,
            scope,
            minter,
            clock,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            minted: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now_ms(&self) -> u64 {
        self.clock.now().millis()
    }

    /// The sink of the job `job_id`, from the configuration alone: no credential in it.
    /// `None` for an id the object layout builds no prefix for.
    pub(crate) fn sink_for(&self, job_id: &str) -> Option<LogSink> {
        self.template.sink_for(job_id)
    }

    /// The credential held for `agent`, unless none is or it has expired: a memory read.
    #[cfg(test)]
    pub(crate) fn credential_for(&self, agent: &str) -> Option<LogGrant> {
        let now = self.now_ms();
        let state = self.lock();
        state
            .hosts
            .get(agent)
            .and_then(|host| host.valid(now))
            .cloned()
    }

    /// Makes sure a credential is on its way to `agent`: queues a mint unless one is held that
    /// has not expired, or one is queued or running, and starts it at once when a worker is
    /// free and the host is not waiting out a backoff. It never waits for the mint.
    pub(crate) fn prepare(self: &Arc<Self>, agent: &str) {
        let now = self.now_ms();
        self.lock()
            .hosts
            .entry(agent.to_owned())
            .or_default()
            .want_unless(now, |_| true);
        self.pump();
    }

    /// Forgets `agent`'s credential: no renewal and no delivery from now on. A mint running for
    /// it is dropped when it ends.
    pub(crate) fn forget(&self, agent: &str) {
        self.lock().hosts.remove(agent);
    }

    /// What a hello that the registry accepted (and whose binding was written) changes: an
    /// agent that reported the capability gets a credential on its way ([`LogGrants::prepare`]),
    /// one that did not has the one held for it forgotten.
    pub(crate) fn on_hello(self: &Arc<Self>, agent: &str, capabilities: &BTreeMap<String, String>) {
        match uploads(capabilities) {
            true => self.prepare(agent),
            false => self.forget(agent),
        }
    }

    /// The heartbeat of the uploading agent `agent`, which reported `state` of the credential it
    /// holds: the credential to put in the reply, if any (module docs, "Delivery and
    /// renewal"). Queues a mint when none is held, and when the agent reports `stale` while it
    /// holds the very credential held here (once per tick). Memory only: it never waits for a
    /// mint and never calls the minter.
    pub(crate) fn on_beat(
        self: &Arc<Self>,
        agent: &str,
        state: Option<LogGrantState>,
    ) -> Option<LogGrant> {
        let now = self.now_ms();
        let reported = state.unwrap_or_default();
        let grant = {
            let mut guard = self.lock();
            let tick = guard.tick;
            let host = guard.hosts.entry(agent.to_owned()).or_default();
            host.want_unless(now, |_| true);
            let held = host.valid(now).cloned();
            let holds_ours = held
                .as_ref()
                .is_some_and(|grant| reported.until_ms == Some(grant.expires_at_ms));
            let ask = reported.stale & holds_ours & (host.stale_tick != Some(tick));
            host.want_unless(now, |_| !ask);
            host.stale_tick = ask.then_some(tick).or(host.stale_tick);
            let refused = held
                .as_ref()
                .filter(|_| ask)
                .map(|g| g.access_key_id.clone());
            host.refused_key = refused.or(host.refused_key.take());
            let refused_key = host.refused_key.as_deref();
            held.filter(|grant| {
                let newer = reported
                    .until_ms
                    .is_none_or(|until| until < grant.expires_at_ms);
                let replaces = refused_key.is_some_and(|key| key != grant.access_key_id);
                newer | (reported.stale & replaces)
            })
        };
        self.pump();
        grant
    }

    /// One tick: forgets every host not in `present` (the hosts that may hold one,
    /// [`uploading_hosts`]), queues the renewal of each credential with less than half of
    /// `credential_ttl` left ([`LogGrants::renew_due`]), and starts the queued mints whose
    /// backoff has passed.
    pub(crate) fn tick(self: &Arc<Self>, present: &BTreeSet<String>) {
        {
            let mut state = self.lock();
            state.tick = state.tick.wrapping_add(1);
            state.hosts.retain(|agent, _| present.contains(agent));
        }
        self.renew_due(self.now_ms());
        self.pump();
    }

    /// Queues a mint for every host whose credential has less than half of `credential_ttl`
    /// left at `now` (or has expired, or that holds none), unless one is queued or running.
    fn renew_due(&self, now: u64) {
        let half = millis(self.scope.ttl / 2);
        let fresh = |grant: &LogGrant| grant.expires_at_ms.saturating_sub(now) >= half;
        for host in self.lock().hosts.values_mut() {
            host.want_unless(now, fresh);
        }
    }

    /// What the status page shows.
    pub(crate) fn counters(&self) -> LogCounters {
        let now = self.now_ms();
        let state = self.lock();
        let hosts = state.hosts.values();
        LogCounters {
            minted: self.minted.load(Ordering::SeqCst),
            failed: self.failed.load(Ordering::SeqCst),
            hosts_with_credential: hosts.clone().filter(|h| h.valid(now).is_some()).count(),
            hosts_failing: hosts.filter(|host| host.failures > 0).count(),
        }
    }

    /// Starts workers for the hosts whose mint is due, up to [`MAX_MINTS_IN_FLIGHT`] running,
    /// on the runtime's pool for blocking calls. Outside a runtime it starts none, and the next
    /// tick does. The workers log to the subscriber of the caller.
    fn pump(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let now = self.now_ms();
        let start = {
            let mut state = self.lock();
            let due = state.hosts.values().filter(|host| host.due(now)).count();
            let start = due.min(MAX_MINTS_IN_FLIGHT.saturating_sub(state.workers));
            state.workers += start;
            start
        };
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        for _ in 0..start {
            let this = Arc::clone(self);
            let dispatch = dispatch.clone();
            runtime.spawn_blocking(move || {
                tracing::dispatcher::with_default(&dispatch, || this.work());
            });
        }
    }

    /// One worker: takes the due hosts one at a time and mints for each with no lock held,
    /// until none is due.
    fn work(&self) {
        while let Some(agent) = self.take_due() {
            let request = MintRequest {
                bucket: self.scope.bucket.clone(),
                prefix: self.scope.prefix.clone(),
                host: agent.clone(),
                ttl: self.scope.ttl,
            };
            let answer = self.minter.mint(&request).and_then(grant_of);
            self.record(&agent, answer);
        }
    }

    /// The next host whose mint is due, marked as minting; `None` ends the worker, which is
    /// then no longer counted.
    fn take_due(&self) -> Option<String> {
        let now = self.now_ms();
        let mut state = self.lock();
        let next = state
            .hosts
            .iter_mut()
            .find(|(_, host)| host.due(now))
            .map(|(agent, host)| {
                host.wanted = false;
                host.minting = true;
                agent.clone()
            });
        state.workers -= usize::from(next.is_none());
        self.changed.notify_all();
        next
    }

    /// Records the answer of `agent`'s mint: the new credential replaces the one held, or the
    /// failure queues the next attempt after the backoff. Dropped when the host was forgotten
    /// meanwhile.
    fn record(&self, agent: &str, answer: Result<LogGrant, MintError>) {
        let now = self.now_ms();
        let mut state = self.lock();
        let Some(host) = state.hosts.get_mut(agent) else {
            return;
        };
        host.minting = false;
        match answer {
            Ok(grant) => {
                self.minted.fetch_add(1, Ordering::SeqCst);
                if host.failures > 0 {
                    info!(
                        host = agent,
                        failures = host.failures,
                        "a host's log credential was minted after failed mints"
                    );
                }
                host.held = Some(grant);
                host.failures = 0;
                host.retry_at_ms = 0;
            }
            Err(error) => {
                self.failed.fetch_add(1, Ordering::SeqCst);
                host.failures = host.failures.saturating_add(1);
                let wait = backoff(host.failures);
                host.wanted = true;
                host.retry_at_ms = now.saturating_add(millis(wait));
                if host.failures == 1 {
                    warn!(host = agent, %error, retry_in_ms = millis(wait), "cannot mint a host's log credential: its jobs run without live upload until a mint succeeds; later failures of this episode are not logged");
                }
            }
        }
    }

    /// Waits until no worker runs. A test calls it on the pool for blocking calls to see a
    /// queued mint through; it never waits for a time.
    #[cfg(test)]
    pub(crate) fn settle(&self) {
        let state = self.lock();
        let _settled = self
            .changed
            .wait_while(state, |state| state.workers > 0)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// The wait after the `failures`-th failed mint in a row: [`MINT_RETRY_FIRST`] doubled for each
/// failure after the first, at most [`MINT_RETRY_CAP`].
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    MINT_RETRY_FIRST
        .saturating_mul(1 << doublings)
        .min(MINT_RETRY_CAP)
}

/// `duration` in whole milliseconds, at most `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The grant an agent is sent for `credential`: its key, secret, token and expiry, checked as
/// the agent checks a reply's grant. A credential the wire cannot carry is a refused mint.
fn grant_of(credential: Credential) -> Result<LogGrant, MintError> {
    let unusable = || MintError::Refused(Refusal::Answer(UNUSABLE));
    let expires_at_ms = credential
        .expires_at
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(millis)
        .ok_or_else(unusable)?;
    let reply = HeartbeatReply {
        log_grant: Some(LogGrant {
            access_key_id: credential.access_key_id,
            secret_access_key: credential.secret_access_key,
            session_token: credential.session_token,
            expires_at_ms,
        }),
        ..HeartbeatReply::default()
    };
    let carried = reply.validate().log_grant.is_ok();
    reply.log_grant.filter(|_| carried).ok_or_else(unusable)
}

#[cfg(test)]
mod tests;
