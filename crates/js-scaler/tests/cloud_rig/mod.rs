//! A scaler run against the elastic fake cloud on a manual clock. Every test of the cloud
//! files drives the pure `step` with the fake's answers and a clock it moves; nothing
//! reads a real clock, sleeps or opens a socket.
//!
//! [`Sim`] plays the shell: each tick it reads the cloud's list, builds the step's input
//! (the registry's hosts are the cloud's booted machines, which say hello at once), runs
//! the step and runs its actions in order against the cloud. A crash can stop it between
//! two actions, and a restart rebuilds the scaler from the last records it wrote, as the
//! server does after a crash.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_elastic::{
    AcquireRequest, Acquired, BootPayload, ElasticProvider, HostName, HostRef, HostStatus,
    SchedulerId,
};
use js_elastic_fake::{Billing, ElasticFake, FakeClock};
use js_scaler::{
    AcquirePlan, AcquireResult, Action, HostFacts, Mark, Millis, Phase, Records, ReleaseResult,
    Settings, State,
};

use crate::common::{Rig, START, host, offer, request};

/// The pool the simulation runs.
pub const POOL: &str = "gpu";

/// How far the clock moves between two ticks.
pub const TICK: Duration = Duration::from_secs(60);

/// A day, in milliseconds.
pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The price of the one offer, in cents per hour: a cent a minute.
pub const PRICE: u64 = 60;

/// A deterministic random stream (SplitMix64). A seed replays exactly.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn draw(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.draw() % n
    }

    /// True once in `n` draws, on average.
    pub fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

/// Where in a tick that launches a machine the process dies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crash {
    /// Before the records are written: nothing of the tick happened.
    BeforeSave,
    /// After the records are written and before the call is sent.
    AfterSave,
    /// After the call is sent, before its answer is read. Whether the cloud made the machine
    /// is the cloud's script.
    AfterCall,
    /// On the tick after a call was sent, after the list was read and before it was used.
    AfterList,
}

/// The simulation: a scaler, a cloud and a registry that follows the cloud.
pub struct Sim {
    pub rig: Rig,
    pub fake: ElasticFake,
    pub clock: FakeClock,
    /// What is on disk: the records of the last `Save` that ran.
    pub saved: Records,
    /// The registry's marks by host name.
    pub marks: BTreeMap<String, Mark>,
    /// How many requests wait this tick.
    pub waiting: usize,
    /// How many ticks the registry's view is incomplete for after a restart.
    pub grace: usize,
    /// Acquire calls sent so far.
    pub sent: usize,
    /// At each acquire call: the cents the cloud had billed in the 24 hours before it.
    pub window_at_acquire: Vec<u64>,
    /// The names of the machines released, in the order the calls were made.
    pub released: Vec<String>,
    /// Whether the agent of a machine that only an orphan record knows never says hello: its
    /// token was minted by a scaler whose records are lost.
    pub silent_orphans: bool,
    /// Every answer the cloud gave an acquire, in order.
    pub answers: Vec<Acquired>,
    /// Acquires answered `Unknown` whose machine was made all the same.
    pub lost_landed: usize,
    /// Acquires answered `Unknown` that made nothing.
    pub lost_unlanded: usize,
    /// When set, this many consumers always want a whole-node lease, each lasting `lease_wall`
    /// and renewed at once on a host the scaler has not marked; `waiting` is then what is
    /// left unserved, and a host keeps its lease until it ends even after a retire mark. The
    /// worst case of a lease booked just before the retire.
    pub consumers: usize,
    /// The wall time of each lease of `consumers`; `None`: no leases are modelled.
    pub lease_wall: Option<Duration>,
    /// When the lease on each host ends, in milliseconds on the cloud's clock.
    pub lease_end: BTreeMap<String, u64>,
}

impl Sim {
    /// A scaler of the pool `settings` and a cloud that bills as `billing`, listing the one
    /// offer, with every call working.
    pub fn new(settings: Settings, billing: Billing) -> Self {
        let clock = FakeClock::new(START);
        let fake = ElasticFake::new(clock.clone(), billing, vec![offer("a10", PRICE)]);
        Self {
            rig: Rig::new(settings),
            fake,
            clock,
            saved: Records::default(),
            marks: BTreeMap::new(),
            waiting: 0,
            grace: 0,
            sent: 0,
            window_at_acquire: Vec::new(),
            released: Vec::new(),
            silent_orphans: false,
            answers: Vec::new(),
            lost_landed: 0,
            lost_unlanded: 0,
            consumers: 0,
            lease_wall: None,
            lease_end: BTreeMap::new(),
        }
    }

    /// The time now, as the scaler reads it.
    pub fn now(&self) -> Millis {
        Millis::new(self.clock.now_ms())
    }

    /// The registry's hosts: each machine the cloud says is active has said hello.
    fn hosts(&self) -> Vec<HostFacts> {
        let active = self
            .fake
            .truth()
            .into_iter()
            .filter(|held| held.status == HostStatus::Active);
        let orphans: Vec<&str> = self
            .records()
            .iter()
            .filter(|record| record.phase == Phase::Orphan)
            .map(|record| record.name.as_str())
            .collect();
        active
            .filter_map(|held| held.name)
            .filter(|name| !(self.silent_orphans && orphans.contains(&name.as_str())))
            .map(|name| HostFacts {
                mark: self.marks.get(&name).copied(),
                busy: self
                    .lease_end
                    .get(&name)
                    .is_some_and(|end| *end > self.clock.now_ms()),
                ..host(&name, POOL)
            })
            .collect()
    }

    /// Sends the acquire `plan` describes, as the server's launcher does.
    fn send(&mut self, plan: &AcquirePlan) -> Acquired {
        let now = self.clock.now_ms();
        let window = self.fake.bill_between(now.saturating_sub(DAY_MS), now);
        self.window_at_acquire.push(window.cents_up());
        self.sent += 1;
        let scheduler = SchedulerId::new("sched").unwrap();
        let before = self.fake.launched();
        let answer = self.fake.acquire(&AcquireRequest {
            acquire: plan.acquire.clone(),
            pool: plan.pool.clone(),
            name: HostName::derive(&scheduler, &plan.acquire),
            offer: plan.offer.clone(),
            lifetime: plan.lifetime,
            labels: Vec::new(),
            payload: BootPayload::new(format!("token-of-{}", plan.name)),
        });
        if answer == Acquired::Unknown {
            let landed = self.fake.launched() > before;
            self.lost_landed += usize::from(landed);
            self.lost_unlanded += usize::from(!landed);
        }
        self.answers.push(answer.clone());
        answer
    }

    /// Books a lease for each consumer on a free host the scaler has not marked, and leaves
    /// `waiting` as the consumers that have no lease. A no-op without `lease_wall`.
    fn serve(&mut self) {
        let Some(wall) = self.lease_wall else {
            return;
        };
        let now = self.clock.now_ms();
        let wall_ms = u64::try_from(wall.as_millis()).unwrap_or(u64::MAX);
        let names: Vec<String> = self
            .hosts()
            .into_iter()
            .map(|h| h.id.as_str().to_owned())
            .collect();
        self.lease_end.retain(|name, _| names.contains(name));
        let leased = |sim: &Self, name: &String| sim.lease_end.get(name).is_some_and(|e| *e > now);
        let mut busy = names.iter().filter(|name| leased(self, name)).count();
        for name in &names {
            let free = !leased(self, name) && !self.marks.contains_key(name);
            if free && busy < self.consumers {
                self.lease_end.insert(name.clone(), now + wall_ms);
                busy += 1;
            }
        }
        self.waiting = self.consumers.saturating_sub(busy);
    }

    /// One tick: reads the cloud, takes a step and runs its actions, then moves the clock.
    /// With a `crash`, the process dies at that point of the first tick that launches a
    /// machine (for [`Crash::AfterList`], of the tick after a call was sent) and the answer
    /// is `true`: the caller restarts. Otherwise `false`.
    pub fn tick(&mut self, crash: Option<Crash>) -> bool {
        self.serve();
        self.rig.pool().held = self.fake.list().ok();
        self.rig.pool().unresolved = self.fake.unresolved();
        self.rig.input.hosts = self.hosts();
        self.rig.input.waiting = (0..self.waiting)
            .map(|n| request(&format!("r{n}")))
            .collect();
        self.rig.input.registry_complete = self.grace == 0;
        self.grace = self.grace.saturating_sub(1);
        if crash == Some(Crash::AfterList) && self.sent > 0 {
            return true;
        }
        let now = self.now();
        let actions = self.rig.tick(now);
        let launches = actions
            .iter()
            .any(|action| matches!(action, Action::Acquire(_)));
        let crashing = crash.filter(|_| launches);
        if crashing == Some(Crash::BeforeSave) {
            return true;
        }
        for action in actions {
            match action {
                Action::Save(records) => self.saved = *records,
                Action::Acquire(plan) => {
                    if crashing == Some(Crash::AfterSave) {
                        return true;
                    }
                    let answer = self.send(&plan);
                    if crashing == Some(Crash::AfterCall) {
                        return true;
                    }
                    self.rig.input.acquired.push(AcquireResult {
                        acquire: plan.acquire.clone(),
                        attempt: plan.attempt,
                        answer,
                    });
                }
                Action::Drain(name) => {
                    self.marks.insert(name, Mark::Draining);
                }
                Action::Retire(name) => {
                    self.marks.insert(name, Mark::Draining);
                }
                Action::Undrain(name) => {
                    self.marks.remove(&name);
                }
                Action::ReleaseHost(name) => {
                    let quiet =
                        [None, Some(Mark::Draining)].contains(&self.marks.get(&name).copied());
                    if quiet {
                        self.marks.insert(name, Mark::Releasing);
                    }
                }
                Action::Release { host, id } => {
                    let answer = self.fake.release(&id);
                    self.released.push(host.clone());
                    self.rig.input.released.push(ReleaseResult { host, answer });
                }
                Action::RemoveHost(name) => {
                    self.marks.remove(&name);
                }
                Action::Mint { .. } | Action::Revoke(_) | Action::ForgetNetwork(_) => {}
            }
        }
        self.clock.advance(TICK);
        false
    }

    /// The process restarts: the scaler is rebuilt from the records on disk, what it held
    /// in memory (the answers not yet read, the registry's marks) is gone, the provider's
    /// client forgets the calls it had no answer for, the registry's view is incomplete for
    /// a few ticks, and the restart takes a tick.
    pub fn restart(&mut self) {
        self.rig.state = State::new(self.saved.clone());
        self.rig.input.acquired.clear();
        self.rig.input.released.clear();
        self.marks.clear();
        self.fake.forget_calls_in_flight();
        self.grace = 3;
        self.clock.advance(TICK);
    }

    /// Runs `ticks` ticks with no crash.
    pub fn run(&mut self, ticks: usize) {
        for _ in 0..ticks {
            self.tick(None);
        }
    }

    /// The scaler's records now.
    pub fn records(&self) -> &[js_scaler::Record] {
        &self.rig.state.records.instances
    }

    /// The phases of the records, in order.
    pub fn phases(&self) -> Vec<Phase> {
        self.records().iter().map(|record| record.phase).collect()
    }

    /// The machines running in the cloud that no live record accounts for, by the provider's
    /// id or by the acquire id. A record that is `Gone` or `Refused` accounts for nothing.
    pub fn unrecorded(&self) -> Vec<HostRef> {
        let live = self
            .records()
            .iter()
            .filter(|record| ![Phase::Gone, Phase::Refused].contains(&record.phase));
        let mut known: BTreeSet<String> = BTreeSet::new();
        for record in live {
            known.extend(record.cloud_id.clone());
            known.extend(record.acquire.as_ref().map(|id| id.as_str().to_owned()));
        }
        let running: BTreeSet<HostRef> = self.fake.running().into_iter().collect();
        let truth = self.fake.truth();
        let held = truth.iter().filter(|held| running.contains(&held.id));
        held.filter(|held| !known.contains(held.id.as_str()) && !known.contains(&held.acquire))
            .map(|held| held.id.clone())
            .collect()
    }

    /// The machines the cloud runs whose records say `Gone`: a machine read as gone while
    /// it exists.
    pub fn gone_but_running(&self) -> Vec<HostRef> {
        let running: BTreeSet<HostRef> = self.fake.running().into_iter().collect();
        let gone = self
            .records()
            .iter()
            .filter(|record| record.phase == Phase::Gone);
        let ids = gone.filter_map(|record| record.cloud_id.clone());
        ids.map(HostRef).filter(|id| running.contains(id)).collect()
    }

    /// The pool's blocked reason at the last tick.
    pub fn blocked(&self) -> Option<js_scaler::Blocked> {
        self.rig.state.blocked.get(POOL).copied()
    }

    /// How many acquire actions the cloud was sent.
    pub fn acquires(&self) -> usize {
        self.fake.count(js_elastic_fake::Call::Acquire)
    }
}
