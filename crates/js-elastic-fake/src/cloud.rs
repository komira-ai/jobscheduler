//! The fake cloud: a [`CapacityProvider`] over memory that keeps its own bill.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CheckResult, Claim,
    EventStream, Held, Hello, HostRef, HostStatus, Observed, Offer, ProviderCaps, ProviderError,
    ProviderEvent, ReadHandle, Reason, Refusal, RefusalCode, Released,
};

use js_types::ProviderId;

use crate::bill::{Bill, Billing};
use crate::clock::{FakeClock, millis};
use crate::script::{Call, Script, Step, THROTTLE_WAIT};

/// The most payload the fake carries, in bytes.
pub const PAYLOAD_LIMIT: usize = 1 << 20;

/// The fake's name as a provider.
pub const PROVIDER_ID: ProviderId = ProviderId::known("fake");

/// The limits and delays of the cloud, which a test sets as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knobs {
    /// The most machines that may run at once before an acquire is refused as
    /// [`RefusalCode::Quota`]. `None`: no limit. It is the bound
    /// [`CapacityProvider::capacity`] reports.
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
    name: Option<String>,
    /// Whether it carries the caller's mark: launched by an acquire, not added as another
    /// scheduler's or as an unmarked machine of the account.
    marked: bool,
    /// Whether the cloud dropped it: a read by id answers that it does not exist.
    dropped: bool,
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
    /// The step the next scripted call of any kind takes, before its own script.
    next: Option<Step>,
    /// The acquires whose answer was lost, and when.
    unresolved: BTreeMap<AcquireId, u64>,
    /// The machine a lost acquire made when it landed, by acquire id: the last lost
    /// acquire of an id sets or clears it.
    landed: BTreeMap<AcquireId, HostRef>,
    /// Where the notices go: one sender per event stream handed out.
    watchers: Vec<Sender<ProviderEvent>>,
}

fn refusal(code: RefusalCode, reason: Reason) -> Refusal {
    Refusal { code, reason }
}

impl Inner {
    /// Records `call` and returns what its script says it does.
    fn enter(&mut self, call: Call) -> Step {
        self.calls.push(call);
        self.step(call)
    }

    /// The step a scripted call of kind `call` takes: the one set for the next call of any
    /// kind, else its own script's.
    fn step(&mut self, call: Call) -> Step {
        let script = self.scripts.entry(call).or_default();
        self.next.take().unwrap_or_else(|| script.take())
    }

    /// What the cloud held of `machine` at time `t`, or `None` when it did not show yet
    /// or has been removed.
    fn status_at(&self, machine: &Machine, t: u64) -> Option<HostStatus> {
        let released = machine.released.filter(|at| *at <= t);
        let ended = machine.ended.filter(|at| *at <= t);
        let removed =
            released.is_some_and(|at| at + millis(self.knobs.remove_lag) <= t) | machine.dropped;
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
            name: machine.name.clone(),
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

    /// The caller's machines as the cloud showed them `lag` ago.
    fn view(&self, lag: Duration) -> Vec<Held> {
        let seen = self.clock.now_ms().saturating_sub(millis(lag));
        let ours = self.machines.iter().filter(|machine| machine.marked);
        let shown = ours.filter_map(|machine| {
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
        let Some(price) = offer.price_cents_per_hour else {
            return Err(refusal(
                RefusalCode::Shape,
                Reason::Api("unpriced offer".to_owned()),
            ));
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
            id: self.next_id(),
            acquire: request.acquire.as_str().to_owned(),
            name: Some(request.name.to_string()),
            marked: true,
            dropped: false,
            price,
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

    /// The id of the next machine the cloud makes.
    fn next_id(&self) -> HostRef {
        HostRef(format!("fake-{:04}", self.machines.len() + 1))
    }

    /// The marked machines that have not ended (running and billing), and every machine
    /// that has not ended, the account's others included.
    fn running(&self) -> (usize, usize) {
        let live = self.machines.iter().filter(|m| m.ended.is_none());
        let ours = live.clone().filter(|m| m.marked).count();
        (ours, live.count())
    }

    /// The acquires whose answer was lost and whose call made no machine: they may still
    /// land. One whose call did make a machine is that machine, whatever became of it.
    fn unanswered(&self) -> usize {
        let lost = self.unresolved.keys();
        lost.filter(|id| !self.landed.contains_key(*id)).count()
    }
}

/// An elastic cloud in memory.
///
/// It implements [`CapacityProvider`], so a scaler, a registry and a server can run against
/// it, and it scripts what real clouds do: capacity refusal, quota, denial, a request that
/// does not fit, preemption (with a notice on its event stream), a call that gets no answer
/// (and may have landed), a 429 storm, a list that fails, a release that is refused, a
/// list that lags the truth, a machine of the account it did not launch, and a machine
/// the cloud drops.
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
                next: None,
                unresolved: BTreeMap::new(),
                landed: BTreeMap::new(),
                watchers: Vec::new(),
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

    /// Makes the next scripted call, of whatever kind, take `step` before any step queued
    /// for its kind. An acquire takes it only when it reaches the cloud: not when it is
    /// deferred or answered with a machine it launched before.
    pub fn script_next(&self, step: Step) {
        self.state().next = Some(step);
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
    /// `preempted` until it is released, and every event stream open now gets
    /// [`ProviderEvent::Reclaimed`] for it, in the order of the preemptions. An id the
    /// cloud does not hold, or a machine that has already ended, changes nothing and sends
    /// no notice. A machine the caller did not launch ([`ElasticFake::add_foreign`]) is
    /// reclaimed all the same, with no notice: the notices are about the caller's machines.
    pub fn preempt(&self, id: &HostRef) {
        let mut state = self.state();
        let now = state.clock.now_ms();
        let found = state.machines.iter_mut().find(|m| m.id == *id);
        let Some(machine) = found.filter(|m| m.ended.is_none()) else {
            return;
        };
        machine.ended = Some(now);
        if !machine.marked {
            return;
        }
        let notice = ProviderEvent::Reclaimed(id.clone());
        state
            .watchers
            .retain(|watcher| watcher.send(notice.clone()).is_ok());
    }

    /// Adds a machine to the account that no acquire of the caller launched (another
    /// scheduler's, named `name`, or an unmarked one with no name), running from now on
    /// `offer`, and returns its id. A list does not show it, a release refuses it as
    /// foreign and a hello is never matched to it; a read by id finds it, and it counts
    /// toward the quota and the room as every running machine of the account does.
    pub fn add_foreign(&self, name: Option<&str>, offer: Offer) -> HostRef {
        let mut state = self.state();
        let id = state.next_id();
        let machine = Machine {
            id: id.clone(),
            acquire: String::new(),
            name: name.map(str::to_owned),
            marked: false,
            dropped: false,
            price: offer.price_cents_per_hour.unwrap_or(0),
            offer,
            launched: state.clock.now_ms(),
            ended: None,
            released: None,
            user_data: String::new(),
        };
        state.machines.push(machine);
        id
    }

    /// The cloud drops the machine `id` at once, as a delete made outside the scheduler
    /// does: it stops running and billing, and a read of it by id answers that it does not
    /// exist. An id the cloud does not hold changes nothing.
    pub fn drop_machine(&self, id: &HostRef) {
        let mut state = self.state();
        let now = state.clock.now_ms();
        if let Some(machine) = state.machines.iter_mut().find(|m| m.id == *id) {
            machine.ended = machine.ended.or(Some(now));
            machine.dropped = true;
        }
    }

    /// The machines a release has ended, in the order they were launched.
    pub fn released(&self) -> Vec<HostRef> {
        let state = self.state();
        let ended = state.machines.iter().filter(|m| m.released.is_some());
        ended.map(|m| m.id.clone()).collect()
    }

    /// The program that calls the cloud restarts: it forgets the acquires whose answer it
    /// never got. The machines are the cloud's and stay.
    pub fn forget_calls_in_flight(&self) {
        self.state().unresolved.clear();
    }

    /// The caller's machines as the cloud holds them now, with no lag and no scripted
    /// failure: what is true, for a test to check what the scheduler believes against.
    pub fn truth(&self) -> Vec<Held> {
        self.state().view(Duration::ZERO)
    }

    /// The machines that have not ended: running and billing.
    pub fn running(&self) -> Vec<HostRef> {
        let state = self.state();
        let running = state.machines.iter().filter(|m| m.ended.is_none());
        running.map(|m| m.id.clone()).collect()
    }

    /// How many machines the cloud has made so far, the ones [`ElasticFake::add_foreign`]
    /// added included.
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

impl CapacityProvider for ElasticFake {
    fn id(&self) -> ProviderId {
        self.state().calls.push(Call::Id);
        PROVIDER_ID
    }

    fn can_launch(&self) -> bool {
        self.state().calls.push(Call::CanLaunch);
        true
    }

    /// The operating systems, architectures and accelerator models (canonical families) of the offers it lists, and a
    /// whole machine with host ports, as every machine that runs the agent has.
    fn capabilities(&self) -> ProviderCaps {
        let mut state = self.state();
        state.calls.push(Call::Capabilities);
        let offers = &state.offers;
        ProviderCaps {
            os: offers.iter().map(|offer| offer.os.clone()).collect(),
            arch: offers.iter().map(|offer| offer.arch.clone()).collect(),
            accelerators: offers
                .iter()
                .filter_map(|offer| offer.accelerator.as_ref().map(|have| have.model.clone()))
                .collect::<BTreeSet<_>>(),
            whole_host: true,
            host_ports: true,
        }
    }

    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        let mut state = self.state();
        state.calls.push(Call::Catalog);
        Ok(state.offers.clone())
    }

    /// The caller's machines that have not ended, and what the quota leaves of it once
    /// every running machine of the account and every lost acquire that made no machine
    /// (it may still land) are counted (`None` with no quota).
    fn capacity(&self) -> Capacity {
        let mut state = self.state();
        state.calls.push(Call::Capacity);
        state.expire();
        let (ours, running) = state.running();
        let running = running + state.unanswered();
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Capacity {
            supplied: count(ours),
            can_add: state
                .knobs
                .quota
                .map(|quota| count(quota.saturating_sub(running))),
        }
    }

    fn checks(&self) -> Vec<CheckResult> {
        self.state().calls.push(Call::Checks);
        Vec::new()
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        self.state().calls.push(Call::HostPrep);
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        self.state().calls.push(Call::PayloadLimit);
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
        let earlier = state.machines.iter().find(|m| {
            let same = m.acquire == request.acquire.as_str();
            same & m.released.is_none() & m.marked & !m.dropped
        });
        if let Some(machine) = earlier {
            let status = state.status_at(machine, now);
            let held = Inner::held(machine, status.unwrap_or(HostStatus::Booting));
            return Acquired::Accepted {
                held,
                extras: Vec::new(),
            };
        }
        match state.step(Call::Acquire) {
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
                let made = landed.then(|| state.launch(request).ok()).flatten();
                let acquire = request.acquire.clone();
                match made {
                    Some(held) => state.landed.insert(acquire, held.id),
                    None => state.landed.remove(&acquire),
                };
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
        let ours = state.machines.iter().position(|m| (m.id == *id) & m.marked);
        let Some(at) = ours else {
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

    /// A machine of the account in the status it has now; `Absent` for an id the cloud
    /// never issued, a machine whose removal has passed and a dropped one. The read is
    /// scripted as [`Call::IsGone`]: any step but [`Step::Pass`] is a read with no answer.
    fn observe(&self, id: &HostRef) -> Observed {
        let mut state = self.state();
        let step = state.enter(Call::IsGone);
        let now = state.clock.now_ms();
        let known = state.machines.iter().find(|m| m.id == *id);
        let status = known.and_then(|machine| state.status_at(machine, now));
        match (step, status) {
            (Step::Pass, Some(status)) => Observed::Present(status),
            (Step::Pass, None) => Observed::Absent,
            (Step::Refuse(_) | Step::Lost { .. } | Step::Throttled, _) => Observed::Unknown,
        }
    }

    /// The caller's machine that was launched with the hello's name: with the hello's
    /// instance id when it carries one, else the one such machine that still runs (none
    /// when several do, as after a release and a new launch of the same acquire).
    fn verify_instance(&self, hello: &Hello) -> Option<HostRef> {
        let mut state = self.state();
        state.calls.push(Call::VerifyInstance);
        let name = Some(hello.name.as_str());
        let mut named = state
            .machines
            .iter()
            .filter(|m| (m.name.as_deref() == name) & m.marked & !m.dropped);
        let found = match hello.instance_id.as_deref() {
            Some(id) => named.find(|m| m.id.as_str() == id),
            None => {
                let live: Vec<&Machine> = named.filter(|m| m.ended.is_none()).collect();
                live.first().filter(|_| live.len() == 1).copied()
            }
        };
        found.map(|m| m.id.clone())
    }

    /// A stream of the reclaims [`ElasticFake::preempt`] makes from now on.
    fn events(&self) -> Option<EventStream> {
        let (sender, receiver) = mpsc::channel();
        let mut state = self.state();
        state.calls.push(Call::Events);
        state.watchers.push(sender);
        Some(Box::new(receiver.into_iter()))
    }
}
