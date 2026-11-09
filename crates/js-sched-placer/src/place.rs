//! The registry's placer, with `js-sched` choosing the host.
//!
//! [`SchedPlacer`] keeps one scheduler for the life of the server ([`Engine`]), built once
//! from the server's configuration: the backfill ceiling (`server.max_lease_wall`), the
//! CPU slack (`--cpu-slack`, js-sched's default 1.1x unless given) and the share of each
//! host's memory it may book (`memory_schedulable_percent` of the pool). The registry
//! tells it of every hello, accepted heartbeat and host it forgets as they happen
//! ([`Placer::host_up`], [`Placer::heard`], [`Placer::host_gone`]), so the engine's picture
//! of the hosts follows the registry's without being rebuilt; a heartbeat's load reading
//! reaches the engine there, as the registry vetted and converted it.
//!
//! The engine places the queued jobs when the server holds the job tables
//! ([`super::engine::jobs`]), and holds their allocations. A job that needs the whole host is
//! queued, marked and committed by the same engine ([`super::engine::whole_jobs`]), from what
//! the job tables hold, with the server's start ([`SchedPlacer::boot`]) deciding how long a
//! stored mark waits for its host to say hello.
//!
//! A host the registry removes is told to the engine and also recorded for the job pass of
//! the tick, which reads it absent and settles the job allocations left on it
//! ([`SchedPlacer::gone`]); a host the engine holds a job allocation on is never forgotten for
//! silence ([`Placer::holds_work_on`]).
//!
//! Locks: the placer's mutex is taken only inside a call the registry makes under its own
//! lock (registry, then placer), and the placer never calls the registry, so the order is
//! fixed. A pass over the queued jobs ([`super::engine::jobs`]) takes its own lock, then the
//! engine's, and reads the registry before either. The record of removed hosts has a lock of
//! its own, a leaf, and so has the record of what pass 2 told of the queued jobs.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_jobs::JobId;
use js_registry::Placer;
use js_sched::queue_status::Outcome;
use js_types::Clock;
use js_types::HostId;
use js_types::Shape;
use js_types::sched::FarmTime;
use js_types::sched::usage::NodeLoad;

use super::engine::jobs_seen::{Read, Seen};
use super::engine::{Engine, Settings};

/// The placer the registry is given: it keeps the engine, the scheduler that places the
/// queued jobs, and follows the registry's hosts.
pub struct SchedPlacer {
    clock: Arc<dyn Clock + Send + Sync>,
    /// The placer's clock when it was made: the server's start, which the marks of the
    /// whole-host jobs the store held then wait from for their hosts' hellos.
    boot: FarmTime,
    engine: Mutex<Engine>,
    /// Held for each pass over the queued jobs, so two passes never decide at once
    /// ([`super::engine::jobs`]); taken before the engine's lock, never under it.
    passes: Mutex<()>,
    /// The hosts the registry removed (given back after the provider read them gone), each
    /// with the number of its removal, until the job pass has settled the job allocations
    /// left on them ([`SchedPlacer::gone`]). Its lock is a leaf.
    gone: Mutex<BTreeMap<HostId, u64>>,
    /// How many removals were recorded.
    removals: AtomicU64,
    /// What pass 2 told of each job since the last pass over the queued jobs, in the order it
    /// came ([`SchedPlacer::note_outcomes`]); the pass takes them all. Its lock is a leaf.
    pub(crate) outcomes: Mutex<BTreeMap<JobId, Vec<Outcome>>>,
    /// How many times the registry changed the engine: the calls the passes over the queued
    /// jobs depend on ([`SchedPlacer::engine_changes`]).
    changes: AtomicU64,
    /// What the last pass over the queued jobs that ran found, while it failed nothing
    /// (`engine::jobs_seen`). Its lock is a leaf.
    pub(crate) seen: Mutex<Option<Seen>>,
    /// The job tables the last pass read ([`SchedPlacer::tables`]). Its lock is a leaf, held
    /// while the tables are read.
    pub(crate) tables: Mutex<Option<Read>>,
    /// How many passes over the queued jobs were skipped, counted for the tests.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) skipped: AtomicU64,
}

impl SchedPlacer {
    /// A placer whose engine is built with `settings`, on `clock` (the registry's).
    pub fn new(settings: Settings, clock: impl Clock + Send + Sync + 'static) -> Self {
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(clock);
        let now = farm_time(&*clock);
        Self {
            engine: Mutex::new(Engine::build(settings, now)),
            passes: Mutex::new(()),
            clock,
            boot: now,
            gone: Mutex::default(),
            removals: AtomicU64::new(0),
            outcomes: Mutex::default(),
            changes: AtomicU64::new(0),
            seen: Mutex::default(),
            tables: Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            skipped: AtomicU64::new(0),
        }
    }

    /// The engine, under its lock.
    pub fn engine(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `change` made to the engine under its lock, then counted
    /// ([`SchedPlacer::engine_changes`]) when it answers that it changed something: after the
    /// change, so a pass that read the count before it reads the engine before the change or
    /// counts it.
    fn changing<T>(&self, change: impl FnOnce(&mut Engine) -> (T, bool)) -> T {
        let mut engine = self.engine();
        let (answer, changed) = change(&mut engine);
        if changed {
            self.changes.fetch_add(1, Ordering::SeqCst);
        }
        drop(engine);
        answer
    }

    /// `change` made to the engine under its lock, and counted ([`SchedPlacer::changing`]).
    fn changed<T>(&self, change: impl FnOnce(&mut Engine) -> T) -> T {
        self.changing(|engine| (change(engine), true))
    }

    /// How many times the registry changed the engine, through the [`Placer`] calls that can
    /// change what a pass over the queued jobs reads of it: every call that changes the
    /// engine counts, except a heartbeat with no load reading that gives no allocation up. The
    /// passes' own changes are not counted (`engine::jobs_seen`).
    pub fn engine_changes(&self) -> u64 {
        self.changes.load(Ordering::SeqCst)
    }

    /// How many passes over the queued jobs were skipped
    /// ([`SchedPlacer::place_jobs_if_changed`]).
    #[cfg(any(test, feature = "test-support"))]
    pub fn passes_skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    /// The lock of one pass over the queued jobs.
    pub(super) fn pass(&self) -> MutexGuard<'_, ()> {
        self.passes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The farm time of the placer's clock when the placer was made: the server's start.
    pub fn boot(&self) -> FarmTime {
        self.boot
    }

    /// The farm time of the placer's clock now.
    pub fn now(&self) -> FarmTime {
        farm_time(&*self.clock)
    }

    fn removed(&self) -> MutexGuard<'_, BTreeMap<HostId, u64>> {
        self.gone.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The hosts the registry removed that the job pass has not settled yet, each with the
    /// number of its removal: the provider reads each of them absent (design section 6.3).
    /// A host that says hello again is taken off at its hello.
    pub fn gone(&self) -> BTreeMap<HostId, u64> {
        self.removed().clone()
    }

    /// Notes `outcomes`, what pass 2 reached for jobs (a refused acquire, a launch), for the
    /// next pass over the queued jobs to record on each job's queue status before its own
    /// outcome (`docs/design/queue-status.md`, section 2): pass 2 writes nothing itself, so the
    /// pass is the one writer of a job's status.
    pub fn note_outcomes(&self, outcomes: impl IntoIterator<Item = (JobId, Outcome)>) {
        let mut noted = self.outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        for (job, outcome) in outcomes {
            noted.entry(job).or_default().push(outcome);
        }
    }

    /// Puts back `earlier`, what pass 2 told of jobs whose write did not land, before what it
    /// told of each since, so the next pass takes them in the order they came.
    pub(super) fn restore_outcomes(&self, earlier: BTreeMap<JobId, Vec<Outcome>>) {
        let mut noted = self.outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        for (job, mut outcomes) in earlier {
            let since = noted.entry(job).or_default();
            outcomes.append(since);
            *since = outcomes;
        }
    }

    /// Takes what pass 2 told since the last pass ([`SchedPlacer::note_outcomes`]).
    pub(super) fn take_outcomes(&self) -> BTreeMap<JobId, Vec<Outcome>> {
        std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Takes off each of `settled`, removals the job pass has settled (every job allocation on
    /// the host freed), unless the host was removed again since.
    pub fn settle_gone(&self, settled: &BTreeMap<HostId, u64>) {
        self.removed()
            .retain(|host, removal| settled.get(host) != Some(&*removal));
    }

    /// What the engine charges `host` by its load, for a test that reads it: `None` for a
    /// host that is no worker of the engine.
    #[cfg(any(test, feature = "test-support"))]
    pub fn load_of(&self, host: &str) -> Option<js_sched::LoadView> {
        self.engine().load_of(host)
    }

    /// How many hosts the engine keeps, for a test that holds it to the registry's.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hosts_kept(&self) -> usize {
        self.engine().hosts_kept()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for SchedPlacer {
    /// A placer with the default settings on a clock that stands still at zero.
    fn default() -> Self {
        let clock = js_types::ManualClock::new(js_types::Timestamp::from_millis(0));
        Self::new(Settings::default(), clock)
    }
}

/// The farm time of `clock`: its milliseconds.
fn farm_time(clock: &dyn Clock) -> FarmTime {
    FarmTime::from_millis(clock.now().millis())
}

impl Placer for SchedPlacer {
    fn host_up(&self, host: &HostId, shape: &Shape) {
        self.removed().remove(host);
        let now = self.now();
        self.changed(|engine| engine.host_up(host, shape, now));
    }

    fn heard(&self, host: &HostId, load: Option<&NodeLoad>) {
        let now = self.now();
        self.changing(|engine| ((), engine.heard_changed(host, load, now)));
    }

    /// Recorded for the job pass ([`SchedPlacer::gone`]) as well as told to the engine: a
    /// removed host's job allocations are freed only by the pass, never here.
    fn host_gone(&self, host: &HostId) {
        self.changed(|engine| engine.host_gone(host));
        let removal = self.removals.fetch_add(1, Ordering::Relaxed);
        self.removed().insert(host.clone(), removal);
    }

    /// A host the engine holds a job allocation on is not forgotten for silence: only its
    /// confirmation or the provider's absence frees that allocation (design section 6.3).
    fn holds_work_on(&self, host: &HostId) -> bool {
        self.engine().holds_jobs_on(host)
    }
}
