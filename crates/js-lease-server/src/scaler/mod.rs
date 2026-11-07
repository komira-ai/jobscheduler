//! The scaler's shell: [`js_scaler::step`] run on the server's tick.
//!
//! Once per tick the shell reads the job tables, the registry and the provider,
//! calls the pure step, and runs the actions it returns in order. Everything it does to the world goes through
//! [`Effects`], and every tick runs on the pool for blocking calls, because the calls block
//! (a file sync, a provider's HTTP request). No lock of the registry or the lease table is
//! held across any of them, and neither is the shell's own memory: a tick works on a copy
//! of it and swaps the copy in when it is done, so the status page and the checks never
//! wait for a provider's call.
//!
//! What waits for a machine and which hosts are in use are read from both the job tables
//! (the `jobs` module) and the registry's lease side, until the lease API is deleted: the
//! registry's waiting whole-node requests and then each queued job without an allocation
//! are waiting requests, and a host is busy while a live allocation or a lease is on it. A
//! tick whose read of the tables fails does nothing else, since an unknown host is not an
//! idle one.
//!
//! What the shell keeps between ticks, besides the step's [`State`] (whose records are the
//! only part on disk):
//!
//! - the provider's answers to the acquires and releases it ran, which the next step reads
//!   as its input;
//! - the hosts whose join token it could not revoke. A revoke that failed is tried again
//!   at the start of every tick, and the host's `Release` is not sent until the revoke has
//!   gone through, so a machine is never terminated while its token still works;
//! - the alerts it has raised, so that each is said once while it holds;
//! - the queued jobs set aside for the machines launched for them (the `jobs` module), in
//!   memory only;
//! - the hosts the registry refused to remove (`unremoved`), tried again at every tick. The
//!   set is kept in its own file ([`UnremovedFile`]), or with a store setting in its own key
//!   (the `kept` module), and is a write-ahead: a host is added
//!   to the set and the file is written before the registry is asked to remove it, and the
//!   host is dropped and the file written again only after the registry has removed it. A
//!   refusal therefore needs no write of its own; the file already lists the host. A tick
//!   writes the file at most once after a failed write and counts one error for it, however
//!   many hosts are pending; the write is tried again at the start of the next tick. If the
//!   write-ahead fails the removal is still attempted: when it succeeds there is nothing to
//!   keep, and when it is refused the host stays in memory and the next tick writes it.
//!   After a restart [`Scaler::retry_removals`] adds the file's hosts to the set, and the
//!   first tick tries them: the new registry holds the host of a gone machine when the lease
//!   file seated a lease on it, and nothing else removes it but the registry's own
//!   `forget_lost` (24 hours after the host was lost, and only when no lease is booked on it).
//!   Two windows remain, both with a crash as the second event: between the write of the
//!   records that call the machine gone and the write-ahead of its host (the records are
//!   written first because no call may run before the file holds the records it depends on),
//!   and after a write-ahead that failed, until a later tick writes it. In either the host
//!   is not retried after the restart and is left to the registry's `forget_lost`.
//!
//! The pool's health and offers are the pool's checks' (the `checks` module): the shell asks
//! the provider for no catalog of its own.
//!
//! The rules of the shell, each tested with a fake [`Effects`]:
//!
//! - The records are written first. If they cannot be written the tick does nothing else
//!   and the step's result is dropped, so the next tick decides again from the same state.
//!   No mint, launch, drain, release or revoke runs before the file holds the records they
//!   depend on.
//! - A machine is not released while its host holds work, read again just before the release
//!   ([`Effects::holds_work`]): a lease the registry books on it or a live job allocation.
//!   The registry's own marks look at its leases and runs only, so a job placed between the
//!   tick's read and a mark is caught here, and the release waits until the job is done.
//! - A join token that could not be minted sends no launch: the shell answers the step's
//!   acquire with a deferral, and the step plans it again, minting again.
//! - A machine whose record is `Releasing` is marked ending in the registry before the
//!   step's other calls and before the server admits any hello after a restart
//!   ([`Scaler::new`]), so a host whose release was under way and that says hello again
//!   cannot be booked before its machine is gone.
//! - The shell vouches for what the step cannot know: a registry view is complete only
//!   once the server has been listening for [`REGISTRY_FLOOR`] and every host it knows of
//!   has said hello again or [`REGISTRY_QUIET`] has passed (the `complete` module); a
//!   silent host books nothing when no live job allocation is on it; a machine is "not
//!   found" when a read by id says the provider no longer has it; the acquire ids the
//!   provider has an unresolved call for are the provider's own.
//! - It says what an operator must act on, once while it holds: an orphan, a record of a
//!   pool the server no longer has, a silent host that books nothing and is still not
//!   marked long past `unknown_hold`; and it logs each machine released on a host's silence.
//! - Nothing here reads a clock or draws a random value directly: the clock is the
//!   server's and the random values for acquire ids come from a [`SessionSource`].

mod alert;
mod build;
mod complete;
mod effects;
mod events;
mod jobs;
pub(crate) mod kept;
mod launch;
mod live;
mod store;
mod unremoved;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use js_alert::{AlertSink, Notifier};
use js_elastic::{Acquired, HostRef, Released, SchedulerId, Secret};
use js_jobs::JobId;
use js_lease::{Booking, Clock};
use js_registry::{HostState, HostView, Launchable, SessionSource};
use js_scaler::{
    AcquireResult, Action, Budgets, HostFacts, Input, Millis, Phase, PoolInput, Records,
    ReleaseResult, Settings, State, Waiting, launchable, step,
};
use js_sched::failover::Refused;
use js_types::{ExecMode, HostId, RequestId};
use tracing::{debug, info, warn};

use crate::adapt::SharedClock;
use crate::metrics::Meter;
use crate::sched::engine::jobs::open_to_jobs;
use crate::util::blocking;

/// The most alerts waiting for the sink; beyond it the oldest is dropped and counted.
const ALERT_QUEUE: usize = 64;

pub(crate) use build::{Parts, build};
pub use complete::{REGISTRY_FLOOR, REGISTRY_QUIET};
pub use effects::{Call, Effects, JobTables};
pub use launch::{KEY_VALID_FOR, Launcher, RETRY_AFTER};
pub(crate) use live::Live;
pub use store::RecordsFile;
pub use unremoved::UnremovedFile;

/// What the last tick saw, kept for the registry's question "could a launch serve this
/// request".
struct Snapshot {
    state: State,
    input: Input,
    now: Millis,
}

/// What the shell keeps between ticks.
#[derive(Clone)]
struct Memory {
    state: State,
    acquired: Vec<AcquireResult>,
    released: Vec<ReleaseResult>,
    unrevoked: BTreeSet<String>,
    /// The hosts of machines that are confirmed gone and that the registry still holds
    /// (it refused to remove them); removed again at the start of every tick.
    unremoved: BTreeSet<String>,
    /// Whether `unremoved` differs from what its file holds.
    unremoved_dirty: bool,
    /// Whether a write of the file failed in this tick: no other is tried in it, and the
    /// failure is counted once.
    unremoved_failed: bool,
    /// The alerts that hold now, by key, with their text.
    alerts: BTreeMap<String, String>,
    save_errors: u64,
    /// The queued jobs set aside for the machines launched for them (the `jobs` module).
    reserved: jobs::Reservations,
    /// The count of completed catalog fetches the last tick saw
    /// ([`Effects::catalog_fetches`]).
    catalog_fetches: u64,
}

/// What a tick's calls answered, for the next tick's input.
#[derive(Default)]
struct Answers {
    acquired: Vec<AcquireResult>,
    released: Vec<ReleaseResult>,
}

struct Core {
    effects: Box<dyn Effects>,
    clock: SharedClock,
    entropy: Box<dyn SessionSource>,
    scheduler: SchedulerId,
    exec: ExecMode,
    settings: Settings,
    /// The server-wide totals every step is given.
    budgets: Budgets,
    /// What decides whether the registry's view is complete (the `complete` module).
    gate: Mutex<complete::Gate>,
    /// Held for the whole of a tick, so that two ticks never work on the same copy.
    one_tick: Mutex<()>,
    /// Held only to copy [`Memory`] out and to swap it back in, never across a call.
    memory: Mutex<Memory>,
    last: Arc<Mutex<Option<Snapshot>>>,
    /// Where each alert is sent besides the log; set once, at start. Its delivery runs on a
    /// thread of its own, so a slow or refusing receiver never holds up a tick.
    sink: OnceLock<Mutex<Notifier>>,
    /// The pools whose failure brake an operator has asked to reset and no tick has taken yet.
    /// The next tick takes them and puts them back if its records were not written, so a
    /// request is dropped only by a restart before the write; asking again is harmless.
    brake_resets: Mutex<BTreeSet<String>>,
    /// Where each launch asked of a provider is counted ([`crate::metrics`]); set once, at
    /// start, when the server serves its metrics.
    meter: OnceLock<Meter>,
    /// Whether the provider's notice stream is open (the `events` module).
    listen: Mutex<events::Listen>,
    /// The provider ids of reclaimed machines that no tick has written records for yet. The
    /// next tick takes them and puts them back if its records were not written.
    reclaimed: Mutex<BTreeSet<String>>,
    /// The reclaim notices that named no machine of the records, since the scaler started.
    reclaims_ignored: AtomicU64,
}

/// The scaler of one elastic pool. A handle: clones share one scaler.
#[derive(Clone)]
pub struct Scaler {
    core: Arc<Core>,
}

/// The machines of `records` that are being released.
fn releasing(records: &Records) -> impl Iterator<Item = &str> {
    let all = records.instances.iter();
    all.filter(|record| record.phase == Phase::Releasing)
        .map(|record| record.name.as_str())
}

impl Scaler {
    /// A scaler for the pool `settings` describes, starting from `records` (what
    /// [`RecordsFile::load`] read), on `clock`, with `entropy` for the random part of
    /// each acquire id. `exec` is the mode the pool's hosts run jobs in.
    ///
    /// Before it returns, every machine the records show as being released is marked
    /// ending in the registry through `effects` and queued for another revoke of its
    /// token. The server calls this before it serves anything, so no hello is admitted
    /// for such a machine unmarked.
    pub fn new(
        effects: Box<dyn Effects>,
        clock: SharedClock,
        entropy: Box<dyn SessionSource>,
        wiring: (SchedulerId, ExecMode),
        settings: Settings,
        records: Records,
    ) -> Self {
        let budgets = (settings, Budgets::default());
        Self::budgeted(effects, clock, entropy, wiring, budgets, records)
    }

    /// A scaler as [`Scaler::new`] makes one, whose steps also hold the server-wide
    /// `budgets` ([`Budgets`]).
    pub fn budgeted(
        effects: Box<dyn Effects>,
        clock: SharedClock,
        entropy: Box<dyn SessionSource>,
        (scheduler, exec): (SchedulerId, ExecMode),
        (settings, budgets): (Settings, Budgets),
        records: Records,
    ) -> Self {
        let mut unrevoked = BTreeSet::new();
        for name in releasing(&records) {
            effects.call(Call::MarkEnding, name);
            unrevoked.insert(name.to_owned());
        }
        let memory = Memory {
            state: State::new(records),
            acquired: Vec::new(),
            released: Vec::new(),
            unrevoked,
            unremoved: BTreeSet::new(),
            unremoved_dirty: false,
            unremoved_failed: false,
            alerts: BTreeMap::new(),
            save_errors: 0,
            reserved: jobs::Reservations::default(),
            catalog_fetches: 0,
        };
        let gate = complete::Gate::starting(Millis::new(clock.now().millis()));
        let core = Core {
            effects,
            clock,
            entropy,
            scheduler,
            exec,
            settings,
            budgets,
            gate: Mutex::new(gate),
            one_tick: Mutex::new(()),
            memory: Mutex::new(memory),
            last: Arc::new(Mutex::new(None)),
            sink: OnceLock::new(),
            meter: OnceLock::new(),
            brake_resets: Mutex::new(BTreeSet::new()),
            listen: Mutex::new(events::Listen::Closed),
            reclaimed: Mutex::new(BTreeSet::new()),
            reclaims_ignored: AtomicU64::new(0),
        };
        Self {
            core: Arc::new(core),
        }
    }

    /// Adds the hosts a run before a restart could not remove from the registry (what
    /// [`UnremovedFile::load`] read) to the ones to remove, so that the first tick tries
    /// them again. Called after [`Scaler::new`] has marked the machines being released; a
    /// second call adds to the first.
    pub fn retry_removals(&self, hosts: BTreeSet<String>) {
        self.core.memory().unremoved.extend(hosts);
    }

    /// The server's listeners are bound: the time before the scaler vouches for the
    /// registry's view counts from now, and `leased`, the bookings of the lease file at start
    /// (refused seats included), are hosts it waits to hear from first. No tick adds more
    /// than `cap` to the time counted, when given: a leap of the wall clock is not uptime.
    pub fn listening(&self, cap: Option<Duration>, leased: &[Booking]) {
        let now = Millis::new(self.core.clock.now().millis());
        let mut gate = self
            .core
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        gate.bound((now, cap), leased.iter().map(|b| b.host.to_string()));
    }

    /// Counts each launch the scaler asks a provider for, and each that fails over, with
    /// `meter` ([`crate::metrics`]). Set once, before the first tick; a second call changes
    /// nothing.
    pub(crate) fn set_meter(&self, meter: Meter) {
        let _ = self.core.meter.set(meter);
    }

    /// Sends every alert the scaler raises to `sink` as well as the log: once when it
    /// begins to hold, again every `repeat_after` while it holds, and again if it clears and
    /// returns. Delivery is on a thread of its own with a bounded queue, so a sink that
    /// refuses or hangs never delays a tick; a refused alert is sent again after a wait
    /// that doubles up to a cap, and stays pending until delivered once. Set once, before
    /// the first tick; a second call changes nothing.
    pub fn set_sink(&self, sink: Arc<dyn AlertSink>, repeat_after: Duration) {
        let _ = self
            .core
            .sink
            .set(Mutex::new(Notifier::new(sink, repeat_after, ALERT_QUEUE)));
    }

    /// Waits until every alert handed to the sink has been answered. For tests and for a
    /// program about to exit; a tick never calls it.
    pub fn flush_alerts(&self) {
        if let Some(notifier) = self.core.sink.get() {
            notifier
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .flush();
        }
    }

    /// The server's time at the end of the last tick that finished, in milliseconds; `None`
    /// before the first. The status page shows it, and a watcher outside the server reads
    /// it as the server's heartbeat. A tick that cannot write the scaler's records does
    /// nothing else and does not finish (the disk is full or gone), so the heartbeat stops
    /// then and the watcher's alarm is right to say so.
    pub fn last_tick(&self) -> Option<Millis> {
        let seen = self
            .core
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        seen.as_ref().map(|seen| seen.now)
    }

    /// Whether the last tick vouched for the registry's view; false before the first.
    pub fn registry_complete(&self) -> bool {
        let seen = self
            .core
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        seen.as_ref()
            .is_some_and(|seen| seen.input.registry_complete)
    }

    /// The registry's question "could a launch serve this request", answered from what the
    /// last tick saw: yes when some pool that is live and available has an offer that fits
    /// and the caps allow one more machine. No when no tick has run yet.
    pub fn launchable(&self) -> Launchable {
        let last = Arc::clone(&self.core.last);
        Launchable::new(move |demand| {
            let seen = last.lock().unwrap_or_else(PoisonError::into_inner);
            let allowed = seen
                .as_ref()
                .map(|seen| launchable(&seen.state, &seen.input, seen.now, demand));
            allowed.unwrap_or(false)
        })
    }

    /// One tick, on the pool for blocking calls. Before it, the provider's notice stream is
    /// opened if it is not open and the provider sends notices (the `events` module).
    pub async fn tick(&self) {
        let core = Arc::clone(&self.core);
        blocking(move || {
            core.listen();
            core.tick();
        })
        .await;
    }

    /// Whether the scaler holds the provider's notice stream open: `false` before the first
    /// tick, for a provider that sends no notices, and after a stream ended until the next
    /// tick opens it again.
    pub fn notices_open(&self) -> bool {
        self.core.notices_open()
    }

    /// How many reclaim notices named no machine of the scaler's records since it started.
    /// Such a notice changes nothing.
    pub fn reclaims_ignored(&self) -> u64 {
        self.core.reclaims_ignored.load(Ordering::Relaxed)
    }

    /// Asks for the failure brake of `pool` to be reset at the next tick: its count, its trip
    /// and its cooldown are cleared and the records written. Idempotent. The pool is the
    /// server's own or one the records still hold a brake for (a pool taken out of the file
    /// keeps its trip, and its alert, until this is asked); `false`, and nothing done, for any
    /// other name.
    pub fn reset_brake(&self, pool: &str) -> bool {
        let held = self.core.memory().state.records.brake.contains_key(pool);
        let known = held || pool == self.core.settings.pool;
        if known {
            let mut asked = self
                .core
                .brake_resets
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            asked.insert(pool.to_owned());
        }
        known
    }

    /// The settings the scaler was built with.
    pub fn settings(&self) -> &Settings {
        &self.core.settings
    }

    /// What the last tick left: the records, why each pool launches nothing, what a plan
    /// pool would launch.
    pub fn state(&self) -> State {
        self.core.memory().state.clone()
    }

    /// What an operator must act on now, one line each: what the shell logs once while it
    /// holds, as of the last tick.
    pub fn alerts(&self) -> Vec<String> {
        self.core.memory().alerts.values().cloned().collect()
    }

    /// How many times the records, or the file of hosts the registry refused to remove,
    /// could not be written since the scaler started.
    pub fn save_errors(&self) -> u64 {
        self.core.memory().save_errors
    }
}

impl Core {
    fn memory(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One tick, on a copy of the memory: the lock of the memory is taken to copy it and
    /// to put the copy back, and not while a call is made.
    fn tick(&self) {
        let _one = self.one_tick.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Millis::new(self.clock.now().millis());
        let mut work = self.memory().clone();
        work.unremoved_failed = false;
        self.retry_revokes(&mut work);
        self.retry_removes(&mut work);
        let fetches = self.effects.catalog_fetches();
        let refreshed = fetches != work.catalog_fetches;
        work.catalog_fetches = fetches;
        let Some((mut input, jobs)) = self.input(&mut work, now) else {
            *self.memory() = work;
            return;
        };
        for pool in &mut input.pools {
            pool.catalog_refreshed = refreshed;
        }
        let asked = std::mem::take(
            &mut *self
                .brake_resets
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        input.brake_resets = asked.iter().cloned().collect();
        let reclaimed = std::mem::take(
            &mut *self
                .reclaimed
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        input.reclaimed = reclaimed.iter().cloned().collect();
        let (next, actions) = step(work.state.clone(), &input, now);
        let Some(answers) = self.run(&mut work, actions) else {
            // The records were not written, so the reset is not made and the reclaims are
            // not acted on: ask again.
            let mut again = self
                .brake_resets
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            again.extend(asked);
            let mut pending = self
                .reclaimed
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            pending.extend(reclaimed);
            *self.memory() = work;
            return;
        };
        self.report(&mut work, (&next, &input), now);
        work.reserved.note(&next.launched, &jobs);
        work.state = next.clone();
        work.acquired = answers.acquired;
        work.released = answers.released;
        *self.memory() = work;
        let seen = Snapshot {
            state: next,
            input,
            now,
        };
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(seen);
    }

    /// Tries again the revokes that failed.
    fn retry_revokes(&self, memory: &mut Memory) {
        let pending: Vec<String> = memory.unrevoked.iter().cloned().collect();
        for host in pending {
            if self.effects.revoke(&host) {
                memory.unrevoked.remove(&host);
            }
        }
    }

    /// Tries again the removals of hosts from the registry that it refused. The file is
    /// written once before them (a write that failed earlier) and once after, and not
    /// between: a tick that cannot write counts one error.
    fn retry_removes(&self, memory: &mut Memory) {
        self.flush_unremoved(memory);
        let pending: Vec<String> = memory.unremoved.iter().cloned().collect();
        for host in pending {
            self.ask_removal(memory, &host, false);
        }
        self.flush_unremoved(memory);
    }

    /// Writes the set of hosts the registry refused to remove when it differs from the
    /// file's. After a failed write nothing more is tried in the tick: the failure is
    /// counted once and the next tick writes again.
    fn flush_unremoved(&self, memory: &mut Memory) {
        if !memory.unremoved_dirty || memory.unremoved_failed {
            return;
        }
        match self.effects.save_unremoved(&memory.unremoved) {
            Ok(()) => memory.unremoved_dirty = false,
            Err(why) => {
                warn!(%why, "cannot write the hosts the registry did not remove; it is tried again");
                memory.save_errors += 1;
                memory.unremoved_failed = true;
            }
        }
    }

    /// Takes the host of a machine that is gone out of the registry. A host not yet in the
    /// set is added and the file written first, so that a crash after the registry's refusal
    /// leaves the host in the file; a failed write does not stop the removal.
    fn remove(&self, memory: &mut Memory, host: String) {
        let added = memory.unremoved.insert(host.clone());
        memory.unremoved_dirty |= added;
        self.flush_unremoved(memory);
        self.ask_removal(memory, &host, added);
        self.flush_unremoved(memory);
    }

    /// Asks the registry to remove the host, and drops it from the set when it did. The
    /// refusal is a warning the first time only (`added`): the same refusal at each later
    /// tick (a lease its holder keeps renewing can last a long while) is a debug line, and
    /// the alert says it once for as long as it holds.
    fn ask_removal(&self, memory: &mut Memory, host: &str, added: bool) {
        match self.effects.remove(host) {
            Ok(()) => memory.unremoved_dirty |= memory.unremoved.remove(host),
            Err(why) if added => {
                warn!(%host, %why, "the registry did not remove the host of a machine that is gone; it is tried again");
            }
            Err(why) => {
                debug!(%host, %why, "the registry still does not remove the host of a machine that is gone");
            }
        }
    }

    /// Says what the step did that an operator should see: each machine released because
    /// its host was silent, and each alert that was not already said.
    fn report(&self, memory: &mut Memory, (next, input): (&State, &Input), now: Millis) {
        for name in &next.silent_released {
            warn!(host = %name, "released a machine whose host was silent and booked nothing");
        }
        for refused in &next.refusals {
            log_refusal(refused);
        }
        let pending = (&memory.unrevoked, &memory.unremoved);
        let found = alert::alerts((next, input), now, &self.settings, pending);
        for (key, message) in &found {
            if !memory.alerts.contains_key(key) {
                warn!("{message}");
            }
        }
        if let Some(notifier) = self.sink.get() {
            let at = Duration::from_millis(now.as_millis());
            let mut notifier = notifier.lock().unwrap_or_else(PoisonError::into_inner);
            notifier.pass(at, &found);
        }
        memory.alerts = found;
    }

    /// The machines the provider no longer has, as far as the records and the registry
    /// tell where to look: a registered or draining machine whose host the registry does
    /// not hear from, read by id.
    fn not_found(&self, state: &State, views: &[HostView]) -> Vec<String> {
        let heard: BTreeSet<&str> = views
            .iter()
            .filter(|host| host.state == HostState::Live)
            .map(|host| host.id.as_str())
            .collect();
        let silent = state
            .records
            .instances
            .iter()
            .filter(|record| [Phase::Registered, Phase::Draining].contains(&record.phase))
            .filter(|record| !heard.contains(record.name.as_str()));
        let gone = |id: &String| self.effects.is_gone(&HostRef(id.clone()));
        let asked = silent.filter(|record| record.cloud_id.as_ref().is_some_and(gone));
        asked.map(|record| record.name.clone()).collect()
    }

    /// The step's input: the job tables, the registry's hosts, the pool's checks and the
    /// provider's list, the answers of the last tick's calls, and random values for the
    /// launches this step may make; with the job of each waiting request, by request id.
    /// The reservations of `memory` that no longer hold are let go first (the `jobs`
    /// module). `None`, and nothing read but the tables, when the job tables cannot be
    /// read: demand and the hosts in use are not known, and an unknown host is not idle, so
    /// the tick does nothing and the next one reads again.
    fn input(
        &self,
        memory: &mut Memory,
        now: Millis,
    ) -> Option<(Input, BTreeMap<RequestId, JobId>)> {
        let tables = match self.effects.jobs() {
            Ok(tables) => tables,
            Err(err) => {
                warn!(%err, "cannot read the job tables; the scaler does nothing this tick");
                return None;
            }
        };
        let (health, offers) = self.effects.checks();
        let held = match self.effects.list() {
            Ok(held) => Some(held),
            Err(err) => {
                warn!(%err, "cannot list the provider's machines; nothing is launched");
                None
            }
        };
        let pool = &self.settings.pool;
        let (views, requests) = self.effects.registry();
        let complete = self.registry_complete(&memory.state.records, &views, now);
        let not_found = self.not_found(&memory.state, &views);
        let at = self.clock.now();
        let open: BTreeSet<HostId> = views
            .iter()
            .filter(|view| open_to_jobs(view, at))
            .map(|view| view.id.clone())
            .collect();
        let used = jobs::in_use(&tables.live);
        let hosts: Vec<HostFacts> = views
            .into_iter()
            .map(|host| {
                let allocated = used.contains(&host.id);
                view::host_facts(host, pool, self.exec, allocated)
            })
            .collect();
        // A job read queued and then found among the live allocations was placed between
        // the two reads: it waits for nothing.
        let placed: BTreeSet<&JobId> = tables.live.iter().map(|live| &live.job).collect();
        let whole_host = self.effects.whole_host();
        let queued: Vec<jobs::Queued> = tables
            .queued
            .iter()
            .filter(|job| !placed.contains(&job.id))
            .filter_map(|job| {
                jobs::request_id(&job.id).map(|request| jobs::Queued {
                    job: job.id.clone(),
                    request,
                    demand: jobs::demand(job, &whole_host),
                })
            })
            .collect();
        memory
            .reserved
            .settle(&queued, &memory.state.records, (&hosts, &open));
        // A launch is set aside for a job only when its request id names no lease request
        // as well: a job and a lease request that share an id are matched by fit alone.
        let leases: BTreeSet<RequestId> = requests.iter().map(|each| each.id.clone()).collect();
        let ids = queued
            .iter()
            .filter(|queued| !leases.contains(&queued.request))
            .map(|queued| (queued.request.clone(), queued.job.clone()))
            .collect();
        // The registry's wait list first, as before the job tables were read, then the jobs
        // in queue order.
        let mut waiting: Vec<Waiting> = requests.into_iter().map(view::waiting).collect();
        waiting.extend(memory.reserved.waiting(queued));
        let draws = (0..self.settings.max_step).filter_map(|_| self.entropy.fresh());
        let input = Input {
            scheduler: self.scheduler.clone(),
            pools: vec![PoolInput {
                settings: self.settings.clone(),
                health,
                offers,
                held,
                unresolved: self.effects.unresolved(),
                catalog_refreshed: false,
            }],
            waiting,
            hosts,
            registry_complete: complete,
            acquired: memory.acquired.clone(),
            released: memory.released.clone(),
            not_found,
            entropy: draws.map(|bytes| tail(&bytes)).collect(),
            budgets: self.budgets.clone(),
            brake_resets: Vec::new(),
            reclaimed: Vec::new(),
        };
        Some((input, ids))
    }

    /// Runs the step's actions in order. `None` when the records could not be written,
    /// which is the first action of a step that changed them: nothing else is run.
    fn run(&self, memory: &mut Memory, actions: Vec<Action>) -> Option<Answers> {
        let mut answers = Answers::default();
        let mut tokens: BTreeMap<String, Secret> = BTreeMap::new();
        for action in actions {
            match action {
                Action::Save(records) => {
                    if let Err(why) = self.effects.save(&records) {
                        warn!(%why, "cannot write the scaler's records; nothing else is done this tick");
                        memory.save_errors += 1;
                        return None;
                    }
                    for name in releasing(&records) {
                        self.effects.call(Call::MarkEnding, name);
                    }
                }
                Action::Mint { host, valid_for } => {
                    let minted = self.effects.mint(&host, valid_for);
                    tokens.extend(minted.map(|token| (host, token)));
                }
                Action::Acquire(plan) => {
                    let answer = match tokens.remove(&plan.name) {
                        Some(token) => {
                            let answer = self.effects.acquire(&plan, token);
                            let meter = self.meter.get().unwrap_or(Meter::off());
                            meter.sink().acquire_answered(&plan.pool, &answer);
                            answer
                        }
                        None => Acquired::Deferred { wait: RETRY_AFTER },
                    };
                    answers.acquired.push(AcquireResult {
                        acquire: plan.acquire,
                        attempt: plan.attempt,
                        answer,
                    });
                }
                Action::Drain(host) => self.effects.call(Call::Drain, &host),
                Action::Retire(host) => self.effects.call(Call::Retire, &host),
                Action::Undrain(host) => self.effects.call(Call::Undrain, &host),
                Action::ReleaseHost(host) => self.effects.call(Call::ReleaseHost, &host),
                Action::Revoke(host) => self.revoke(memory, host),
                Action::Release { host, id } => {
                    if memory.unrevoked.contains(&host) {
                        debug!(%host, "the token is not revoked yet; the machine is not released");
                    } else if self.effects.holds_work(&host) {
                        warn!(%host, "the host holds a lease or a job allocation; the machine is not released until it ends");
                    } else {
                        let answer = self.effects.release(&id);
                        say_release(&host, &answer);
                        answers.released.push(ReleaseResult { host, answer });
                    }
                }
                Action::ForgetNetwork(host) => self.effects.forget(&host),
                Action::RemoveHost(host) => self.remove(memory, host),
            }
        }
        Some(answers)
    }

    fn revoke(&self, memory: &mut Memory, host: String) {
        if !self.effects.revoke(&host) {
            memory.unrevoked.insert(host);
        }
    }
}

/// Says an answer to a release that is neither "gone" nor "being terminated": the step
/// repeats the release at every tick, and the machine stays up and is paid for until the
/// provider says otherwise.
fn say_release(host: &str, answer: &Released) {
    match answer {
        Released::Refused(claim) => {
            warn!(%host, ?claim, "the provider refused to release the machine; it is tried again");
        }
        Released::Unknown => {
            warn!(%host, "the provider gave no answer to a release; it is tried again")
        }
        Released::Gone | Released::Terminating(_) => {}
    }
}

/// The last 16 bytes of a random value: the part that differs between the values of a
/// deterministic source.
fn tail(bytes: &[u8; 32]) -> [u8; 16] {
    let mut out = [0; 16];
    out.copy_from_slice(&bytes[16..]);
    out
}

/// The log line of one refused acquire: the provider, offer and zone and the code, and for
/// `quota` the instance type and region passed over and until when (milliseconds since the
/// epoch, as the scaler's clock reads them).
fn log_refusal(refused: &Refused) {
    let zone = refused.zone.as_deref().unwrap_or("-");
    match &refused.passed_over {
        Some((over, until)) => info!(
            provider = %refused.provider,
            offer = %refused.offer,
            zone = %zone,
            code = ?refused.code,
            instance_type = %over.instance_type,
            region = %over.region.as_deref().unwrap_or("-"),
            until_ms = until.millis(),
            "acquire refused: the instance type is passed over in the region until the time given"
        ),
        None => info!(
            provider = %refused.provider,
            offer = %refused.offer,
            zone = %zone,
            code = ?refused.code,
            "acquire refused: the next zone, offer or provider is asked"
        ),
    }
}
