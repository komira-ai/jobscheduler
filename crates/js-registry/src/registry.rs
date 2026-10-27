use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent_wire::{Heartbeat, Hello, Phase, Welcome};
use js_lease::{Clock, Timestamp};
use js_pool_static::{HostBook, SeatError};
use js_provider::{HostCapacity, Observed, RefusalCode, StartAck, StartSpec};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass, Shape};

use crate::config::PoolConfig;
use crate::credential::{Binding, Credential};
use crate::refusal::Refusal;
use crate::session::{OsSessions, SessionDigest, SessionSource, secret_text};

/// Where a host's session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// A booking was put back for the host after a restart, and it has not said hello
    /// since. Its lease is held and unobservable; the host takes no new lease.
    Awaiting,
    /// The host said hello or heartbeat within the grace period.
    Live,
    /// The host has been silent for longer than the grace period. It keeps its booking.
    Lost,
}

/// One host as the registry knows it: what an operator would list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    /// The host id.
    pub id: HostId,
    /// Whether the host is live, lost, or waiting to say hello again.
    pub state: HostState,
    /// The phase the host last announced.
    pub phase: Phase,
    /// The lease holding the host, if any.
    pub lease: Option<LeaseId>,
    /// The shape the pool books for the host (not the host's own claim).
    pub shape: Shape,
    /// The labels the host announced.
    pub labels: BTreeMap<String, String>,
    /// The agent version the host announced, once it has.
    pub agent_version: Option<String>,
    /// The current session epoch; 0 for a host that has not yet said hello.
    pub epoch: u64,
}

#[derive(Debug)]
struct Record {
    binding: Option<Binding>,
    epoch: u64,
    /// The digest of the secret issued at the hello that began this epoch; `None` for a
    /// host that has not said hello since a restart.
    session: Option<SessionDigest>,
    phase: Phase,
    heard: Option<Timestamp>,
    agent_version: Option<String>,
    labels: BTreeMap<String, String>,
}

impl Record {
    fn awaiting(binding: Option<Binding>) -> Self {
        Self {
            binding,
            epoch: 0,
            session: None,
            phase: Phase::Quarantined,
            heard: None,
            agent_version: None,
            labels: BTreeMap::new(),
        }
    }

    fn state(&self, now: Timestamp, grace_ms: u64) -> HostState {
        match self.heard {
            None => HostState::Awaiting,
            Some(heard) => match now.millis().saturating_sub(heard.millis()) > grace_ms {
                true => HostState::Lost,
                false => HostState::Live,
            },
        }
    }

    /// Whether a new lease may be placed here: heard from within the grace period and
    /// serving. Every other phase takes no new lease.
    fn bookable(&self, now: Timestamp, grace_ms: u64) -> bool {
        let live = self.state(now, grace_ms) == HostState::Live;
        let serving = self.phase == Phase::Serving;
        [live, serving].into_iter().all(|ok| ok)
    }

    /// Whether the host has been silent for longer than the grace period and the
    /// retention together. A host never heard (awaiting) is not.
    fn out_of_retention(&self, now: Timestamp, grace_ms: u64, retention_ms: u64) -> bool {
        let kept_for = grace_ms.saturating_add(retention_ms);
        self.heard
            .is_some_and(|heard| now.millis().saturating_sub(heard.millis()) > kept_for)
    }
}

struct State {
    book: HostBook,
    records: BTreeMap<HostId, Record>,
}

impl State {
    /// Forgets every host that has been lost for longer than the retention and holds no
    /// booking, binding included. A booked host stays however long it is silent.
    fn forget_lost(&mut self, now: Timestamp, grace_ms: u64, retention_ms: u64) {
        let lost = self
            .records
            .iter()
            .filter(|(_, record)| record.out_of_retention(now, grace_ms, retention_ms))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in lost {
            if self.book.remove_free(&id) {
                self.records.remove(&id);
            }
        }
    }
}

struct Shared {
    config: PoolConfig,
    clock: Arc<dyn Clock + Send + Sync>,
    sessions: Box<dyn SessionSource>,
    state: Mutex<State>,
}

/// The hosts of one pool that registered themselves, and the lease bookings on them.
///
/// A handle: clones share one registry. The transport calls [`Registry::hello`] and
/// [`Registry::heartbeat`] from the agent's requests, and the lease table calls the
/// [`crate::RegisteredProvider`] from [`Registry::provider`]. Every call takes the one
/// internal mutex for the length of the call and returns; none awaits, and no call
/// holds the mutex while calling out. Time comes only from the clock given at
/// construction: a host is *lost* when the clock reads more than the grace period past
/// its last hello or heartbeat, computed when asked, so nothing ticks. A lost host that
/// holds no booking is forgotten, binding included, once it has been lost for the pool's
/// lost-host retention ([`PoolConfig::with_lost_retention`]), also computed when asked:
/// every call forgets such hosts first, so they stop counting toward the pool's limit.
/// A host that holds a booking is never forgotten. An awaiting host (seated after a
/// restart, never heard) has no time to count from, so it is kept, even after its lease
/// is stopped, until its agent says hello.
///
/// A call that panics while it holds the mutex leaves the state as the panic left it: a
/// poisoned mutex is recovered, not propagated.
///
/// # Lock order
///
/// The lease table's lock is taken before the registry's, never the reverse: the
/// registry never calls the table.
#[derive(Clone)]
pub struct Registry {
    shared: Arc<Shared>,
}

impl Registry {
    /// An empty registry for the pool `config` describes, reading time from `clock` and
    /// session secrets from the operating system ([`OsSessions`]).
    pub fn new(config: PoolConfig, clock: impl Clock + Send + Sync + 'static) -> Self {
        Self::with_sessions(config, clock, OsSessions)
    }

    /// As [`Registry::new`], drawing session secrets from `sessions`. A deterministic
    /// source ([`crate::SequentialSessions`]) makes a test repeatable; production uses
    /// [`Registry::new`].
    pub fn with_sessions(
        config: PoolConfig,
        clock: impl Clock + Send + Sync + 'static,
        sessions: impl SessionSource + 'static,
    ) -> Self {
        let state = State {
            book: HostBook::new(),
            records: BTreeMap::new(),
        };
        Self {
            shared: Arc::new(Shared {
                config,
                clock: Arc::new(clock),
                sessions: Box::new(sessions),
                state: Mutex::new(state),
            }),
        }
    }

    /// The provider the lease table runs on, serving the pool's class.
    pub fn provider(&self) -> crate::RegisteredProvider {
        crate::RegisteredProvider::new(self.clone(), self.shared.config.class().clone())
    }

    /// The registry's state as of `now`: hosts lost for longer than the retention, with
    /// no booking, are forgotten first, so no call sees or counts one. Every call locks
    /// through here.
    fn lock_at(&self, now: Timestamp) -> MutexGuard<'_, State> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let config = &self.shared.config;
        state.forget_lost(now, config.grace_ms(), config.lost_retention_ms());
        state
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.lock_at(self.shared.clock.now())
    }

    pub(crate) fn pool_id(&self) -> ProviderId {
        self.shared.config.name().clone()
    }

    fn grace_ms(&self) -> u64 {
        self.shared.config.grace_ms()
    }

    /// Admits a host, or says why not. `credential` is what the transport authenticated
    /// the request with.
    ///
    /// The hello is checked as the wire rules say first ([`Hello::negotiate`] and
    /// [`Hello::validate`], which include the reserved label prefix), then in this
    /// order: the pool is this one; the host id is not bound to another credential, nor
    /// to another instance when its binding names one; the host id is not live; the
    /// shape claimed offers at least the pool's shape; the pool has room for a host it
    /// does not know (hosts forgotten after the lost-host retention do not count); a
    /// session secret can be made. A
    /// refused hello creates and changes nothing. An admitted one starts a new session:
    /// the epoch is one more than the host's last, the host is live, and the id is bound
    /// to `credential`, and to `Hello::instance` when the hello names one, if it was not
    /// bound yet. A host that is lost or only awaiting is replaced, keeping its booking.
    /// The welcome carries a fresh session secret from the registry's
    /// [`SessionSource`]; the registry keeps only its digest, and every heartbeat of the
    /// session must carry it ([`Registry::heartbeat`]). The secret of the replaced
    /// session stops working.
    ///
    /// The binding is as strong as what the first hello presented. With an instance
    /// identity it names the machine. Without one it holds at the level of the pool
    /// credential only: whoever holds the pool's token can say hello for the id once
    /// the host is lost, and inherits its booking. Per-host tokens or client
    /// certificates close that later (design D18). A binding is not upgraded by a later
    /// hello that names an instance.
    ///
    /// The pool-full check does not stop a holder of the pool token from locking the pool
    /// out: retention bounds only silent squatters. Such a holder can say hello for
    /// `max_machines` made-up ids and keep them live with the sessions it was issued, or
    /// refill the pool every grace plus retention, and every real new host then gets
    /// pool-full. The transport must rate-limit hellos per credential, and per-host
    /// credentials (design D18) close it; there is no per-credential cap here.
    ///
    /// The runs a hello lists as held are not looked at yet.
    pub fn hello(&self, hello: &Hello, credential: &Credential) -> Result<Welcome, Refusal> {
        let host = HostId::new(&hello.host_id).map_err(Refusal::BadHostId)?;
        let protocol = hello.negotiate().map_err(Refusal::Invalid)?;
        hello.validate().map_err(Refusal::Invalid)?;
        let config = &self.shared.config;
        if hello.pool != config.name().as_str() {
            return Err(Refusal::UnknownPool);
        }
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        let known = state.records.get(&host);
        let kept_binding = known.and_then(|record| record.binding.clone());
        let instance = hello.instance.as_ref();
        let bound_elsewhere = kept_binding
            .as_ref()
            .is_some_and(|bound| !bound.admits(credential, instance));
        if bound_elsewhere {
            return Err(Refusal::HostBound);
        }
        let live =
            known.is_some_and(|record| record.state(now, self.grace_ms()) == HostState::Live);
        if live {
            return Err(Refusal::HostLive);
        }
        let prior_epoch = known.map(|record| record.epoch);
        let floor = config.shape();
        let offers_floor = hello
            .to_shape()
            .ok()
            .is_some_and(|claim| claim.satisfies(floor.resources(), floor.capabilities()));
        if !offers_floor {
            return Err(Refusal::ShapeNotAccepted);
        }
        let room = state.records.len() < config.max_machines();
        if ![prior_epoch.is_some(), room].into_iter().any(|ok| ok) {
            return Err(Refusal::PoolFull);
        }
        let epoch = prior_epoch.unwrap_or(0).saturating_add(1);
        let secret = self
            .shared
            .sessions
            .fresh()
            .map(|bytes| secret_text(&bytes))
            .ok_or(Refusal::SessionUnavailable)?;
        state.book.insert_host(host.clone(), floor.clone());
        state.records.insert(
            host,
            Record {
                binding: Some(
                    kept_binding
                        .unwrap_or_else(|| Binding::new(credential.clone(), instance.cloned())),
                ),
                epoch,
                session: Some(SessionDigest::of(&secret)),
                phase: hello.phase,
                heard: Some(now),
                agent_version: Some(hello.agent_version.clone()),
                labels: hello.labels.clone(),
            },
        );
        Ok(Welcome {
            protocol,
            epoch,
            heartbeat_interval_ms: config.heartbeat_interval_ms(),
            grace_ms: config.grace_ms(),
            pool: config.name().to_string(),
            session: secret,
        })
    }

    /// Records that a host is alive in its session, with the phase it announces. A host
    /// that was lost is live again, with the booking it kept.
    ///
    /// `credential` is what the transport authenticated the request with. Refused when the
    /// message breaks the wire rules, when the host has no session (the
    /// registry restarted, or the host only awaits its first hello after one), when the
    /// credential is not the one the host id is bound to (checked before the epoch, which
    /// is a small counter and no proof of a session), when it
    /// names an epoch that is not the current one, or when it does not carry the session
    /// secret ([`Heartbeat::session`]) issued at the host's last hello. The agent answers
    /// the first and the epoch refusal by saying hello again.
    ///
    /// The session secret is what proves the sender is the host that was admitted: the
    /// pool's join token is shared by every host of the pool, and the host id and epoch
    /// are guessable, so neither names a session. The registry keeps only the SHA-256
    /// digest of the secret and compares digests in constant time. A refusal for the
    /// session (`session-mismatch`) is the last check and, like every refusal, changes
    /// nothing: the host's last-heard time, phase and epoch stay as they were. A hello
    /// replaces the secret, so the old one stops working at once.
    ///
    /// What this does not cover: the secret travels in the heartbeat, so it protects a
    /// session only as far as the transport does, and a pool-token holder can still
    /// take an id over with a hello, as [`Registry::hello`] says.
    pub fn heartbeat(&self, heartbeat: &Heartbeat, credential: &Credential) -> Result<(), Refusal> {
        let host = HostId::new(&heartbeat.host_id).map_err(Refusal::BadHeartbeatHostId)?;
        heartbeat.validate().map_err(Refusal::Invalid)?;
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        let record = state
            .records
            .get_mut(&host)
            .filter(|record| record.heard.is_some())
            .ok_or(Refusal::UnknownHost)?;
        if record
            .binding
            .as_ref()
            .is_some_and(|bound| bound.credential() != credential)
        {
            return Err(Refusal::HostBound);
        }
        if record.epoch != heartbeat.epoch {
            return Err(Refusal::StaleEpoch);
        }
        let issued = record
            .session
            .as_ref()
            .is_some_and(|digest| digest.matches(&heartbeat.session));
        if !issued {
            return Err(Refusal::SessionMismatch);
        }
        record.heard = Some(now);
        record.phase = heartbeat.phase;
        Ok(())
    }

    /// Every host the registry holds, in id order.
    pub fn hosts(&self) -> Vec<HostView> {
        let now = self.shared.clock.now();
        let state = self.lock_at(now);
        let booked = state.book.booked();
        let shape = self.shared.config.shape();
        let views = state.records.iter().map(|(id, record)| HostView {
            id: id.clone(),
            state: record.state(now, self.grace_ms()),
            phase: record.phase,
            lease: booked.get(id).copied(),
            shape: shape.clone(),
            labels: record.labels.clone(),
            agent_version: record.agent_version.clone(),
            epoch: record.epoch,
        });
        views.collect()
    }

    pub(crate) fn start(&self, classes: &[ResourceClass], spec: &StartSpec) -> StartAck {
        let now = self.shared.clock.now();
        let grace = self.grace_ms();
        let mut guard = self.lock_at(now);
        let State { book, records } = &mut *guard;
        let gate = HostBook::class_gate(classes, spec);
        let eligible = |id: &HostId, _: &Shape| {
            let record = records.get(id);
            record.is_some_and(|record| record.bookable(now, grace))
        };
        let ack = book.start_where(spec, gate, &eligible);
        // The book answers `NoFit` when no listed host fits. Every host is booked as the
        // pool's shape, so that happens for a request the pool's shape fits only when
        // no host is listed yet: a host could still join, which is capacity, not fit.
        let floor = self.shared.config.shape();
        let wanted = spec.request();
        let fits_pool = floor.satisfies(&wanted.resources, &wanted.capabilities);
        match (ack, fits_pool) {
            (StartAck::Refused(RefusalCode::NoFit), true) => {
                StartAck::Refused(RefusalCode::NoCapacity)
            }
            (other, _) => other,
        }
    }

    pub(crate) fn stop(&self, lease: &LeaseId) -> Observed {
        self.lock().book.stop(lease)
    }

    /// `Present` only while the host holding the lease is live; a lost or awaiting
    /// host is `Unknown`, never `Absent`, so the booking stays.
    pub(crate) fn observe(&self, lease: &LeaseId) -> Observed {
        let now = self.shared.clock.now();
        let state = self.lock_at(now);
        match state.book.read(lease) {
            Observed::Present { host } => {
                let record = state.records.get(&host);
                let live = record
                    .is_some_and(|record| record.state(now, self.grace_ms()) == HostState::Live);
                match live {
                    true => Observed::Present { host },
                    false => Observed::Unknown,
                }
            }
            absent => absent,
        }
    }

    pub(crate) fn capacity(&self) -> Vec<HostCapacity> {
        self.lock().book.hosts()
    }

    pub(crate) fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        self.lock().book.booked()
    }

    /// Puts back a booking the registry lost with its process. A host the registry does
    /// not know becomes an awaiting host: held, unobservable, not bookable, until its
    /// agent says hello; `binding` is what that hello must match, or `None` to let the
    /// first hello bind it. See [`crate::RegisteredProvider::seat`].
    pub(crate) fn seat(
        &self,
        host: &HostId,
        lease: LeaseId,
        binding: Option<Binding>,
    ) -> Result<(), SeatError> {
        let mut guard = self.lock();
        let State { book, records } = &mut *guard;
        match book.seat(host, lease) {
            Err(SeatError::Unlisted(_)) => {
                book.insert_host(host.clone(), self.shared.config.shape().clone());
                records.insert(host.clone(), Record::awaiting(binding));
                book.seat(host, lease)
            }
            settled => settled,
        }
    }
}
