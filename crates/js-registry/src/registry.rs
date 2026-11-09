use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use js_agent_wire::{Heartbeat, HeartbeatReply, Hello, Phase, Taint, Welcome};
use js_caps::{HostContract, RUNTIME, SCRATCH_QUOTA};
use js_provider::HostBook;
use js_types::sched::usage::NodeLoad;
use js_types::{Clock, Timestamp};
use js_types::{HostId, LeaseId, MemoryHeadroom, ProviderId, Shape};

use crate::config::{PoolConfig, quota_default};
use crate::credential::{Binding, Credential};
use crate::load;
use crate::refusal::Refusal;
use crate::session::{OsSessions, SessionDigest, SessionSource, secret_text};

mod beat;
mod cordon;
mod marks;
mod placer_side;
mod record;
mod taint;
mod view;

use record::Record;

pub use beat::HeartbeatCheck;
pub use cordon::{Cordoned, NotHeld, OwnHost, Uncordoned};
pub use marks::Busy;

/// Where a host's session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// The host was put back after a restart ([`Registry::restore_tainted`]) and has not
    /// said hello since.
    Awaiting,
    /// The host said hello or heartbeat within the grace period.
    Live,
    /// The host has been silent for longer than the grace period.
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
    /// The lease booked first on the host. No lease is booked on a host any more, so this
    /// is always `None`.
    pub lease: Option<LeaseId>,
    /// Every lease booked on the host. Always empty: no lease is booked on a host any
    /// more.
    pub leases: Vec<LeaseId>,
    /// The shape the pool books for the host (not the host's own claim), with its memory
    /// cut to the pool's memory percent: what may be booked on it, also for a pool whose
    /// placer keeps the percent ([`crate::PoolConfig::booking_whole_memory`]).
    pub shape: Shape,
    /// The memory the pool lists for the host, the percent of it that may be booked and
    /// what that comes to (the memory of `shape`).
    pub memory: MemoryHeadroom,
    /// The shape the host announced at its last hello, which the registry checked against
    /// the pool's floor; `None` for a host that has not said hello since a restart. A
    /// scaler compares it with the machine it launched.
    pub claim: Option<Shape>,
    /// Whether the host is placed by the capabilities of `claim` the pool's shape does not
    /// name as well ([`crate::PoolConfig::placing_by_hello`]); see
    /// [`HostView::placement_shape`].
    pub by_hello: bool,
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
    /// Whether the host's last hello or heartbeat listed a run (`held`): a run of the
    /// previous release's lease protocol, which the registry answers with a cancel, so the
    /// host is not yet clear of it.
    pub releasing: bool,
    /// The taint the host reported, once it has: it can no longer be trusted clean, so it
    /// takes no new job. It is kept across a restart and is not cleared by anything in this
    /// crate.
    pub taint: Option<Taint>,
    /// How the host is set aside, if it is: to be scaled away ([`HostMark::Draining`],
    /// [`HostMark::Releasing`]). The registry sets no other mark.
    pub mark: Option<HostMark>,
    /// The request a reserved host is set aside for. Always `None`: the registry reserves
    /// no host for a request.
    pub reserved_for: Option<js_types::RequestId>,
    /// When a reservation drains a busy host. Always `None`: the registry reserves no host
    /// for a request.
    pub drain_deadline: Option<Timestamp>,
    /// When the host's cordon lapses, when it is cordoned ([`Registry::cordon`]): it
    /// takes no new job until then.
    pub cordon_until: Option<Timestamp>,
    /// The latest load the host reported (in the units of [`NodeLoad`]). Only the latest
    /// accepted beat's load is kept: minutes it carries that a consumer has not read before the
    /// next beat are lost to it, and a beat whose reply was lost is sent again, so a consumer
    /// that sums minutes must dedupe them by (lease, minute). Nothing says how much the agent dropped (the wire has no drop counts),
    /// so no minutes does not mean none were dropped. After a lost gap a reviving beat that
    /// repeats the old `seq` leaves the old reading in place; it is stale, not wrong, and carries
    /// its own `mono_usec`. Zero `cpu_throttled_usec` and
    /// `cpu_pressure_some_total_usec` mean not measured. `None` when the
    /// host has not reported one in this session, when its latest heartbeat carried none
    /// (the agent could not measure), and while the host is lost. A new hello clears it.
    pub load: Option<NodeLoad>,
}

/// How a host is set aside: for whole-node use, or to be scaled away. The registry sets
/// only [`HostMark::Draining`] and [`HostMark::Releasing`]; the other two are the
/// vocabulary of the host listing and the scaler's view, which the registry no longer
/// produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMark {
    /// Set aside for a waiting request.
    Reserved,
    /// A whole-node lease is committed on it.
    Held,
    /// Marked by [`Registry::drain`]: it holds no run and takes no new job. Only
    /// [`Registry::undrain`] or [`Registry::release_host`] ends the mark.
    Draining,
    /// Marked by [`Registry::release_host`]: the host is being given back and takes no
    /// job. Nothing clears the mark; [`Registry::remove`] drops the host.
    Releasing,
}

struct State {
    book: HostBook,
    records: BTreeMap<HostId, Record>,
    /// Ids another source of hosts owns; a hello for one is refused.
    reserved: BTreeSet<HostId>,
    /// Hosts that are being given back and are to be marked releasing from their first
    /// hello, whether or not the registry holds them yet ([`Registry::mark_ending`]).
    ending: BTreeSet<HostId>,
}

impl State {
    /// Forgets every host that has been lost for longer than the retention and that `held`
    /// says holds no work, binding included, and answers which. A host `held` says holds work
    /// stays however long it is silent.
    fn forget_lost(
        &mut self,
        (now, grace_ms, retention_ms): (Timestamp, u64, u64),
        held: &dyn Fn(&HostId) -> bool,
    ) -> Vec<HostId> {
        let lost = self
            .records
            .iter()
            .filter(|(id, record)| {
                record.out_of_retention(now, grace_ms, retention_ms) && !held(id)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let forgotten = lost.into_iter().filter(|id| self.book.remove_free(id));
        let forgotten: Vec<HostId> = forgotten.collect();
        for id in &forgotten {
            self.records.remove(id);
        }
        forgotten
    }
}

struct Shared {
    config: PoolConfig,
    clock: Arc<dyn Clock + Send + Sync>,
    sessions: Box<dyn SessionSource>,
    state: Mutex<State>,
    /// The placer the registry tells of its hosts, given at construction
    /// ([`Registry::new`]).
    placer: Arc<dyn crate::Placer>,
}

/// The hosts of one pool that registered themselves.
///
/// A handle: clones share one registry. The transport calls [`Registry::hello`] and
/// [`Registry::heartbeat`] from the agent's requests. Every call takes the one
/// internal mutex for the length of the call and returns; none awaits, and no call
/// holds the mutex while calling out. Time comes only from the clock given at
/// construction: a host is *lost* when the clock reads more than the grace period past
/// its last hello or heartbeat, computed when asked, so nothing ticks. A lost host that
/// its placer holds no work on is forgotten, binding included, once it has been lost for
/// the pool's lost-host retention ([`PoolConfig::with_lost_retention`]), also computed when
/// asked: every call forgets such hosts first, so they stop counting toward the pool's
/// limit ([`Registry::check_heartbeat`], which changes nothing, answers as if it had).
/// A host that its placer holds work on ([`crate::Placer::holds_work_on`]) is never
/// forgotten. An awaiting host (put back after a restart, never heard) is counted from the
/// time it was put back: it is forgotten after the grace period and the retention have
/// passed since then, so an agent that never returns does not hold a place until the next
/// restart.
///
/// A call that panics while it holds the mutex leaves the state as the panic left it: a
/// poisoned mutex is recovered, not propagated.
///
/// # Lock order
///
/// The registry's lock is taken before the placer's, never the reverse: the placer never
/// calls the registry.
#[derive(Clone)]
pub struct Registry {
    shared: Arc<Shared>,
}

impl Registry {
    /// An empty registry for the pool `config` describes, reading time from `clock` and
    /// session secrets from the operating system ([`OsSessions`]), telling `placer` of its
    /// hosts.
    ///
    /// The placer is told when a host says hello, when a heartbeat is accepted and when a
    /// host is forgotten, and is asked which hosts it holds work on ([`crate::Placer`]); the
    /// registry keeps every admission decision.
    pub fn new(
        config: PoolConfig,
        clock: impl Clock + Send + Sync + 'static,
        placer: Arc<dyn crate::Placer>,
    ) -> Self {
        Self::with_sessions(config, clock, OsSessions, placer)
    }

    /// As [`Registry::new`], drawing session secrets from `sessions`. A deterministic
    /// source ([`crate::SequentialSessions`]) makes a test repeatable; production uses
    /// [`Registry::new`].
    pub fn with_sessions(
        config: PoolConfig,
        clock: impl Clock + Send + Sync + 'static,
        sessions: impl SessionSource + 'static,
        placer: Arc<dyn crate::Placer>,
    ) -> Self {
        let state = State {
            book: HostBook::new(),
            records: BTreeMap::new(),
            reserved: BTreeSet::new(),
            ending: BTreeSet::new(),
        };
        Self {
            shared: Arc::new(Shared {
                config,
                clock: Arc::new(clock),
                sessions: Box::new(sessions),
                state: Mutex::new(state),
                placer,
            }),
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
    /// counts them (the job engine, the scaler).
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
    /// bound yet. A host that is lost or only awaiting is replaced.
    /// The welcome carries a fresh session secret from the registry's
    /// [`SessionSource`]; the registry keeps only its digest, and every heartbeat of the
    /// session must carry it ([`Registry::heartbeat`]). The secret of the replaced
    /// session stops working.
    ///
    /// The binding is as strong as what the first hello presented. With an instance
    /// identity it names the machine. Without one it holds at the level of the pool
    /// credential only: whoever holds the pool's token can say hello for the id once
    /// the host is lost. Per-host tokens or client
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
    /// A pool that books no disk and does not name `scratch-quota` refuses (also
    /// `shape-not-accepted`) a host that reports a quota with a default that counts: the
    /// pool would book nothing for jobs the host books at its default.
    ///
    /// The runs a hello lists as held (the previous release's lease protocol) are never
    /// adopted: each is answered with a cancel in the host's next heartbeat reply (a
    /// welcome carries no commands).
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
        let kept_cordon = known.and_then(|record| record.cordon);
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
        // A pool that names `scratch-quota` books a default for every job without disk, so
        // a host must report one that counts and fits the pool's disk: otherwise its jobs
        // without disk would book nothing, or never fit.
        let quota_pool = floor.capabilities().contains_key(SCRATCH_QUOTA);
        let default = quota_default(&hello.capabilities);
        let fits = default.is_some_and(|bytes| bytes <= floor.resources().disk_bytes());
        if [quota_pool, !fits].into_iter().all(|ok| ok) {
            return Err(Refusal::ShapeNotAccepted);
        }
        // A host that enforces a quota books a default for every job without disk, so a
        // pool that books no disk (and does not name `scratch-quota`) would count 0 for
        // jobs the host counts at its default, and the server would admit what the agent
        // refuses. Such a host is refused rather than booked wrongly.
        let books_no_disk = floor.resources().disk_bytes() == 0;
        if [!quota_pool, books_no_disk, default.is_some()]
            .into_iter()
            .all(|ok| ok)
        {
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
        config.list_claimed(&mut state.book, host.clone(), claim.as_ref());
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
                held,
                beats: 0,
                taint: kept_taint,
                cordon: kept_cordon,
                hello_at: Some(now),
                runtime: hello.capabilities.get(RUNTIME).cloned(),
                leaving: kept_leaving,
                claim,
                load: load::Kept::default(),
            },
        );
        // The placer sees the host as the book lists it.
        let listed = state.book.shape_of(&host).cloned();
        listed.iter().for_each(|shape| self.tell_up(&host, shape));
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
    /// that was lost is live again.
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
    /// The reply is the one for a previous release's agent that still keys its work by run
    /// ([`beat::answer`]): a cancel for each run the heartbeat lists as held, at most
    /// [`js_agent_wire::limits::MAX_COMMANDS`], and an `UnknownLease` acknowledgement for
    /// each result it carries; nothing is queued for it. The job half of the reply is added
    /// by the transport.
    ///
    /// **Taint.** The first taint report a host sends is kept, and the reply says
    /// `taint_ack` whenever the heartbeat carries one. The caller must make the taint
    /// durable ([`Registry::bindings`]) before it lets the acknowledgement go, since the
    /// agent stops reporting once it has it. A tainted host takes no new job and is never
    /// forgotten for being silent. Nothing here clears a taint.
    ///
    /// This records the beat: it changes the registry. [`Registry::check_heartbeat`] runs
    /// the same checks and finds the taint the beat would record, changing nothing, so a
    /// caller can make that taint durable first.
    pub fn heartbeat(
        &self,
        heartbeat: &Heartbeat,
        credential: &Credential,
    ) -> Result<HeartbeatReply, Refusal> {
        let host = beat::parse(heartbeat)?;
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let record = beat::admitted(guard.records.get_mut(&host), heartbeat, credential)?;
        let taint = beat::taint_after(record, heartbeat);
        record.heard = Some(now);
        record.phase = heartbeat.phase;
        record.beats = record.beats.saturating_add(1);
        let reading = record.load.take(heartbeat.load.as_ref());
        self.tell_heard(&host, reading);
        record.held = heartbeat
            .held
            .iter()
            .map(|run| run.run_id.clone())
            .collect();
        record.taint = taint;
        Ok(beat::answer(heartbeat))
    }

    /// Reserves `ids` to another source of hosts: a hello for one is refused as
    /// host-listed, whoever sends it. A host id then belongs to one source only, which
    /// keeps a consumer from being offered two hosts of one name. The reservation is in
    /// memory and adds to what was reserved before; the caller makes it again at every
    /// start, before the registry takes a hello.
    pub fn reserve(&self, ids: impl IntoIterator<Item = HostId>) {
        self.lock().reserved.extend(ids);
    }

    /// What every known host id is bound to, in id order, with the taint the host
    /// reported when it has and the cordon that has not lapsed when it is cordoned. A
    /// caller that persists bindings writes this, so that a restart can hand the bindings
    /// to [`Registry::restore_tainted`].
    pub fn bindings(&self) -> BTreeMap<HostId, Binding> {
        let now = self.shared.clock.now();
        let state = self.lock_at(now);
        let bound = state.records.iter();
        let bound = bound.filter_map(|(id, record)| {
            let binding = record.binding.clone()?;
            let cordon = record.cordon.filter(|until| *until > now);
            let binding = binding.with_taint(record.taint.clone());
            Some((
                id.clone(),
                binding.with_cordon(cordon.map(|until| until.millis())),
            ))
        });
        bound.collect()
    }

    /// Taints `host` with `taint` unless it is tainted already: the first taint a host has
    /// stands, whichever side set it (a heartbeat's report or this call). From then on the
    /// host's view ([`Registry::hosts`]) and [`Registry::bindings`] carry the taint, and the
    /// host takes no new job. Returns whether this call set the taint: `false`
    /// when the registry does not hold `host` or its taint was set before.
    pub fn taint_host(&self, host: &HostId, taint: Taint) -> bool {
        let mut guard = self.lock_at(self.shared.clock.now());
        let Some(record) = guard.records.get_mut(host) else {
            return false;
        };
        let was = record.taint.is_some();
        record.taint.get_or_insert(taint);
        !was
    }

    /// Puts back a host that reported a taint, or was cordoned, before a restart: an
    /// awaiting host, bound as it was, tainted or cordoned as it was, that counts toward the
    /// pool's limit and is not forgotten for being silent. A host the registry already knows
    /// is left as it is. `binding` is what [`Registry::bindings`] returned for it; one that
    /// carries no taint and no cordon that is still in force is ignored.
    pub fn restore_tainted(&self, host: &HostId, binding: Binding) {
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State { book, records, .. } = &mut *guard;
        let cordoned = binding
            .cordon_until_ms()
            .is_some_and(|until| until > now.millis());
        let kept = [binding.taint().is_some(), cordoned]
            .into_iter()
            .any(|kept| kept);
        if !kept || records.contains_key(host) {
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
}
