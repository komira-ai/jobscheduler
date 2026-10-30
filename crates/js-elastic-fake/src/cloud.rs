//! The fake cloud: an [`ElasticProvider`] over memory that keeps its own bill.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, CheckResult, Claim, ElasticProvider, Held,
    HostRef, HostStatus, Offer, ProviderError, ReadHandle, Reason, Refusal, RefusalCode, Released,
};

use crate::bill::{Bill, Billing};
use crate::clock::{FakeClock, millis};
use crate::script::{Call, Script, Step, THROTTLE_WAIT};

/// The most payload the fake carries, in bytes.
pub const PAYLOAD_LIMIT: usize = 1 << 20;

/// The limits and delays of the cloud, which a test sets as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knobs {
    /// The most machines that may run at once before an acquire is refused as
    /// [`RefusalCode::Quota`]. `None`: no limit.
    pub quota: Option<usize>,
    /// The most machines the cloud has room for before an acquire is refused as
    /// [`RefusalCode::Capacity`]. `None`: no limit.
    pub capacity: Option<usize>,
    /// How long a machine is `booting` before it reads `active`.
    pub boot: Duration,
    /// How long a list lags the truth: it shows the cloud as it was this long ago. A
    /// machine launched less than this ago is not in it yet, and a release shows in it
    /// this late.
    pub list_lag: Duration,
    /// How long a released machine reads `terminating` before a read by id says it is
    /// gone.
    pub remove_lag: Duration,
    /// How long an acquire whose answer was lost stays unresolved, unless a list shows
    /// its machine first.
    pub unknown_hold: Duration,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            quota: None,
            capacity: None,
            boot: Duration::from_secs(120),
            list_lag: Duration::ZERO,
            remove_lag: Duration::from_secs(20),
            unknown_hold: Duration::from_secs(30 * 60),
        }
    }
}

/// One machine the cloud made.
struct Machine {
    id: HostRef,
    acquire: String,
    name: String,
    offer: Offer,
    /// The price per hour in the cloud's own catalog when it was launched, not what the
    /// caller believed.
    price: u64,
    launched: u64,
    /// When it stopped running and billing: released, or preempted.
    ended: Option<u64>,
    /// When a release took effect.
    released: Option<u64>,
    user_data: String,
}

impl Machine {
    /// A release took effect at `now`. Billing stops then, unless the machine had
    /// already been preempted.
    fn end(&mut self, now: u64) {
        self.ended = self.ended.or(Some(now));
        self.released = self.released.or(Some(now));
    }
}

struct Inner {
    clock: FakeClock,
    billing: Billing,
    offers: Vec<Offer>,
    knobs: Knobs,
    machines: Vec<Machine>,
    scripts: BTreeMap<Call, Script>,
    calls: Vec<Call>,
    /// The acquires whose answer was lost, and when.
    unresolved: BTreeMap<AcquireId, u64>,
}

fn refusal(code: RefusalCode, reason: Reason) -> Refusal {
    Refusal { code, reason }
}

impl Inner {
    /// Records `call` and returns what its script says it does.
    fn enter(&mut self, call: Call) -> Step {
        self.calls.push(call);
        self.scripts.entry(call).or_default().take()
    }

    /// What the cloud held of `machine` at time `t`, or `None` when it did not show yet
    /// or has been removed.
    fn status_at(&self, machine: &Machine, t: u64) -> Option<HostStatus> {
        let released = machine.released.filter(|at| *at <= t);
        let ended = machine.ended.filter(|at| *at <= t);
        let removed = released.is_some_and(|at| at + millis(self.knobs.remove_lag) <= t);
        let booted = machine.launched + millis(self.knobs.boot) <= t;
        let status = match (released, ended, booted) {
            (Some(_), _, _) => HostStatus::Terminating,
            (None, Some(_), _) => HostStatus::Preempted,
            (None, None, false) => HostStatus::Booting,
            (None, None, true) => HostStatus::Active,
        };
        (machine.launched <= t && !removed).then_some(status)
    }

    fn held(machine: &Machine, status: HostStatus) -> Held {
        Held {
            id: machine.id.clone(),
            acquire: machine.acquire.clone(),
            name: Some(machine.name.clone()),
            offer: machine.offer.id.clone(),
            zone: machine.offer.zone.clone(),
            status,
        }
    }

    /// What `machine` is billed for between `from` and `to` milliseconds, in cent-milliseconds:
    /// from its launch for the provider's rounding of the time it ran (a running machine as
    /// if it ended now), at the price its offer has in this cloud's catalog.
    fn owed(&self, machine: &Machine, from: u64, to: u64) -> u128 {
        let now = self.clock.now_ms();
        let run = machine
            .ended
            .unwrap_or(now)
            .saturating_sub(machine.launched);
        let billed = self.billing.billed_ms(run);
        let start = machine.launched.max(from);
        let stop = (machine.launched + billed).min(to);
        u128::from(machine.price) * u128::from(stop.saturating_sub(start))
    }

    /// The machines as the cloud showed them `lag` ago.
    fn view(&self, lag: Duration) -> Vec<Held> {
        let seen = self.clock.now_ms().saturating_sub(millis(lag));
        let shown = self.machines.iter().filter_map(|machine| {
            let status = self.status_at(machine, seen);
            status.map(|status| Self::held(machine, status))
        });
        shown.collect()
    }

    /// Forgets the acquires whose hold has run out.
    fn expire(&mut self) {
        let (now, hold) = (self.clock.now_ms(), millis(self.knobs.unknown_hold));
        self.unresolved.retain(|_, since| *since + hold > now);
    }

    /// Makes a machine for `request`, if the catalog, the quota and the room allow.
    fn launch(&mut self, request: &AcquireRequest) -> Result<Held, Refusal> {
        let wanted = (&request.offer.id, &request.offer.zone);
        let found = self
            .offers
            .iter()
            .find(|offer| (&offer.id, &offer.zone) == wanted);
        let Some(offer) = found else {
            return Err(refusal(RefusalCode::Shape, Reason::NotAllowed));
        };
        let running = self.machines.iter().filter(|m| m.ended.is_none()).count();
        let over_quota = self.knobs.quota.is_some_and(|quota| running >= quota);
        let full = self.knobs.capacity.is_some_and(|room| running >= room);
        if over_quota {
            return Err(refusal(RefusalCode::Quota, Reason::Api("quota".to_owned())));
        }
        if full {
            return Err(refusal(
                RefusalCode::Capacity,
                Reason::Api("capacity".to_owned()),
            ));
        }
        let machine = Machine {
            id: HostRef(format!("fake-{:04}", self.machines.len() + 1)),
            acquire: request.acquire.as_str().to_owned(),
            name: request.name.to_string(),
            price: offer.price_cents_per_hour.unwrap_or_default(),
            offer: offer.clone(),
            launched: self.clock.now_ms(),
            ended: None,
            released: None,
            user_data: request.payload.expose().to_owned(),
        };
        let held = Self::held(&machine, HostStatus::Booting);
        self.machines.push(machine);
        Ok(held)
    }
}

/// An elastic cloud in memory.
///
/// It implements [`ElasticProvider`], so a scaler, a registry and a server can run against
/// it, and it scripts what real clouds do: capacity refusal, quota, denial, a request that
/// does not fit, preemption, a call that gets no answer (and may have landed), a 429
/// storm, a list that fails, a release that is refused, and a list that lags the truth.
///
/// It keeps its own bill: from the times the cloud launched and ended each machine, at the
/// [`Billing`] granularity of the provider it stands for, at the price in its own
/// catalog. The scaler's arithmetic is not consulted, so a test that compares the two
/// can catch an accounting bug. Time is a [`FakeClock`]; the cloud never reads the system
/// clock and never touches a network. It is cheap to clone, and clones are the same cloud.
#[derive(Clone)]
pub struct ElasticFake {
    inner: Arc<Mutex<Inner>>,
}

impl ElasticFake {
    /// A cloud that bills as `billing` and lists `offers`, on `clock`, with no limits and
    /// every call working.
    pub fn new(clock: FakeClock, billing: Billing, offers: Vec<Offer>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                clock,
                billing,
                offers,
                knobs: Knobs::default(),
                machines: Vec::new(),
                scripts: BTreeMap::new(),
                calls: Vec::new(),
                unresolved: BTreeMap::new(),
            })),
        }
    }

    fn state(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cloud's clock (the same time as every clone of it).
    pub fn clock(&self) -> FakeClock {
        self.state().clock.clone()
    }

    /// The cloud seen through its reads only: what a pool in plan mode is given.
    pub fn read_handle(&self) -> ReadHandle {
        ReadHandle::new(Arc::new(self.clone()))
    }

    /// The limits and delays now.
    pub fn knobs(&self) -> Knobs {
        self.state().knobs.clone()
    }

    /// Replaces the limits and delays.
    pub fn set_knobs(&self, knobs: Knobs) {
        self.state().knobs = knobs;
    }

    /// Queues `steps` for `call`, after the ones already queued. Each call of that kind
    /// takes the oldest step; with none queued it takes the step set by
    /// [`ElasticFake::script_forever`] (by default [`Step::Pass`]).
    pub fn script(&self, call: Call, steps: &[Step]) {
        self.state().scripts.entry(call).or_default().queue(steps);
    }

    /// Makes every call of kind `call` that has no queued step take `step`, for good, or
    /// until this is called again. `script_forever(call, Step::Pass)` ends a storm.
    pub fn script_forever(&self, call: Call, step: Step) {
        self.state().scripts.entry(call).or_default().repeat(step);
    }

    /// Every call made so far, in order, whether it worked or not.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// How many times `call` was made.
    pub fn count(&self, call: Call) -> usize {
        self.state()
            .calls
            .iter()
            .filter(|each| **each == call)
            .count()
    }

    /// The cloud reclaims the machine `id`: it stops running and billing now and reads
    /// `preempted` until it is released. An id the cloud does not hold changes nothing.
    pub fn preempt(&self, id: &HostRef) {
        let mut state = self.state();
        let now = state.clock.now_ms();
        if let Some(machine) = state.machines.iter_mut().find(|m| m.id == *id) {
            machine.ended = machine.ended.or(Some(now));
        }
    }

    /// The program that calls the cloud restarts: it forgets the acquires whose answer it
    /// never got. The machines are the cloud's and stay.
    pub fn forget_calls_in_flight(&self) {
        self.state().unresolved.clear();
    }

    /// The machines as the cloud holds them now, with no lag and no scripted failure: what
    /// is true, for a test to check what the scheduler believes against.
    pub fn truth(&self) -> Vec<Held> {
        self.state().view(Duration::ZERO)
    }

    /// The machines that have not ended: running and billing.
    pub fn running(&self) -> Vec<HostRef> {
        let state = self.state();
        let running = state.machines.iter().filter(|m| m.ended.is_none());
        running.map(|m| m.id.clone()).collect()
    }

    /// How many machines the cloud has made so far.
    pub fn launched(&self) -> usize {
        self.state().machines.len()
    }

    /// The user data the `n`th machine launched (from zero) was given.
    ///
    /// # Panics
    ///
    /// When fewer than `n + 1` machines were launched.
    pub fn user_data(&self, n: usize) -> String {
        self.state().machines[n].user_data.clone()
    }

    /// The whole bill: every machine billed for the time it ran, rounded as the provider
    /// rounds, a running one as if it ended now.
    pub fn bill(&self) -> Bill {
        self.bill_between(0, u64::MAX)
    }

    /// The part of the bill whose billed time falls between `from_ms` and `to_ms`
    /// (milliseconds on the cloud's clock). A machine is billed from its launch for the
    /// provider's rounding of the time it ran (a running one, as if it ended now), at the
    /// price its offer had in this cloud's catalog.
    pub fn bill_between(&self, from_ms: u64, to_ms: u64) -> Bill {
        let state = self.state();
        let owed = state
            .machines
            .iter()
            .map(|machine| state.owed(machine, from_ms, to_ms));
        Bill(owed.sum())
    }

    /// The whole bill of the machine `id`; nothing for an id the cloud does not hold.
    pub fn bill_of(&self, id: &HostRef) -> Bill {
        let state = self.state();
        let machine = state.machines.iter().find(|m| m.id == *id);
        Bill(machine.map_or(0, |machine| state.owed(machine, 0, u64::MAX)))
    }
}

impl ElasticProvider for ElasticFake {
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        let mut state = self.state();
        state.calls.push(Call::Catalog);
        Ok(state.offers.clone())
    }

    fn checks(&self) -> Vec<CheckResult> {
        self.state().calls.push(Call::Checks);
        Vec::new()
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        PAYLOAD_LIMIT
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        let mut state = self.state();
        state.calls.push(Call::Acquire);
        state.expire();
        let now = state.clock.now_ms();
        let hold = millis(state.knobs.unknown_hold);
        if let Some(since) = state.unresolved.get(&request.acquire) {
            let wait = (*since + hold).saturating_sub(now);
            return Acquired::Deferred {
                wait: Duration::from_millis(wait),
            };
        }
        let earlier = state
            .machines
            .iter()
            .find(|m| m.acquire == request.acquire.as_str() && m.released.is_none());
        if let Some(machine) = earlier {
            let status = state.status_at(machine, now);
            let held = Inner::held(machine, status.unwrap_or(HostStatus::Booting));
            return Acquired::Accepted {
                held,
                extras: Vec::new(),
            };
        }
        let step = state.scripts.entry(Call::Acquire).or_default().take();
        match step {
            Step::Pass => match state.launch(request) {
                Ok(held) => Acquired::Accepted {
                    held,
                    extras: Vec::new(),
                },
                Err(refused) => Acquired::Refused(refused),
            },
            Step::Refuse(code) => {
                Acquired::Refused(refusal(code, Reason::Api("scripted".to_owned())))
            }
            Step::Lost { landed } => {
                if landed {
                    let _ = state.launch(request);
                }
                state.unresolved.insert(request.acquire.clone(), now);
                Acquired::Unknown
            }
            Step::Throttled => Acquired::Deferred {
                wait: THROTTLE_WAIT,
            },
        }
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        let mut state = self.state();
        match state.enter(Call::List) {
            Step::Pass => {
                let shown = state.view(state.knobs.list_lag);
                state
                    .unresolved
                    .retain(|id, _| !shown.iter().any(|held| held.acquire == id.as_str()));
                Ok(shown)
            }
            Step::Refuse(_) => Err(ProviderError::new("the list was refused")),
            Step::Lost { .. } => Err(ProviderError::new("the list timed out")),
            Step::Throttled => Err(ProviderError::new("the list was throttled (429)")),
        }
    }

    fn release(&self, id: &HostRef) -> Released {
        let mut state = self.state();
        let step = state.enter(Call::Release);
        let now = state.clock.now_ms();
        let Some(at) = state.machines.iter().position(|m| m.id == *id) else {
            return Released::Refused(Claim::Foreign);
        };
        let lands = match step {
            Step::Pass => true,
            Step::Lost { landed } => landed,
            Step::Refuse(_) | Step::Throttled => false,
        };
        if lands {
            state.machines[at].end(now);
        }
        match step {
            Step::Pass => match state.status_at(&state.machines[at], now) {
                Some(status) => Released::Terminating(status),
                None => Released::Gone,
            },
            Step::Refuse(_) => Released::Refused(Claim::Suspect),
            Step::Lost { .. } | Step::Throttled => Released::Unknown,
        }
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        let mut state = self.state();
        state.calls.push(Call::Unresolved);
        state.expire();
        state.unresolved.keys().cloned().collect()
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        let mut state = self.state();
        state.calls.push(Call::IsGone);
        let now = state.clock.now_ms();
        let known = state.machines.iter().find(|m| m.id == *id);
        known.is_none_or(|machine| state.status_at(machine, now).is_none())
    }
}
