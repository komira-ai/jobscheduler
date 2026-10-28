use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::refusal as refusal_code;
use js_agent_wire::{
    Heartbeat, HeartbeatReply, Hello, JobSpec, Outcome, Phase, RefusalReason, Taint, Welcome,
};
use js_lease::{Clock, Timestamp};
use js_pool_static::{HostBook, SeatError};
use js_provider::{HostCapacity, Observed, RefusalCode, StartAck, StartSpec};
use js_types::{Admission, HostId, LeaseId, ProviderId, RequestId, ResourceClass, Shape};

use crate::config::PoolConfig;
use crate::credential::{Binding, Credential};
use crate::refusal::Refusal;
use crate::runs::{RunRefusal, RunView, Runs, Standing, vet};
use crate::session::{OsSessions, SessionDigest, SessionSource, secret_text};
use crate::whole::ending::Ending;
use crate::whole::exclusive::Exclusive;
use crate::whole::placement::{self, Declined};
use crate::whole::queue::WaitList;
use crate::whole::reasons::Cause;
use crate::whole::requests::{NotReady, RequestKey, RequestRecord, Requests, Rules};

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
    /// Whether the host holds no lease but is not yet clear of the last one: it still has
    /// runs to finish (runs of a lease that ended, or runs it lists that the server does
    /// not know), or it is restoring after a whole-node lease. It takes no new lease until
    /// a heartbeat shows none and, after a whole-node lease, shows it serving.
    pub releasing: bool,
    /// The taint the host reported, once it has: it can no longer be trusted clean, so it
    /// is sent no start and takes no new run. It is kept across a restart and is not
    /// cleared by anything in this crate.
    pub taint: Option<Taint>,
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
    /// The run ids the host listed as held in its last hello or heartbeat.
    held: BTreeSet<String>,
    /// The heartbeats taken in this session.
    beats: u64,
    /// The first taint the host reported. It outlives a session and, through the file of
    /// bindings, a restart.
    taint: Option<Taint>,
    /// Where the host stands in a whole-node lease; `Free` for a host that never held one.
    exclusive: Exclusive,
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
            held: BTreeSet::new(),
            beats: 0,
            exclusive: Exclusive::Free,
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
    /// serving, `quiet` (no run left over from the last lease), not tainted and clear of
    /// a whole-node lease (not held, not restoring). Every other phase takes no new
    /// lease. A lease already on a tainted host is untouched.
    fn bookable(&self, now: Timestamp, grace_ms: u64, quiet: bool) -> bool {
        let live = self.state(now, grace_ms) == HostState::Live;
        let serving = self.phase == Phase::Serving;
        let clean = self.taint.is_none();
        let clear = self.exclusive.is_free();
        [live, serving, quiet, clean, clear]
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
    /// order: the pool is this one; the host id is not reserved to another source
    /// ([`Registry::reserve`]); the host id is not bound to another credential, nor
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
                exclusive: kept_exclusive,
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
    /// booked shape alone ([`RunRefusal::LimitsExceedShape`]); [`RunRefusal::HostLost`]
    /// for a host not heard within the grace period; [`RunRefusal::NotHeld`] when the
    /// registry's booking of `host` is not `lease`; [`RunRefusal::HostTainted`] for a
    /// host that reported a taint; [`RunRefusal::NoCapacity`] when the
    /// host is out of the serving phase or the run does not fit what is left of the
    /// booked shape (the limits of the host's live runs, the GPU indices they hold, and
    /// [`crate::MAX_LIVE_RUNS`]); [`RunRefusal::RunExists`] for an id in use. A refusal
    /// changes nothing.
    ///
    /// A run's limits count against the budget until it leaves the host's held list, and
    /// not before: the host drops a run from that list only after its container and
    /// scratch are gone.
    pub fn queue_run(
        &self,
        host: &HostId,
        lease: &LeaseId,
        mut spec: JobSpec,
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
        let shape = self.shared.config.shape();
        vet(&spec, shape.resources())?;
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
        if record.phase != Phase::Serving {
            return Err(RunRefusal::NoCapacity);
        }
        let seen = (record.beats, now.millis());
        runs.admit(host, lease, spec, shape.resources(), seen)
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
        book.insert_host(host.clone(), self.shared.config.shape().clone());
        records.insert(host.clone(), Record::awaiting(Some(binding), now));
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
            releasing: !booked.contains_key(id)
                & !(record.quiet(&state.runs, id) & record.exclusive.is_free()),
            taint: record.taint.clone(),
        });
        views.collect()
    }

    /// Books `spec`'s lease as [`js_pool_static::HostBook`] does. A `Whole` class is
    /// committed, not granted: the booking is made only on a host that is bookable (live,
    /// serving, clean), quiet (no held run, none live here) and clear of another
    /// whole-node lease or a restore, and the answer is `Unknown` so the lease stays
    /// pending. A request that used up its tries is answered
    /// `Refused(ExclusiveNotReady)` once (see [`Registry::not_ready`]). One request id holds at most one whole host: a start
    /// of an id that another lease of this pool holds, committed or ready, books nothing
    /// and is answered `NoCapacity` (it waits) until that lease ends. Only a `Whole` class
    /// is held to those rules; a `BookAll` start never reads a request's record. It reads `Present` once the agent reports the host ready
    /// ([`Registry::heartbeat`]), and a repeated start answers `Unknown` until then and
    /// `Started` after.
    pub(crate) fn start(&self, classes: &[ResourceClass], spec: &StartSpec) -> StartAck {
        let now = self.shared.clock.now();
        let grace = self.grace_ms();
        let mut guard = self.lock_at(now);
        let State {
            book,
            records,
            runs,
            requests,
            waiting,
            ..
        } = &mut *guard;
        let lease = spec.lease();
        let key = RequestKey::of(spec);
        let whole = matches!(spec.class().admission(), Admission::Whole);
        let held = book.read(&lease);
        let pending = match &held {
            Observed::Present { host } => records
                .get(host)
                .is_some_and(|record| record.exclusive.awaits_ready()),
            Observed::Absent | Observed::Unknown => false,
        };
        if pending {
            return StartAck::Unknown;
        }
        if whole {
            // One request id holds at most one whole host: a start of it under another
            // lease waits until the lease that holds it ends.
            let id_held = spec.request_id().is_some();
            if id_held & requests.held_by_other(&key, &lease) {
                return StartAck::Refused(RefusalCode::NoCapacity);
            }
            // A request that used up its tries is not placed again.
            if requests.start(&key, now) {
                return StartAck::Refused(RefusalCode::ExclusiveNotReady);
            }
        }
        let gate = HostBook::class_gate(classes, spec);
        let eligible = |id: &HostId, _: &Shape| {
            let record = records.get(id);
            // Only a whole-node request is kept off a host its earlier tries failed on.
            let barred = whole & requests.excluded(&key, id, now);
            let bookable =
                record.is_some_and(|record| record.bookable(now, grace, record.quiet(runs, id)));
            bookable & !barred
        };
        let ack = book.start_where(spec, gate, &eligible);
        let fresh = !matches!(held, Observed::Present { .. });
        let commit = [whole, fresh].into_iter().all(|ok| ok);
        if let (StartAck::Started { host }, true) = (&ack, commit) {
            // The host was eligible, so it has a record.
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                record.exclusive = Exclusive::Committed { lease, at: now };
            });
            requests.commit(lease, key);
            spec.request_id().into_iter().for_each(|id| {
                waiting.cancel(id);
            });
            return StartAck::Unknown;
        }
        // The book answers `NoFit` when no listed host fits. Every host is booked as the
        // pool's shape, so that happens for a request the pool's shape fits only when
        // no host is listed yet: a host could still join, which is capacity, not fit.
        let floor = self.shared.config.shape();
        let wanted = spec.request();
        let fits_pool = floor.satisfies(&wanted.resources, &wanted.capabilities);
        let declined = matches!(
            ack,
            StartAck::Refused(RefusalCode::NoFit | RefusalCode::NoCapacity)
        );
        match (ack, fits_pool, whole && declined) {
            (_, true, true) => {
                let tainted = records
                    .iter()
                    .filter(|(_, record)| record.taint.is_some())
                    .map(|(host, _)| host.clone())
                    .collect::<BTreeSet<_>>();
                let hosts = book.hosts();
                let declined = Declined {
                    spec,
                    key: &key,
                    now,
                    regain_grace_ms: Rules::of(&self.shared.config).regain_grace_ms,
                    hosts: &hosts,
                    tainted: &tainted,
                };
                placement::answer(waiting, requests, &declined)
            }
            (StartAck::Refused(RefusalCode::NoFit), true, false) => {
                StartAck::Refused(RefusalCode::NoCapacity)
            }
            (other, _, _) => other,
        }
    }

    /// Frees the booking at once, as the book does, and asks every live run of the lease
    /// to stop. The host takes no new lease until a heartbeat lists none of those runs.
    /// After a whole-node lease it takes none until a second heartbeat has followed the
    /// stop (the first may have crossed it) and the last one shows it serving with
    /// nothing held (design `whole-node.md` section 8).
    pub(crate) fn stop(&self, lease: &LeaseId) -> Observed {
        let mut guard = self.lock();
        let State {
            book,
            records,
            runs,
            requests,
            ..
        } = &mut *guard;
        runs.stop_lease(lease);
        requests.release(lease);
        if let Observed::Present { host } = book.read(lease) {
            let record = records.get_mut(&host);
            record.into_iter().for_each(|record| {
                record.exclusive = record.exclusive.stopped();
            });
        }
        book.stop(lease)
    }

    /// `Present` only while the host holding the lease is live and, for a whole-node
    /// lease, ready; a lost or awaiting host, and a whole-node host not yet ready, is
    /// `Unknown`, never `Absent`, so the booking stays.
    pub(crate) fn observe(&self, lease: &LeaseId) -> Observed {
        let now = self.shared.clock.now();
        let mut state = self.lock_at(now);
        self.advance_at(&mut state, now);
        match state.book.read(lease) {
            Observed::Present { host } => {
                let record = state.records.get(&host);
                let live = record
                    .is_some_and(|record| record.state(now, self.grace_ms()) == HostState::Live);
                let ready = record.is_some_and(|record| !record.exclusive.awaits_ready());
                match [live, ready].into_iter().all(|ok| ok) {
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
        let now = self.shared.clock.now();
        let mut guard = self.lock_at(now);
        let State { book, records, .. } = &mut *guard;
        let seated = match book.seat(host, lease) {
            Err(SeatError::Unlisted(_)) => {
                book.insert_host(host.clone(), self.shared.config.shape().clone());
                records.insert(host.clone(), Record::awaiting(binding, now));
                book.seat(host, lease)
            }
            settled => settled,
        };
        // A restored whole-node booking is committed and not ready: the agent's
        // preparation is in memory only, so it must report ready for this lease again
        // before the lease reads present, a run is queued or the host books to anyone.
        // A host that lists held runs is ready: only the ready lease could have runs
        // (see `RegisteredProvider::seat` for where that fails).
        let whole = matches!(self.shared.config.class().admission(), Admission::Whole);
        if let (Ok(()), true) = (&seated, whole) {
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                let resumed = !record.held.is_empty();
                record.exclusive = Exclusive::seated(lease, now, resumed);
            });
        }
        seated
    }
}
