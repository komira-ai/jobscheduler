//! The scaler's shell: [`js_scaler::step`] run on the server's tick.
//!
//! Once per tick the shell reads the registry and the provider, calls the pure step, and
//! runs the actions it returns in order. Everything it does to the world goes through
//! [`Effects`], and every tick runs on the pool for blocking calls, because the calls block
//! (a file sync, a provider's HTTP request). No lock of the registry or the lease table is
//! held across any of them, and neither is the shell's own memory: a tick works on a copy
//! of it and swaps the copy in when it is done, so the status page and the checks never
//! wait for a provider's call.
//!
//! What the shell keeps between ticks, besides the step's [`State`] (whose records are the
//! only part on disk):
//!
//! - the provider's answers to the acquires and releases it ran, which the next step reads
//!   as its input;
//! - the hosts whose join token it could not revoke. A revoke that failed is tried again
//!   at the start of every tick, and the host's `Release` is not sent until the revoke has
//!   gone through, so a machine is never terminated while its token still works;
//! - the alerts it has raised, so that each is said once while it holds.
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
//! - A join token that could not be minted sends no launch: the shell answers the step's
//!   acquire with a deferral, and the step plans it again, minting again.
//! - A machine whose record is `Releasing` is marked ending in the registry before the
//!   step's other calls and before the server admits any hello after a restart
//!   ([`Scaler::new`]), so a host whose release was under way and that says hello again
//!   cannot be booked before its machine is gone.
//! - The shell vouches for what the step cannot know: a registry view is complete only
//!   once the server has been up for [`REGISTRY_SETTLE`] (a host registered before a restart
//!   needs that long to say hello again); a silent host books nothing when the registry
//!   shows no lease on it; a machine is "not found" when a read by id says the provider no
//!   longer has it; the acquire ids the provider has an unresolved call for are the
//!   provider's own.
//! - It says what an operator must act on, once while it holds: an orphan, a record of a
//!   pool the server no longer has, a silent host that books nothing and is still not
//!   marked long past `unknown_hold`; and it logs each machine released on a host's silence.
//! - Nothing here reads a clock or draws a random value directly: the clock is the
//!   server's and the random values for acquire ids come from a [`SessionSource`].

mod alert;
mod build;
mod effects;
mod launch;
mod live;
mod store;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_elastic::{Acquired, HostRef, SchedulerId, Secret};
use js_lease::Clock;
use js_registry::{HostState, HostView, Launchable, SessionSource};
use js_scaler::{
    AcquireResult, Action, Input, Millis, Phase, PoolInput, Records, ReleaseResult, Settings,
    State, launchable, step,
};
use js_types::ExecMode;
use tracing::{debug, warn};

use crate::adapt::SharedClock;
use crate::util::blocking;

pub(crate) use build::{Parts, build};
pub use effects::{Call, Effects};
pub use launch::{KEY_VALID_FOR, Launcher, RETRY_AFTER};
pub(crate) use live::Live;
pub use store::RecordsFile;

/// How long after the server starts the shell does not vouch for the registry's view of
/// its hosts. A host registered before a restart is missing from the registry until its
/// agent says hello again (the agent beats every few seconds, and the registry's grace is
/// 15 s unless the file says more); until then the step must not read a missing host as
/// gone.
pub const REGISTRY_SETTLE: Duration = Duration::from_secs(120);

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
    alerted: BTreeSet<String>,
    save_errors: u64,
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
    started: Millis,
    /// Held for the whole of a tick, so that two ticks never work on the same copy.
    one_tick: Mutex<()>,
    /// Held only to copy [`Memory`] out and to swap it back in, never across a call.
    memory: Mutex<Memory>,
    last: Arc<Mutex<Option<Snapshot>>>,
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
        (scheduler, exec): (SchedulerId, ExecMode),
        settings: Settings,
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
            alerted: BTreeSet::new(),
            save_errors: 0,
        };
        let started = Millis::new(clock.now().millis());
        let core = Core {
            effects,
            clock,
            entropy,
            scheduler,
            exec,
            settings,
            started,
            one_tick: Mutex::new(()),
            memory: Mutex::new(memory),
            last: Arc::new(Mutex::new(None)),
        };
        Self {
            core: Arc::new(core),
        }
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

    /// One tick, on the pool for blocking calls.
    pub async fn tick(&self) {
        let core = Arc::clone(&self.core);
        blocking(move || core.tick()).await;
    }

    /// What the last tick left: the records, why each pool launches nothing, what a plan
    /// pool would launch.
    pub fn state(&self) -> State {
        self.core.memory().state.clone()
    }

    /// How many times the records could not be written since the scaler started.
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
        self.retry_revokes(&mut work);
        let input = self.input(&work, now);
        let (next, actions) = step(work.state.clone(), &input, now);
        let Some(answers) = self.run(&mut work, actions) else {
            *self.memory() = work;
            return;
        };
        self.report(&mut work, (&next, &input), now);
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

    /// Says what the step did that an operator should see: each machine released because
    /// its host was silent, and each alert that was not already said.
    fn report(&self, memory: &mut Memory, (next, input): (&State, &Input), now: Millis) {
        for name in &next.silent_released {
            warn!(host = %name, "released a machine whose host was silent and booked nothing");
        }
        let hold = self.settings.unknown_hold;
        let found = alert::alerts((next, input), now, hold, &memory.unrevoked);
        memory.alerted.retain(|key| found.contains_key(key));
        for (key, message) in found {
            if memory.alerted.insert(key) {
                warn!("{message}");
            }
        }
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

    /// The step's input: the registry, the pool's checks and the provider's list, the
    /// answers of the last tick's calls, and random values for the launches this step may
    /// make.
    fn input(&self, memory: &Memory, now: Millis) -> Input {
        let (health, offers) = self.effects.checks();
        let held = match self.effects.list() {
            Ok(held) => Some(held),
            Err(err) => {
                warn!(%err, "cannot list the provider's machines; nothing is launched");
                None
            }
        };
        let pool = &self.settings.pool;
        let views = self.effects.hosts();
        let not_found = self.not_found(&memory.state, &views);
        let hosts = views
            .into_iter()
            .map(|host| view::host_facts(host, pool, self.exec));
        let draws = (0..self.settings.max_step).filter_map(|_| self.entropy.fresh());
        Input {
            scheduler: self.scheduler.clone(),
            pools: vec![PoolInput {
                settings: self.settings.clone(),
                health,
                offers,
                held,
                unresolved: self.effects.unresolved(),
            }],
            waiting: self
                .effects
                .waiting()
                .into_iter()
                .map(view::waiting)
                .collect(),
            hosts: hosts.collect(),
            registry_complete: now.since(self.started) >= REGISTRY_SETTLE,
            acquired: memory.acquired.clone(),
            released: memory.released.clone(),
            not_found,
            entropy: draws.map(|bytes| tail(&bytes)).collect(),
        }
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
                        Some(token) => self.effects.acquire(&plan, token),
                        None => Acquired::Deferred { wait: RETRY_AFTER },
                    };
                    answers.acquired.push(AcquireResult {
                        acquire: plan.acquire,
                        attempt: plan.attempt,
                        answer,
                    });
                }
                Action::Drain(host) => self.effects.call(Call::Drain, &host),
                Action::Undrain(host) => self.effects.call(Call::Undrain, &host),
                Action::ReleaseHost(host) => self.effects.call(Call::ReleaseHost, &host),
                Action::Revoke(host) => self.revoke(memory, host),
                Action::Release { host, id } => {
                    if memory.unrevoked.contains(&host) {
                        debug!(%host, "the token is not revoked yet; the machine is not released");
                    } else {
                        let answer = self.effects.release(&id);
                        answers.released.push(ReleaseResult { host, answer });
                    }
                }
                Action::ForgetNetwork(host) => self.effects.forget(&host),
                Action::RemoveHost(host) => self.effects.call(Call::Remove, &host),
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

/// The last 16 bytes of a random value: the part that differs between the values of a
/// deterministic source.
fn tail(bytes: &[u8; 32]) -> [u8; 16] {
    let mut out = [0; 16];
    out.copy_from_slice(&bytes[16..]);
    out
}
