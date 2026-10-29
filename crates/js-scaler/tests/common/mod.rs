//! Fixtures the test files share. Every test drives the pure `step` with a hand-built
//! input and a time it names; none reads a clock or waits.
#![allow(dead_code)]

use std::time::Duration;

use js_elastic::{
    AcquireId, Acquired, Demand, GpuClass, Held, HostRef, HostStatus, Offer, PriceUnit, SchedulerId,
};
use js_scaler::{
    AcquirePlan, AcquireResult, Action, Health, HostFacts, HostState, Input, LaunchMode, Millis,
    Phase, PoolInput, Record, Settings, State, StoredOffer, Waiting, step,
};
use js_types::{ExecMode, HostId, RequestId, Resources};

/// The time the tests start at: well past one day, so a day window has room behind it.
pub const START: u64 = 1_000_000_000_000;

pub const SECOND: u64 = 1_000;
pub const MINUTE: u64 = 60 * SECOND;
pub const HOUR: u64 = 60 * MINUTE;
pub const DAY: u64 = 24 * HOUR;

/// The instant `millis` after the start.
pub fn at(millis: u64) -> Millis {
    Millis::new(START + millis)
}

pub fn minutes(count: u64) -> Duration {
    Duration::from_secs(60 * count)
}

/// An offer of a linux x86_64 machine with one a10 gpu, at `price` cents per hour.
pub fn offer(id: &str, price: u64) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: Some("z1".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 64 << 30,
        disk_bytes: 512 << 30,
        gpus: 1,
        gpu_class: Some(GpuClass::parse("nvidia-a10").unwrap()),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
        price_cents_per_hour: Some(price),
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

/// What a job needs: a linux x86_64 machine with the a10 and a little of the rest.
pub fn demand() -> Demand {
    Demand {
        resources: Resources::new(4000, 16 << 30, 1, Some("nvidia-a10".to_owned())).unwrap(),
        disk_bytes: Some(100 << 30),
        os: Some("linux".to_owned()),
        arch: Some("x86_64".to_owned()),
        exec: Some(ExecMode::Container),
    }
}

/// A request that needs `demand()`.
pub fn request(id: &str) -> Waiting {
    Waiting {
        id: RequestId::new(id).unwrap(),
        demand: demand(),
        reserved: None,
    }
}

/// A request that holds a reservation on `host`.
pub fn reserved(id: &str, host: &str) -> Waiting {
    Waiting {
        reserved: Some(HostId::new(host).unwrap()),
        ..request(id)
    }
}

/// Live settings with room: four machines, 1000 cents an hour, 100000 a day.
pub fn settings(pool: &str) -> Settings {
    Settings {
        launch: LaunchMode::Live,
        ..Settings::new(pool, 4, 1000, 100_000)
    }
}

/// A pool whose checks pass, whose list is empty and whose catalog is one a10 offer at 60
/// cents an hour (a cent a minute).
pub fn pool(settings: Settings) -> PoolInput {
    PoolInput {
        settings,
        health: Health::Available,
        offers: vec![offer("a10", 60)],
        held: Some(Vec::new()),
        unresolved: Vec::new(),
    }
}

/// A host of the shape of `offer("a10", _)`: live, unmarked, doing nothing.
pub fn host(name: &str, pool: &str) -> HostFacts {
    HostFacts {
        id: HostId::new(name).unwrap(),
        pool: pool.to_owned(),
        state: HostState::Live,
        shape: Offer {
            price_cents_per_hour: None,
            ..offer("a10", 0)
        },
        mark: None,
        busy: false,
        lease_given_up: false,
        heard_since_hello: true,
    }
}

/// An input with one pool and nothing waiting.
pub fn input(pool: PoolInput) -> Input {
    Input {
        scheduler: SchedulerId::new("sched").unwrap(),
        pools: vec![pool],
        waiting: Vec::new(),
        hosts: Vec::new(),
        registry_complete: true,
        acquired: Vec::new(),
        released: Vec::new(),
        not_found: Vec::new(),
        entropy: Vec::new(),
    }
}

/// The random value number `n`: 16 bytes, so an acquire id made from it is the pool, a dash
/// and `draw_text(n)`.
pub fn draw(n: u64) -> [u8; 16] {
    u128::from(n).to_be_bytes()
}

/// The 32 hex digits of `draw(n)`.
pub fn draw_text(n: u64) -> String {
    format!("{n:032x}")
}

/// A scaler and the input it is fed. `tick` runs a step and clears the answers and the
/// random values, which the caller hands over once. A step with no values set is given
/// sixteen new ones, numbered from 1000 up, that no earlier step had.
pub struct Rig {
    pub state: State,
    pub input: Input,
    pub drawn: u64,
}

impl Rig {
    pub fn new(settings: Settings) -> Self {
        Self {
            state: State::default(),
            input: input(pool(settings)),
            drawn: 1000,
        }
    }

    pub fn tick(&mut self, now: Millis) -> Vec<Action> {
        if self.input.entropy.is_empty() {
            self.input.entropy = (self.drawn..self.drawn + 16).map(draw).collect();
            self.drawn += 16;
        }
        let state = std::mem::take(&mut self.state);
        let (state, actions) = step(state, &self.input, now);
        self.state = state;
        self.input.acquired.clear();
        self.input.released.clear();
        self.input.not_found.clear();
        self.input.entropy.clear();
        actions
    }

    pub fn pool(&mut self) -> &mut PoolInput {
        &mut self.input.pools[0]
    }

    pub fn record(&self, index: usize) -> &Record {
        &self.state.records.instances[index]
    }

    pub fn phase(&self, index: usize) -> Phase {
        self.record(index).phase
    }
}

/// The name of each action's kind, in order.
pub fn kinds(actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .map(|action| {
            format!("{action:?}")
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// How many actions are of `kind`.
pub fn count(actions: &[Action], kind: &str) -> usize {
    kinds(actions).iter().filter(|each| *each == kind).count()
}

/// The acquires among `actions`.
pub fn acquires(actions: &[Action]) -> Vec<AcquirePlan> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Acquire(plan) => Some((**plan).clone()),
            _ => None,
        })
        .collect()
}

/// The machine `plan` asked for, as the provider lists it with `status`.
pub fn listed(plan: &AcquirePlan, cloud: &str, status: HostStatus) -> Held {
    Held {
        id: HostRef(cloud.to_owned()),
        acquire: plan.acquire.as_str().to_owned(),
        name: Some(plan.name.clone()),
        offer: plan.offer.id.clone(),
        zone: plan.offer.zone.clone(),
        status,
    }
}

/// The provider's answer to `plan`: it launched the machine `cloud`.
pub fn accepted(plan: &AcquirePlan, cloud: &str) -> AcquireResult {
    AcquireResult {
        acquire: plan.acquire.clone(),
        attempt: plan.attempt,
        answer: Acquired::Accepted {
            held: listed(plan, cloud, HostStatus::Booting),
            extras: Vec::new(),
        },
    }
}

/// An answer to `plan` that is not an acceptance.
pub fn answered(plan: &AcquirePlan, answer: Acquired) -> AcquireResult {
    AcquireResult {
        acquire: plan.acquire.clone(),
        attempt: plan.attempt,
        answer,
    }
}

/// An acquire id that no record has.
pub fn stranger() -> AcquireId {
    AcquireId::new("nobody-launched-this").unwrap()
}

/// A rig whose pool has launched one machine for `r1` and been told it is `cloud`, with
/// the machine listed and the request gone: the machine is Booting. Returns the rig, the
/// plan and the time of the last tick.
pub fn booting(settings: Settings, cloud: &str) -> (Rig, AcquirePlan, u64) {
    let mut rig = Rig::new(settings);
    rig.input.waiting = vec![request("r1")];
    let actions = rig.tick(at(0));
    let plan = acquires(&actions).remove(0);
    rig.input.acquired = vec![accepted(&plan, cloud)];
    rig.pool().held = Some(vec![listed(&plan, cloud, HostStatus::Booting)]);
    rig.input.waiting = Vec::new();
    rig.tick(at(SECOND));
    (rig, plan, SECOND)
}

/// A rig as `booting` leaves it, then the agent's hello at `later` milliseconds: the
/// machine is Registered and quiet, first seen quiet at `later`.
pub fn registered(settings: Settings, cloud: &str, later: u64) -> (Rig, AcquirePlan) {
    let (mut rig, plan, _) = booting(settings, cloud);
    let pool_name = rig.input.pools[0].settings.pool.clone();
    rig.input.hosts = vec![host(&plan.name, &pool_name)];
    rig.tick(at(later));
    (rig, plan)
}

/// A record of a machine of the pool "gpu" in `phase`, launched `launched` milliseconds
/// after the start at `price` cents an hour. Its name and acquire id are made from the
/// two, so two records differ.
pub fn record(phase: Phase, price: u64, launched: u64) -> Record {
    Record {
        pool: "gpu".to_owned(),
        acquire: Some(AcquireId::new(&format!("made-{launched}-{price}")).unwrap()),
        name: format!("js-made-{launched}-{price}"),
        offer: StoredOffer::from_offer(&offer("a10", price)),
        price_cents_per_hour: price,
        phase,
        since: at(launched),
        launched_at: at(launched),
        token_expires: None,
        cloud_id: Some(format!("i-{launched}-{price}")),
        retry_at: None,
        attempt: 0,
        gone_at: None,
        registered: false,
        condemned: false,
        clean_lists: 0,
        last_clean: None,
        seen_live: false,
    }
}
