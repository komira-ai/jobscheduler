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
//! Locks: the registry tells the placer of its hosts under its own lock, and a pass over the
//! queued jobs ([`super::engine::jobs`]) holds the engine's lock for as long as it decides,
//! which can be long with a long queue. So a call the registry makes never waits for the
//! engine's lock ([`Placer`]): when the engine is free it is told at once, and when a pass
//! holds it the call is kept, in the order the registry made it, and the engine is told
//! first thing when it is next taken ([`SchedPlacer::engine`]). The registry's lock, which the
//! job service's read calls and the agents' heartbeats take on the server's async workers,
//! is so never held while a pass decides. The placer never calls the registry, and a pass
//! takes its own lock, then the engine's, and reads the registry before either. The record
//! of removed hosts has a lock of its own, a leaf, and so have the calls kept for the engine
//! and the record of what pass 2 told of the queued jobs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use js_jobs::{JobId, JobRecord, SizedFrom};
use js_registry::Placer;
use js_sched::queue_status::Outcome;
use js_types::Clock;
use js_types::HostId;
use js_types::Shape;
use js_types::sched::FarmTime;
use js_types::sched::usage::NodeLoad;

use super::engine::jobs::Decided;
use super::engine::jobs_seen::{Kept, Seen};
use super::engine::{Engine, Settings};
use super::sizes::day_of;

/// The placer the registry is given: it keeps the engine, the scheduler that places the
/// queued jobs, and follows the registry's hosts.
pub struct SchedPlacer {
    pub(crate) clock: Arc<dyn Clock + Send + Sync>,
    /// The placer's clock when it was made: the server's start, which the marks of the
    /// whole-host jobs the store held then wait from for their hosts' hellos.
    boot: FarmTime,
    engine: Mutex<Engine>,
    /// The registry's calls that found the engine held by a pass, in the order the registry
    /// made them, until the engine is next taken ([`SchedPlacer::engine`]). Its lock is a
    /// leaf.
    kept: Mutex<Vec<Tell>>,
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
    /// jobs depend on, and every call kept while the engine was held
    /// ([`SchedPlacer::engine_changes`]).
    changes: AtomicU64,
    /// What the last pass over the queued jobs that ran found, while it failed nothing
    /// (`engine::jobs_seen`). Its lock is a leaf.
    pub(crate) seen: Mutex<Option<Seen>>,
    /// The job tables the last pass read ([`SchedPlacer::tables`]). Its lock is a leaf, held
    /// while the tables are read.
    pub(crate) tables: Mutex<Option<Kept>>,
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
            kept: Mutex::default(),
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

    /// `decided` with each placement's [`Placement::sized_from`](js_jobs::Placement::sized_from):
    /// which history sized the job, read from its record in `queued` (what the walk that
    /// decided it sized it from) as that walk saw the history on the day of `now`; none for a
    /// job that named both its CPU and its memory.
    pub(crate) fn sized_from_of(
        &self,
        queued: &[JobRecord],
        decided: Vec<Decided>,
        now: FarmTime,
    ) -> Vec<Decided> {
        let ids: BTreeSet<&JobId> = decided.iter().map(|(job, ..)| job).collect();
        let (day, mut engine) = (day_of(now), self.engine());
        let sizes = engine.sizes();
        let sized: BTreeMap<&JobId, SizedFrom> = queued
            .iter()
            .filter(|record| ids.contains(&record.id))
            .filter_map(|record| Some((&record.id, sizes.sized_from(record, day)?)))
            .collect();
        decided
            .into_iter()
            .map(|(job, host, booked, placement)| {
                let sized_from = sized.get(&job).copied();
                (
                    job,
                    host,
                    booked,
                    js_jobs::Placement {
                        sized_from,
                        ..placement
                    },
                )
            })
            .collect()
    }

    /// The engine, under its lock, told first of the registry's calls that found it held
    /// ([`Placer`]).
    pub fn engine(&self) -> MutexGuard<'_, Engine> {
        let engine = self.engine.lock().unwrap_or_else(PoisonError::into_inner);
        self.caught_up(engine)
    }

    /// The engine, under its lock and told of the kept calls, when no one holds it; `None`
    /// while a pass does.
    fn free_engine(&self) -> Option<MutexGuard<'_, Engine>> {
        let engine = match self.engine.try_lock() {
            Ok(engine) => engine,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        Some(self.caught_up(engine))
    }

    /// `engine` told of the calls kept for it, in the order they came. They were counted
    /// when they were kept ([`SchedPlacer::tell`]).
    fn caught_up<'a>(&self, mut engine: MutexGuard<'a, Engine>) -> MutexGuard<'a, Engine> {
        let kept = std::mem::take(&mut *self.kept.lock().unwrap_or_else(PoisonError::into_inner));
        for tell in kept {
            let _ = tell.apply(&mut engine);
        }
        engine
    }

    /// Tells the engine `tell` now when it is free, counting the change when it answers one;
    /// keeps it for the engine's next taker when a pass holds the engine, counting it as a
    /// change, so the next pass is not skipped for one it has not seen
    /// ([`SchedPlacer::engine_changes`]).
    fn tell(&self, tell: Tell) {
        let changed = match self.free_engine() {
            Some(mut engine) => tell.apply(&mut engine),
            None => {
                self.kept
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(tell);
                // The holder may have let go between the try and the push: an engine free now
                // is told of the kept calls here rather than at its next taker.
                drop(self.free_engine());
                true
            }
        };
        if changed {
            self.changes.fetch_add(1, Ordering::SeqCst);
        }
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

    /// How many times the registry changed the engine, through the [`Placer`] calls that can
    /// change what a pass over the queued jobs reads of it: every call that changes the
    /// engine counts, except a heartbeat with no load reading that gives no allocation up. A
    /// call kept while a pass held the engine counts when it is kept, whatever it changes. The
    /// passes' own changes are not counted (`engine::jobs_seen`).
    pub fn engine_changes(&self) -> u64 {
        self.changes.load(Ordering::SeqCst)
    }

    /// `host`, booked as `shape`, is one a pass may offer jobs although its hello may have been
    /// taken by another server (its `node/` row is live): the engine is told of it as at a
    /// hello ([`Placer::host_up`]) when it does not know the host booked as `shape`, and the
    /// change is counted then only ([`SchedPlacer::engine_changes`]). Unlike a hello, it leaves
    /// a removal the job pass has not settled ([`SchedPlacer::gone`]) as it is.
    pub fn host_seen(&self, host: &HostId, shape: &Shape) {
        let now = self.now();
        self.changing(|engine| {
            let known = engine.knows(host, shape);
            if !known {
                engine.host_up(host, shape, now);
            }
            ((), !known)
        });
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

/// A call of the registry's, as the engine is told it ([`Placer`]).
enum Tell {
    /// [`Placer::host_up`], at the placer's time of the call.
    Up(HostId, Shape, FarmTime),
    /// [`Placer::heard`], at the placer's time of the call.
    Heard(HostId, Option<NodeLoad>, FarmTime),
    /// [`Placer::host_gone`].
    Gone(HostId),
}

impl Tell {
    /// Tells `engine`, answering whether that changed what a pass reads of it.
    fn apply(self, engine: &mut Engine) -> bool {
        match self {
            Tell::Up(host, shape, now) => {
                engine.host_up(&host, &shape, now);
                true
            }
            Tell::Heard(host, load, now) => engine.heard_changed(&host, load.as_ref(), now),
            Tell::Gone(host) => {
                engine.host_gone(&host);
                true
            }
        }
    }
}

/// Each call is made under the registry's lock and never waits for the engine's: the engine
/// is told at once when it is free, else first thing when it is next taken
/// ([`SchedPlacer::tell`]).
impl Placer for SchedPlacer {
    fn host_up(&self, host: &HostId, shape: &Shape) {
        self.removed().remove(host);
        self.tell(Tell::Up(host.clone(), shape.clone(), self.now()));
    }

    fn heard(&self, host: &HostId, load: Option<&NodeLoad>) {
        self.tell(Tell::Heard(host.clone(), load.cloned(), self.now()));
    }

    /// Recorded for the job pass ([`SchedPlacer::gone`]) as well as told to the engine: a
    /// removed host's job allocations are freed only by the pass, never here.
    fn host_gone(&self, host: &HostId) {
        self.tell(Tell::Gone(host.clone()));
        let removal = self.removals.fetch_add(1, Ordering::Relaxed);
        self.removed().insert(host.clone(), removal);
    }

    /// A host the engine holds a job allocation on is not forgotten for silence: only its
    /// confirmation or the provider's absence frees that allocation (design section 6.3).
    /// While a pass holds the engine the answer is that it holds work there, so the host is
    /// kept until a call that finds the engine free asks again.
    fn holds_work_on(&self, host: &HostId) -> bool {
        self.free_engine()
            .is_none_or(|engine| engine.holds_jobs_on(host))
    }
}

#[cfg(test)]
#[path = "place_tests.rs"]
mod tests;
