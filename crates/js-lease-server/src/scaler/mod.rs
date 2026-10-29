//! The scaler's shell: [`js_scaler::step`] run on the server's tick.
//!
//! Once per tick the shell reads the registry and the provider, calls the pure step, and
//! runs the actions it returns in order. Everything it does to the world goes through
//! [`Effects`], and every tick runs on the pool for blocking calls, because the calls block
//! (a file sync, a provider's HTTP request). No lock of the registry or the lease table is
//! held across any of them.
//!
//! What the shell keeps between ticks, besides the step's [`State`] (whose records are the
//! only part on disk):
//!
//! - the provider's answers to the acquires and releases it ran, which the next step reads
//!   as its input;
//! - the hosts whose join token it could not revoke. A revoke that failed is tried again
//!   at the start of every tick, and the host's `Release` is not sent until the revoke has
//!   gone through, so a machine is never terminated while its token still works;
//! - the offers of the provider's catalog, read again every [`CATALOG_EVERY`].
//!
//! The rules of the shell, each tested with a fake [`Effects`]:
//!
//! - The records are written first. If they cannot be written the tick does nothing else
//!   and the step's result is dropped, so the next tick decides again from the same state.
//! - A join token that could not be minted sends no launch: the shell answers the step's
//!   acquire with a deferral, and the step plans it again, minting again.
//! - A machine whose record is `Releasing` is marked ending in the registry before the
//!   step's other calls and before the server admits any hello after a restart
//!   ([`Scaler::new`]), so a host whose release was under way and that says hello again
//!   cannot be booked before its machine is gone.
//! - Nothing here reads a clock or draws a random value directly: the clock is the
//!   server's and the random values for acquire ids come from a [`SessionSource`].

mod build;
mod effects;
mod launch;
mod live;
mod store;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_elastic::{Acquired, Offer, SchedulerId, Secret};
use js_lease::Clock;
use js_registry::{Launchable, SessionSource};
use js_scaler::{
    AcquireResult, Action, Health, Input, Millis, Phase, PoolInput, Records, ReleaseResult,
    Settings, State, launchable, step,
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

/// How long the provider's catalog is trusted before it is read again.
pub const CATALOG_EVERY: Duration = Duration::from_secs(30);

/// What the last tick saw, kept for the registry's question "could a launch serve this
/// request".
struct Snapshot {
    state: State,
    input: Input,
    now: Millis,
}

/// What the shell keeps between ticks.
struct Memory {
    state: State,
    acquired: Vec<AcquireResult>,
    released: Vec<ReleaseResult>,
    unrevoked: BTreeSet<String>,
    offers: Vec<Offer>,
    catalog_at: Option<Millis>,
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
            offers: Vec::new(),
            catalog_at: None,
            save_errors: 0,
        };
        let core = Core {
            effects,
            clock,
            entropy,
            scheduler,
            exec,
            settings,
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

    fn tick(&self) {
        let mut memory = self.memory();
        let now = Millis::new(self.clock.now().millis());
        self.retry_revokes(&mut memory);
        let input = self.input(&mut memory, now);
        let (next, actions) = step(memory.state.clone(), &input, now);
        let Some(answers) = self.run(&mut memory, actions) else {
            return;
        };
        memory.state = next.clone();
        memory.acquired = answers.acquired;
        memory.released = answers.released;
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

    /// The provider's catalog, read again when it is older than [`CATALOG_EVERY`] or the
    /// last read failed; whether the pool is available.
    fn refresh_offers(&self, memory: &mut Memory, now: Millis) -> Health {
        let fresh = memory
            .catalog_at
            .is_some_and(|at| now.since(at) < CATALOG_EVERY);
        if fresh {
            return Health::Available;
        }
        match self.effects.catalog() {
            Ok(offers) => {
                memory.offers = offers;
                memory.catalog_at = Some(now);
                Health::Available
            }
            Err(err) => {
                warn!(%err, "cannot read the provider's catalog");
                memory.offers.clear();
                memory.catalog_at = None;
                Health::Failing
            }
        }
    }

    /// The step's input: the registry, the provider's list and catalog, the answers of
    /// the last tick's calls, and random values for the launches this step may make.
    fn input(&self, memory: &mut Memory, now: Millis) -> Input {
        let health = self.refresh_offers(memory, now);
        let held = match self.effects.list() {
            Ok(held) => Some(held),
            Err(err) => {
                warn!(%err, "cannot list the provider's machines; nothing is launched");
                None
            }
        };
        let pool = &self.settings.pool;
        let hosts = self.effects.hosts();
        let hosts = hosts
            .into_iter()
            .map(|host| view::host_facts(host, pool, self.exec));
        let draws = (0..self.settings.max_step).filter_map(|_| self.entropy.fresh());
        Input {
            scheduler: self.scheduler.clone(),
            pools: vec![PoolInput {
                settings: self.settings.clone(),
                health,
                offers: memory.offers.clone(),
                held,
            }],
            waiting: self
                .effects
                .waiting()
                .into_iter()
                .map(view::waiting)
                .collect(),
            hosts: hosts.collect(),
            acquired: memory.acquired.clone(),
            released: memory.released.clone(),
            not_found: Vec::new(),
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
