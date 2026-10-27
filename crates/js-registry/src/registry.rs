use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent_wire::{Heartbeat, Hello, Phase, Welcome};
use js_lease::{Clock, Timestamp};
use js_pool_static::{HostBook, SeatError};
use js_provider::{HostCapacity, Observed, RefusalCode, StartAck, StartSpec};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass, Shape};

use crate::config::PoolConfig;
use crate::credential::Credential;
use crate::refusal::Refusal;

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
    binding: Option<Credential>,
    epoch: u64,
    phase: Phase,
    heard: Option<Timestamp>,
    agent_version: Option<String>,
    labels: BTreeMap<String, String>,
}

impl Record {
    fn awaiting() -> Self {
        Self {
            binding: None,
            epoch: 0,
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
}

struct State {
    book: HostBook,
    records: BTreeMap<HostId, Record>,
}

struct Shared {
    config: PoolConfig,
    clock: Arc<dyn Clock + Send + Sync>,
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
/// its last hello or heartbeat, computed when asked, so nothing ticks.
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
    /// An empty registry for the pool `config` describes, reading time from `clock`.
    pub fn new(config: PoolConfig, clock: impl Clock + Send + Sync + 'static) -> Self {
        let state = State {
            book: HostBook::new(),
            records: BTreeMap::new(),
        };
        Self {
            shared: Arc::new(Shared {
                config,
                clock: Arc::new(clock),
                state: Mutex::new(state),
            }),
        }
    }

    /// The provider the lease table runs on, serving the pool's class.
    pub fn provider(&self) -> crate::RegisteredProvider {
        crate::RegisteredProvider::new(self.clone(), self.shared.config.class().clone())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
    /// order: the pool is this one; the host id was not first admitted with another
    /// credential; the host id is not live; the shape claimed offers at least the pool's
    /// shape; the pool has room for a host it does not know. A refused hello creates and
    /// changes nothing. An admitted one starts a new session: the epoch is one more than
    /// the host's last, the host is live, and the id is bound to `credential` if it was
    /// not bound yet. A host that is lost or only awaiting is replaced, keeping its
    /// booking.
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
        let mut state = self.lock();
        let known = state.records.get(&host);
        let bound_elsewhere = known
            .and_then(|record| record.binding.as_ref())
            .is_some_and(|bound| bound != credential);
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
        state.book.insert_host(host.clone(), floor.clone());
        state.records.insert(
            host,
            Record {
                binding: Some(credential.clone()),
                epoch,
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
        })
    }

    /// Records that a host is alive in its session, with the phase it announces. A host
    /// that was lost is live again, with the booking it kept.
    ///
    /// `credential` is what the transport authenticated the request with. Refused when the
    /// message breaks the wire rules, when the host has no session (the
    /// registry restarted, or the host only awaits its first hello after one), when the
    /// credential is not the one the host id is bound to (checked before the epoch, which
    /// is a small counter and no proof of a session), or when it
    /// names an epoch that is not the current one. The agent answers the first and last by
    /// saying hello again.
    pub fn heartbeat(&self, heartbeat: &Heartbeat, credential: &Credential) -> Result<(), Refusal> {
        let host = HostId::new(&heartbeat.host_id).map_err(Refusal::BadHostId)?;
        heartbeat.validate().map_err(Refusal::Invalid)?;
        let now = self.shared.clock.now();
        let mut state = self.lock();
        let record = state
            .records
            .get_mut(&host)
            .filter(|record| record.heard.is_some())
            .ok_or(Refusal::UnknownHost)?;
        if record
            .binding
            .as_ref()
            .is_some_and(|bound| bound != credential)
        {
            return Err(Refusal::HostBound);
        }
        if record.epoch != heartbeat.epoch {
            return Err(Refusal::StaleEpoch);
        }
        record.heard = Some(now);
        record.phase = heartbeat.phase;
        Ok(())
    }

    /// Every host the registry holds, in id order.
    pub fn hosts(&self) -> Vec<HostView> {
        let now = self.shared.clock.now();
        let state = self.lock();
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
        let mut guard = self.lock();
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
        let state = self.lock();
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
    /// agent says hello. See [`crate::RegisteredProvider::seat`].
    pub(crate) fn seat(&self, host: &HostId, lease: LeaseId) -> Result<(), SeatError> {
        let mut guard = self.lock();
        let State { book, records } = &mut *guard;
        match book.seat(host, lease) {
            Err(SeatError::Unlisted(_)) => {
                book.insert_host(host.clone(), self.shared.config.shape().clone());
                records.insert(host.clone(), Record::awaiting());
                book.seat(host, lease)
            }
            settled => settled,
        }
    }
}
