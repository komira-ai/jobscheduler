use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent_wire::refusal as refusal_code;
use js_agent_wire::{Heartbeat, HeartbeatReply, Hello, JobSpec, Outcome, Phase, Taint, Welcome};
use js_caps::{FLAG_SET, HostContract, RUN_FILES, SCRATCH_QUOTA};
use js_lease::{Clock, Timestamp};
use js_pool_static::HostBook;
use js_provider::Observed;
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, LeaseId, MemoryHeadroom, ProviderId, RequestId, Shape};
use js_usage::ProfileKey;

use crate::config::PoolConfig;
use crate::credential::{Binding, Credential};
use crate::load;
use crate::refusal::Refusal;
use crate::runs::{RunRefusal, RunView, Runs, Standing, quota_default, vet};
use crate::session::{OsSessions, SessionDigest, SessionSource, secret_text};
use crate::whole::ending::Ending;
use crate::whole::exclusive::Exclusive;
use crate::whole::queue::WaitList;
use crate::whole::reasons::Cause;
use crate::whole::requests::{
    NotReady, RequestKey, RequestRecord, Requests, Rules, whole_settings,
};
use crate::whole::reserve::{Marks, Reservation};

mod demand;
mod marks;
mod provider_side;
mod start;

use demand::waiting_rows;
pub use demand::{Launchable, Waiting};
pub use marks::Busy;

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
    /// The shape the pool books for the host (not the host's own claim), with its memory
    /// cut to the pool's memory percent.
    pub shape: Shape,
    /// The memory the pool lists for the host, the percent of it that is booked and what
    /// that comes to (the memory of `shape`).
    pub memory: MemoryHeadroom,
    /// The shape the host announced at its last hello, which the registry checked against
    /// the pool's floor; `None` for a host that has not said hello since a restart. A
    /// scaler compares it with the machine it launched.
    pub claim: Option<Shape>,
    /// The labels the host announced.
    pub labels: BTreeMap<String, String>,
    /// The agent version the host announced, once it has.
    pub agent_version: Option<String>,
    /// The current session epoch; 0 for a host that has not yet said hello.
    pub epoch: u64,
    /// Whether the host has taken a heartbeat since its last hello. It is false only for a
    /// host that said hello and has not beaten since; a new hello sets it false again. A
    /// host with no session (not yet heard from since a restart) reads true: the registry
    /// cannot tell, and a scaler must not read silence there as a failed boot.
    pub heard_since_hello: bool,
    /// Whether the host holds no lease but is not yet clear of the last one: it still has
    /// runs to finish (runs of a lease that ended, or runs it lists that the server does
    /// not know), or it is restoring after a whole-node lease. It takes no new lease until
    /// a heartbeat shows none and, after a whole-node lease, shows it serving.
    pub releasing: bool,
    /// The taint the host reported, once it has: it can no longer be trusted clean, so it
    /// is sent no start and takes no new run. It is kept across a restart and is not
    /// cleared by anything in this crate.
    pub taint: Option<Taint>,
    /// How the host is set aside, if it is: for a whole-node request, or to be scaled away
    /// ([`HostMark::Draining`], [`HostMark::Releasing`], which win over the others).
    pub mark: Option<HostMark>,
    /// The request a reserved host is set aside for; `None` unless `mark` is
    /// [`HostMark::Reserved`].
    pub reserved_for: Option<RequestId>,
    /// When a reservation that drains a busy host is given up if the host is still not
    /// quiet; `None` unless the host is reserved and was busy when it was set aside.
    pub drain_deadline: Option<Timestamp>,
    /// The latest load the host reported (in the units of [`NodeLoad`]). Only the latest accepted beat's load is kept: minutes
    /// it carries that a consumer has not read before the next beat are lost to it, and a beat
    /// whose reply was lost is sent again, so a consumer that sums minutes must dedupe them by
    /// (lease, minute). Nothing says how much the agent dropped (the wire has no drop counts),
    /// so no minutes does not mean none were dropped. After a lost gap a reviving beat that
    /// repeats the old `seq` leaves the old reading in place; it is stale, not wrong, and carries
    /// its own `mono_usec`. Zero `cpu_throttled_usec` and
    /// `cpu_pressure_some_total_usec` mean not measured. `None` when the
    /// host has not reported one in this session, when its latest heartbeat carried none
    /// (the agent could not measure), and while the host is lost. A new hello clears it.
    pub load: Option<NodeLoad>,
}

/// How a host is set aside: for whole-node use, or to be scaled away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMark {
    /// Set aside for a waiting request: it takes no start but that request's.
    Reserved,
    /// A whole-node lease is committed on it.
    Held,
    /// Marked by [`Registry::drain`]: it holds no lease and no run, and takes no start of
    /// any kind. Only [`Registry::undrain`] or [`Registry::release_host`] ends the mark.
    Draining,
    /// Marked by [`Registry::release_host`]: the host is being given back and takes no
    /// start. Nothing clears the mark; [`Registry::remove`] drops the host.
    Releasing,
}

/// How many hosts are reserved, how many are held whole, and how many requests wait
/// ([`Registry::whole_counts`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WholeCounts {
    /// Hosts set aside for a waiting request.
    pub reserved: usize,
    /// Hosts with a whole-node lease committed.
    pub held: usize,
    /// Whole-node requests on the wait list.
    pub queued: usize,
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
    /// When a booking was put back for the host after a restart; `None` for a host that
    /// has said hello. It is what an awaiting host's retention counts from.
    seated: Option<Timestamp>,
    agent_version: Option<String>,
    labels: BTreeMap<String, String>,
    /// Whether the host's last hello reported the `run-files` capability. It changes only
    /// at a hello, and a hello of a lost host gives up every run still queued for it, so
    /// the check at queueing time is the only one a start needs.
    files: bool,
    /// The quota in bytes the host's last hello said it enforces on a run that sets no
    /// `disk_bytes` (`scratch-default-gib`), when it also reported `scratch-quota`; `None`
    /// for a host that reported no quota, or a default that is missing, not a whole
    /// number, zero or too large to count. Kept like `files`, and for the same reason: the
    /// run check sees only the pool's shape otherwise. It changes only at a hello, and a
    /// run books what it was given at queueing, so a new default never rebooks a live run.
    quota_default: Option<u64>,
    /// The run ids the host listed as held in its last hello or heartbeat.
    held: BTreeSet<String>,
    /// The heartbeats taken in this session.
    beats: u64,
    /// The first taint the host reported. It outlives a session and, through the file of
    /// bindings, a restart.
    taint: Option<Taint>,
    /// Where the host stands in a whole-node lease; `Free` for a host that never held one.
    exclusive: Exclusive,
    /// The last lease started on the host and the latest it can end: its start plus its
    /// max wall time. A lease put back after a restart has none, and counts as ending now.
    bound: Option<(LeaseId, Timestamp)>,
    /// Whether the host is being scaled away (see [`Registry::drain`]). It outlives a
    /// session and is held in memory only.
    leaving: Option<marks::Leaving>,
    /// The shape the host announced at its last hello.
    claim: Option<Shape>,
    /// The load of the host's latest accepted reading, see [`HostView::load`].
    load: load::Kept,
}

impl Record {
    /// A host put back after a restart. The taint a binding carries is the host's own, and
    /// the binding kept is the bare one, so the taint has a single home.
    fn awaiting(binding: Option<Binding>, seated: Timestamp) -> Self {
        let taint = binding.as_ref().and_then(|bound| bound.taint().cloned());
        Self {
            binding: binding.map(|bound| bound.with_taint(None)),
            taint,
            epoch: 0,
            session: None,
            phase: Phase::Quarantined,
            heard: None,
            seated: Some(seated),
            agent_version: None,
            labels: BTreeMap::new(),
            files: false,
            quota_default: None,
            held: BTreeSet::new(),
            beats: 0,
            exclusive: Exclusive::Free,
            bound: None,
            leaving: None,
            claim: None,
            load: load::Kept::default(),
        }
    }

    /// Whether the host lists no run and the server holds none live for it: nothing is
    /// left of the last lease for a new one to meet.
    fn quiet(&self, runs: &Runs, host: &HostId) -> bool {
        [self.held.is_empty(), !runs.live_on(host)]
            .into_iter()
            .all(|ok| ok)
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

    /// Whether a new lease may be placed here: heard from within the grace period,
    /// serving, `quiet` (no run left over from the last lease), not tainted, clear of
    /// a whole-node lease (not held, not restoring) and not being scaled away. Every
    /// other phase takes no new lease. A lease already on a tainted host is untouched.
    fn bookable(&self, now: Timestamp, grace_ms: u64, quiet: bool) -> bool {
        let live = self.state(now, grace_ms) == HostState::Live;
        let serving = self.phase == Phase::Serving;
        let clean = self.taint.is_none();
        let clear = self.exclusive.is_free();
        let staying = self.leaving.is_none();
        [live, serving, quiet, clean, clear, staying]
            .into_iter()
            .all(|ok| ok)
    }

    /// Whether the host has been silent for longer than the grace period and the
    /// retention together, counted from when it was last heard or, for a host never heard
    /// (awaiting), from when it was seated. A tainted host is never out of retention: it
    /// keeps its place, and its taint, until an operator deals with it.
    fn out_of_retention(&self, now: Timestamp, grace_ms: u64, retention_ms: u64) -> bool {
        let kept_for = grace_ms.saturating_add(retention_ms);
        let silent = self
            .heard
            .or(self.seated)
            .is_some_and(|since| now.millis().saturating_sub(since.millis()) > kept_for);
        [silent, self.taint.is_none()].into_iter().all(|ok| ok)
    }
}

struct State {
    book: HostBook,
    records: BTreeMap<HostId, Record>,
    /// Ids another source of hosts owns; a hello for one is refused.
    reserved: BTreeSet<HostId>,
    /// The container runs queued for hosts. In memory only.
    runs: Runs,
    /// The whole-node requests' failed prepares and the leases committed for them.
    requests: Requests,
    /// The whole-node requests that found no free host and wait for one.
    waiting: WaitList,
    /// The hosts set aside for waiting requests.
    marks: Marks,
    /// Whether a request that finds no host could be served by a launch.
    launchable: Option<Launchable>,
    /// Hosts that are being given back and are to be marked releasing from their first
    /// hello, whether or not the registry holds them yet ([`Registry::mark_ending`]).
    ending: BTreeSet<HostId>,
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
                self.runs.forget(&id);
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
/// restart, never heard) is counted from the time it was seated: once its lease has
/// ended, it is forgotten after the grace period and the retention have passed since
/// then, so an agent that never returns does not hold a place until the next restart.
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
            reserved: BTreeSet::new(),
            runs: Runs::default(),
            requests: Requests::default(),
            waiting: WaitList::default(),
            marks: Marks::default(),
            launchable: None,
            ending: BTreeSet::new(),
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
        let horizon = Rules::of(config).idle_horizon_ms();
        state.requests.sweep(now, horizon);
        state.waiting.expire(now, Rules::of(config).regain_grace_ms);
        let State { marks, waiting, .. } = &mut *state;
        marks.retain_waiting(waiting);
        state
    }

    /// Ends every committed whole-node lease that was not reported ready within
    /// `prepare_timeout` of its commit, as a failed prepare: the lease then reads
    /// `Absent`, so the lease table drops it by its ordinary path. A silent agent and a
    /// host gone lost are overdue the same as a slow one. The lease table's tick reads
    /// every pending lease through [`crate::RegisteredProvider`], which advances first;
    /// this does the same for a caller with a clock of its own.
    pub fn advance(&self) {
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        self.advance_at(&mut state, now);
    }

    fn advance_at(&self, state: &mut State, now: Timestamp) {
        let State {
            book,
            records,
            runs,
            requests,
            ..
        } = state;
        let rules = Rules::of(&self.shared.config);
        let mut ending = Ending {
            book,
            runs,
            requests,
            rules,
            now,
        };
        for (host, record) in records.iter_mut() {
            let overdue = record.exclusive.overdue(now, rules.prepare_timeout_ms);
            if let Some(lease) = overdue {
                ending.fail(
                    (&mut record.taint, &mut record.exclusive),
                    host,
                    lease,
                    (Cause::Timeout, ""),
                );
            }
        }
        // Nothing waits and nothing is set aside: there is nothing to place or to drop.
        if state.waiting.is_empty() & state.marks.is_empty() {
            return;
        }
        let settings = whole_settings(&self.shared.config);
        let hosts = self.host_facts(state, now);
        self.place_waiting(state, &settings, &hosts, now);
    }

    /// The record of a whole-node request: its failed prepares, the hosts left out of its
    /// placements and the last failure. `None` for a request that has failed none, or
    /// was placed since. Ask by the request id (`&RequestId`); a request whose start
    /// carried no id is kept under its content (`&Request`), so equal requests share a
    /// record.
    pub fn request_record(&self, key: impl Into<RequestKey>) -> Option<RequestRecord> {
        self.lock().requests.record(&key.into()).cloned()
    }

    /// Why a request will not be placed: the last code and detail reported, once it has
    /// used up `max_replace` prepare failures; `None` before, and after the refusal has
    /// been delivered to a start (the record is then removed, see the `requests` module)
    /// or the record has gone idle. A start of such a request answers
    /// `Refused(ExclusiveNotReady)`, which the consumer sees as the typed
    /// `exclusive-not-ready` refusal (wire 409). A restart forgets the record and resets `max_replace`. Ask as
    /// [`Registry::request_record`] does.
    pub fn not_ready(&self, key: impl Into<RequestKey>) -> Option<NotReady> {
        let record = self.request_record(key);
        record
            .filter(|record| record.terminal)
            .and_then(|record| record.last)
    }

    /// Where whole-node request `id` waits for a host, from the front (zero is next), or
    /// `None` if it is not waiting. A request waits from the first start that finds a
    /// capable host but none free, in the order such starts were first seen; a repeat of
    /// the start is a poll and keeps its place. It stops waiting when it is placed, is
    /// cancelled, is refused, or is not presented for the pool's `regain_grace`.
    ///
    /// The order is advisory: a start places on any eligible host without asking whether
    /// others wait, so a later waiter, or a request not yet on the list, can take a host
    /// that freed before the request at the front presents itself again. The list holds
    /// at most [`crate::MAX_WAITING_REQUESTS`]; a new request past that is answered
    /// `NoCapacity` and not entered.
    pub fn queue_position(&self, id: &RequestId) -> Option<usize> {
        self.lock().waiting.position(id)
    }

    /// Takes whole-node request `id` off the wait list at once; whether it was waiting.
    pub fn cancel_request(&self, id: &RequestId) -> bool {
        self.lock().waiting.cancel(id)
    }

    /// The hosts set aside for waiting whole-node requests, in host order. A host is set
    /// aside for one request and a request holds one host. A request's next start commits
    /// its lease on the host it holds; until then the host takes no other start.
    pub fn reservations(&self) -> Vec<Reservation> {
        self.lock().marks.reservations()
    }

    /// Whether `host` is set aside for a waiting request.
    pub fn is_reserved(&self, host: &HostId) -> bool {
        self.lock().marks.holder(host).is_some()
    }

    /// The host `lease` is booked on, if it is booked on one.
    pub fn host_of(&self, lease: &LeaseId) -> Option<HostId> {
        match self.lock().book.read(lease) {
            Observed::Present { host } => Some(host),
            Observed::Absent | Observed::Unknown => None,
        }
    }

    /// The hosts reserved and held, and the requests waiting, now.
    pub fn whole_counts(&self) -> WholeCounts {
        let state = self.lock();
        let held = state
            .records
            .values()
            .filter(|r| r.exclusive.held_since().is_some());
        WholeCounts {
            reserved: state.marks.reservations().len(),
            held: held.count(),
            queued: state.waiting.waiting().count(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.lock_at(self.shared.clock.now())
    }

    pub(crate) fn pool_id(&self) -> ProviderId {
        self.shared.config.name().clone()
    }

    /// The whole-node settings of the pool this registry serves, when it has them: the one
    /// source of the prepare timeout and the other whole-node figures for everything that
    /// counts them (the registry, the lease table, the scaler).
    pub fn whole_node(&self) -> Option<&crate::WholeNodeConfig> {
        self.shared.config.whole_node()
    }

    fn grace_ms(&self) -> u64 {
        self.shared.config.grace_ms()
    }

    /// Admits a host, or says why not. `credential` is what the transport authenticated
    /// the request with.
    ///
    /// The hello is checked as the wire rules say first ([`Hello::negotiate`] and
    /// [`Hello::validate`], which include the reserved label prefix), then in this
    /// order: the pool is this one; the host id is not reserved to another source
    /// ([`Registry::reserve`]); the host id is not bound to another credential, nor
    /// to another instance when its binding names one; the host id is not live; when
    /// the pool declares an execution mode ([`PoolConfig::with_exec`]), the host meets its
    /// contract ([`Refusal::HostContract`] and [`Refusal::ExecutionMismatch`] name the
    /// capability at fault); the shape claimed offers at least the pool's shape; the pool has room for a host it
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
    /// A pool that names `scratch-quota` also refuses (`shape-not-accepted`) a host whose
    /// quota default does not count (see `quota_default`) or is more than the pool's disk.
    ///
    /// The runs a hello lists as held are never adopted. A run the registry holds a live
    /// record of stays as it is, now listed by the host; every other listed run is
    /// answered with a cancel in the host's next heartbeat reply (a welcome carries no
    /// commands), and the host takes no new lease until a heartbeat lists none. A queued
    /// run the hello does not list will never be listed, since the host was lost or only
    /// awaited before it said hello, and is given up.
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
        if state.reserved.contains(&host) {
            return Err(Refusal::HostListed);
        }
        let known = state.records.get(&host);
        let kept_binding = known.and_then(|record| record.binding.clone());
        let kept_taint = known.and_then(|record| record.taint.clone());
        let kept_exclusive =
            known.map_or(Exclusive::Free, |record| record.exclusive.after_hello(now));
        let kept_bound = known.and_then(|record| record.bound);
        let kept_leaving = known.and_then(|record| record.leaving);
        let kept_leaving = match state.ending.contains(&host) {
            true => Some(marks::Leaving::Releasing),
            false => kept_leaving,
        };
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
        if let Some(exec) = config.exec() {
            let contract = HostContract::new(exec, floor.resources().gpus() > 0);
            contract.check(&hello.capabilities)?;
        }
        let claim = hello.to_shape().ok();
        let offers_floor = claim
            .as_ref()
            .is_some_and(|claim| claim.satisfies(floor.resources(), floor.capabilities()));
        if !offers_floor {
            return Err(Refusal::ShapeNotAccepted);
        }
        // A pool that names `scratch-quota` books a default for every run without disk, so
        // a host must report one that counts and fits the pool's disk: otherwise its runs
        // without disk would book nothing, or never fit.
        let quota_pool = floor.capabilities().contains_key(SCRATCH_QUOTA);
        let default = quota_default(&hello.capabilities);
        let fits = default.is_some_and(|bytes| bytes <= floor.resources().disk_bytes());
        if [quota_pool, !fits].into_iter().all(|ok| ok) {
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
        config.list_in(&mut state.book, host.clone());
        let held = hello.held.iter().map(|run| run.run_id.clone()).collect();
        state.records.insert(
            host.clone(),
            Record {
                binding: Some(
                    kept_binding
                        .unwrap_or_else(|| Binding::new(credential.clone(), instance.cloned())),
                ),
                epoch,
                session: Some(SessionDigest::of(&secret)),
                phase: hello.phase,
                heard: Some(now),
                seated: None,
                agent_version: Some(hello.agent_version.clone()),
                labels: hello.labels.clone(),
                files: hello.capabilities.get(RUN_FILES).map(String::as_str) == Some(FLAG_SET),
                quota_default: default,
                held,
                beats: 0,
                taint: kept_taint,
                exclusive: kept_exclusive,
                bound: kept_bound,
                leaving: kept_leaving,
                claim,
                load: load::Kept::default(),
            },
        );
        state.runs.sync(&host, &hello.held, true);
        Ok(Welcome {
            protocol,
            epoch,
            heartbeat_interval_ms: config.heartbeat_interval_ms(),
            grace_ms: config.grace_ms(),
            pool: config.name().to_string(),
            session: secret,
            host_credential: None,
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
    ///
    /// An admitted heartbeat is answered with what the host is to do ([`Registry::queue_run`]
    /// and the runs module's rules): a stop for each live run a stop was asked for, a
    /// cancel for each run it lists that is not a live run of its own, a start for each
    /// queued run (not while the host is out of the serving phase), at most
    /// [`js_agent_wire::limits::MAX_COMMANDS`] in that order; and an acknowledgement
    /// for each result it carries. A refused heartbeat changes no run.
    ///
    /// A run queued with a bound ([`Registry::queue_run`]) is started from the time that
    /// bound has left at each reply, and is not started at all once the bound has passed:
    /// it is then stopped, and given up as never started when the host still does not list
    /// it after that.
    ///
    /// **Taint.** The first taint report a host sends is kept, and the reply says
    /// `taint_ack` whenever the heartbeat carries one. The caller must make the taint
    /// durable ([`Registry::bindings`]) before it lets the acknowledgement go, since the
    /// agent stops reporting once it has it. A tainted host is sent no start, takes no new
    /// run ([`RunRefusal::HostTainted`]), takes no new lease (a lease already on it is
    /// untouched until it ends), and is never forgotten for being silent; the
    /// runs it already holds finish, their results are acknowledged, and their stops are
    /// sent as for any host. Nothing here clears a taint.
    pub fn heartbeat(
        &self,
        heartbeat: &Heartbeat,
        credential: &Credential,
    ) -> Result<HeartbeatReply, Refusal> {
        let host = HostId::new(&heartbeat.host_id).map_err(Refusal::BadHeartbeatHostId)?;
        heartbeat.validate().map_err(Refusal::Invalid)?;
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State {
            book,
            records,
            runs,
            requests,
            ..
        } = &mut *guard;
        let record = records
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
        let gap = record.state(now, self.grace_ms()) == HostState::Lost;
        record.heard = Some(now);
        record.phase = heartbeat.phase;
        record.beats = record.beats.saturating_add(1);
        record.load.take(heartbeat.load.as_ref());
        record.held = heartbeat
            .held
            .iter()
            .map(|run| run.run_id.clone())
            .collect();
        // An agent that refuses a start because its host is tainted may be unable to write
        // its own marker (a full disk), so that refusal taints the host here as a report
        // would. It is the host's fault: the result is answered like any other.
        let refused = heartbeat.results.iter().find(|result| {
            let by_host = result.detail.as_deref() == Some(refusal_code::HOST_TAINTED);
            by_host && result.outcome == Outcome::FailedToStart
        });
        let reported = heartbeat.taint.clone().or_else(|| {
            refused.map(|result| Taint {
                reason: refusal_code::HOST_TAINTED.to_string(),
                run_id: Some(result.run_id.clone()),
                check: None,
            })
        });
        record.taint = record.taint.take().or(reported);
        record.exclusive = record.exclusive.after_beat();
        let clean = record.taint.is_none();
        if let Some(ready) = &heartbeat.exclusive_ready {
            record.exclusive = record.exclusive.report_ready(ready, clean);
            let granted = record.exclusive.ready_lease();
            granted.iter().for_each(|lease| requests.granted(lease));
        }
        // A typed refusal of the lease committed to the host ends it at once; one for any
        // other lease says nothing about this host's preparation.
        let refused = heartbeat.exclusive_refused.as_ref().and_then(|refused| {
            let lease = record.exclusive.committed()?;
            let named = lease.to_string() == refused.lease_id;
            named.then_some((lease, Cause::Refused(refused.code), refused.detail.as_str()))
        });
        if let Some((lease, cause, detail)) = refused {
            let rules = Rules::of(&self.shared.config);
            let mut ending = Ending {
                book: &mut *book,
                runs: &mut *runs,
                requests: &mut *requests,
                rules,
                now,
            };
            ending.fail(
                (&mut record.taint, &mut record.exclusive),
                &host,
                lease,
                (cause, detail),
            );
        }
        let standing = Standing {
            gap,
            tainted: !clean,
            unready: record.exclusive.awaits_ready(),
        };
        let reply = runs.answer(&host, heartbeat, standing, (book, now.millis()));
        Ok(HeartbeatReply {
            taint_ack: heartbeat.taint.is_some(),
            exclusive: record.exclusive.granted(),
            ..reply
        })
    }

    /// Queues `spec` as a run of `lease` on `host`, to go out on the host's next
    /// heartbeat reply. The caller (the transport) has found that the lease is held on
    /// `host`, filled `spec.run_id` with an id it minted and bounded `spec.limits.wall_ms`
    /// and `spec.valid_for_ms` by the lease's remaining time; the registry fills
    /// `lease_id` and `heartbeat_seq`. A `spec.valid_for_ms` above zero is the time the
    /// lease has left: the registry keeps the instant it ends and makes every start from
    /// the time left then, so the wall limit and the validity of a start that goes out
    /// late are cut again to what is left (`spec.limits.wall_ms` is the most the run may
    /// have). Zero is no bound.
    ///
    /// Refused, in this order: [`RunRefusal::HostNotRegistered`] for a host that did not
    /// register itself; the job when it breaks a wire rule ([`RunRefusal::Invalid`]),
    /// asks for host isolation ([`RunRefusal::HostJobsNotAllowed`]) or exceeds the
    /// booked shape alone ([`RunRefusal::LimitsExceedShape`]); a job with run files for a
    /// host whose hello did not report the `run-files` capability
    /// ([`RunRefusal::FilesUnsupported`]); a job with `limits.disk_bytes` for a host whose
    /// hello did not report `scratch-quota` with a default ([`RunRefusal::DiskUnsupported`]);
    /// [`RunRefusal::HostLost`]
    /// for a host not heard within the grace period; [`RunRefusal::NotHeld`] when the
    /// registry's booking of `host` is not `lease`; [`RunRefusal::HostTainted`] for a
    /// host that reported a taint; [`RunRefusal::NoCapacity`] when the host is marked
    /// draining or releasing ([`Registry::drain`]), is out of the serving phase or the
    /// run does not fit what is left of the booked shape (the limits of the host's live
    /// runs, the GPU indices they hold, and [`crate::MAX_LIVE_RUNS`]);
    /// [`RunRefusal::RunExists`] for an id in use. A refusal changes nothing.
    ///
    /// A run's limits count against the budget until it leaves the host's held list, and
    /// not before: the host drops a run from that list only after its container and
    /// scratch are gone.
    pub fn queue_run(
        &self,
        host: &HostId,
        lease: &LeaseId,
        spec: JobSpec,
    ) -> Result<RunView, RunRefusal> {
        self.queue_keyed_run(host, lease, spec, None)
    }

    /// [`Registry::queue_run`] for a run whose work the lease holder named: `profile`, the
    /// name the holder supplied, is kept beside the run (in memory, like the run) and is what
    /// [`Registry::profile_of`] answers. It is not part of the job, so the host never receives it.
    pub fn queue_keyed_run(
        &self,
        host: &HostId,
        lease: &LeaseId,
        mut spec: JobSpec,
        profile: Option<ProfileKey>,
    ) -> Result<RunView, RunRefusal> {
        spec.lease_id = lease.to_string();
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State {
            book,
            records,
            runs,
            ..
        } = &mut *guard;
        let record = records.get(host).ok_or(RunRefusal::HostNotRegistered)?;
        let shape = self.shared.config.booked();
        vet(&spec, shape.resources())?;
        if !spec.files.is_empty() && !record.files {
            return Err(RunRefusal::FilesUnsupported);
        }
        if spec.limits.disk_bytes.is_some() && record.quota_default.is_none() {
            return Err(RunRefusal::DiskUnsupported);
        }
        if record.state(now, self.grace_ms()) != HostState::Live {
            return Err(RunRefusal::HostLost);
        }
        let booked = Observed::Present { host: host.clone() };
        if book.read(lease) != booked {
            return Err(RunRefusal::NotHeld);
        }
        if record.taint.is_some() {
            return Err(RunRefusal::HostTainted);
        }
        if record.exclusive.awaits_ready() {
            return Err(RunRefusal::NotReady);
        }
        // A host being scaled away takes no run, as one out of the serving phase takes none,
        // with one exception: a retired host (`Registry::retire`, at its maximum age) still
        // runs work for a lease booked on it, which the check above has just shown this is,
        // so a retire cuts no lease. No new lease is booked on a marked host. A plainly
        // drained host (set only on a quiet host) and a releasing one take nothing.
        let leaving = [None, Some(marks::Leaving::Retiring)].contains(&record.leaving);
        let takes_runs = (record.phase == Phase::Serving) & leaving;
        if !takes_runs {
            return Err(RunRefusal::NoCapacity);
        }
        let seen = (record.beats, now.millis());
        runs.admit(
            host,
            lease,
            (spec, profile),
            shape.resources(),
            record.quota_default,
            seen,
        )
    }

    /// The profile name the run `run_id` was queued with, or `None` when it was queued
    /// without one or the registry does not know the run (a restart forgets every run).
    pub fn profile_of(&self, run_id: &str) -> Option<ProfileKey> {
        self.lock().runs.profile_of(run_id)
    }

    /// Asks for the run `run_id` of `lease` to stop: a retained intent, carried as a stop
    /// on every heartbeat reply until the host no longer lists the run. A run that has
    /// ended is left as it is; the answer is its state. [`RunRefusal::UnknownRun`] when
    /// the lease has no such run.
    pub fn stop_run(&self, lease: &LeaseId, run_id: &str) -> Result<RunView, RunRefusal> {
        self.lock().runs.stop_run(lease, run_id)
    }

    /// The runs of `lease`, oldest first, finished ones included while the lease holds
    /// its host (at most [`crate::MAX_FINISHED_RUNS`] of them per host).
    pub fn runs_of(&self, lease: &LeaseId) -> Vec<RunView> {
        self.lock().runs.of_lease(lease)
    }

    /// Reserves `ids` to another source of hosts: a hello for one is refused as
    /// host-listed, whoever sends it. A host id then belongs to one source only, which
    /// keeps a consumer from being offered two hosts of one name. The reservation is in
    /// memory and adds to what was reserved before; the caller makes it again at every
    /// start, before the registry takes a hello.
    pub fn reserve(&self, ids: impl IntoIterator<Item = HostId>) {
        self.lock().reserved.extend(ids);
    }

    /// The ids reserved so far, in id order ([`Registry::reserve`]).
    pub fn reserved(&self) -> BTreeSet<HostId> {
        self.lock().reserved.clone()
    }

    /// What every known host id is bound to, in id order, with the taint the host
    /// reported when it has. A caller that persists bindings writes this, so that a restart
    /// can hand the bindings to [`crate::RegisteredProvider::seat`] and the tainted hosts
    /// to [`Registry::restore_tainted`].
    pub fn bindings(&self) -> BTreeMap<HostId, Binding> {
        let state = self.lock();
        let bound = state.records.iter();
        let bound = bound.filter_map(|(id, record)| {
            let binding = record.binding.clone()?;
            Some((id.clone(), binding.with_taint(record.taint.clone())))
        });
        bound.collect()
    }

    /// Puts back a host that reported a taint before a restart, whether or not it holds a
    /// lease: an awaiting host, bound as it was and tainted, that counts toward the pool's
    /// limit and is not forgotten for being silent. A host the registry already knows is
    /// left as it is. `binding` is what [`Registry::bindings`] returned for it; one that
    /// carries no taint is ignored, since a host that was never tainted is put back by
    /// [`crate::RegisteredProvider::seat`] when it holds a lease and by nothing else.
    pub fn restore_tainted(&self, host: &HostId, binding: Binding) {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State { book, records, .. } = &mut *guard;
        if binding.taint().is_none() || records.contains_key(host) {
            return;
        }
        self.shared.config.list_in(book, host.clone());
        records.insert(host.clone(), Record::awaiting(Some(binding), now));
    }

    /// Every host the registry holds, in id order.
    pub fn hosts(&self) -> Vec<HostView> {
        let now = self.shared.clock.now();
        let state = self.lock_at(now);
        self.host_views(&state, now)
    }

    /// The hosts ([`Registry::hosts`]) and the wait list ([`Registry::waiting`]) as of one
    /// moment, read under one lock: a request is not shown as reserved on a host that the
    /// hosts do not show as set aside, nor the other way round.
    pub fn snapshot(&self) -> (Vec<HostView>, Vec<Waiting>) {
        let now = self.shared.clock.now();
        let state = self.lock_at(now);
        (self.host_views(&state, now), waiting_rows(&state))
    }

    fn host_views(&self, state: &State, now: Timestamp) -> Vec<HostView> {
        let booked = state.book.booked();
        let shape = self.shared.config.booked();
        let memory = self.shared.config.memory_headroom();
        let marks = state.marks.reservations();
        let set_aside = |id: &HostId| marks.iter().find(|mark| mark.host == *id);
        let views = state.records.iter().map(|(id, record)| HostView {
            id: id.clone(),
            state: record.state(now, self.grace_ms()),
            load: record
                .load
                .latest
                .clone()
                .filter(|_| record.state(now, self.grace_ms()) != HostState::Lost),
            phase: record.phase,
            lease: booked.get(id).copied(),
            shape: shape.clone(),
            memory,
            claim: record.claim.clone(),
            labels: record.labels.clone(),
            agent_version: record.agent_version.clone(),
            epoch: record.epoch,
            heard_since_hello: record.session.is_none() | (record.beats > 0),
            releasing: !booked.contains_key(id)
                & !(record.quiet(&state.runs, id) & record.exclusive.is_free()),
            taint: record.taint.clone(),
            mark: match (record.leaving, set_aside(id), record.exclusive.held_since()) {
                (Some(leaving), _, _) => Some(leaving.mark()),
                (None, Some(_), _) => Some(HostMark::Reserved),
                (None, None, Some(_)) => Some(HostMark::Held),
                (None, None, None) => None,
            },
            reserved_for: set_aside(id).map(|mark| mark.request.clone()),
            drain_deadline: set_aside(id)
                .filter(|mark| !mark.assigned)
                .map(|mark| mark.deadline),
        });
        views.collect()
    }
}
