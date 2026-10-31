//! A fake for everything the scaler's shell reads and does: it logs each call that changes
//! something, keeps a provider that holds machines, and keeps a registry whose marks move
//! the way the real one's do. A test changes what the fake answers and reads its log.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::Phase as WirePhase;
use js_elastic::AcquireId;
use js_elastic::{
    Acquired, GpuClass, Held, HostRef, HostStatus, Offer, PriceUnit, ProviderError, Released,
    SchedulerId, Secret,
};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::{Call, Effects, Scaler, SharedClock};
use js_registry::{
    Busy, HostMark, HostState, HostView, SequentialSessions, SessionSource, Waiting,
};
use js_scaler::{
    AcquirePlan, Health, LaunchMode, Millis, Phase, Record, Records, Settings, StoredOffer,
};
use js_types::{ExecMode, HostId, MemoryHeadroom, RequestId, Requirements, Resources, Shape};

/// The wall time every test clock starts at.
pub const START: u64 = 1_000_000_000_000;

/// What the fake holds and answers.
pub struct World {
    /// Every call that changes something, as text, in order.
    pub log: Vec<String>,
    pub waiting: Vec<Waiting>,
    pub hosts: Vec<HostView>,
    pub health: Health,
    pub offers: Vec<Offer>,
    /// The acquire ids the provider holds an unresolved call for.
    pub unresolved: Vec<AcquireId>,
    /// The provider ids a read by id finds gone.
    pub gone: Vec<String>,
    /// The provider ids a read by id was made for, in order.
    pub gone_asked: Vec<String>,
    pub list_fails: bool,
    pub machines: Vec<Held>,
    pub saved: Option<Records>,
    /// The hosts last written as the ones the registry would not remove.
    pub saved_unremoved: Option<BTreeSet<String>>,
    /// Every set of unremoved hosts a write was asked for, in order, whether it succeeded or not.
    pub unremoved_writes: Vec<BTreeSet<String>>,
    /// Whether a write of the unremoved hosts succeeds.
    pub save_unremoved_ok: bool,
    pub save_ok: bool,
    pub mint_ok: bool,
    pub revoke_ok: bool,
    pub remove_ok: bool,
    /// The registry does not mark a host draining or releasing, whatever it is asked.
    pub marks_refused: bool,
    /// The registry's hosts as they are after the next save: a host that says hello and is
    /// booked between the shell's read of the registry and its calls.
    pub after_save: Option<Vec<HostView>>,
    /// When set, every release is answered with this and the machine is left as it is.
    pub release_answer: Option<Released>,
    /// When set, the provider's `list` says it has been called and then waits to be let go.
    pub gate: Option<Arc<Gate>>,
    /// The lifetime each acquire carried, in the order the acquires were made.
    pub lifetimes: Vec<Option<Duration>>,
}

/// A provider call held at a gate: the call tells `entered` it is there and waits (for at
/// most 30 seconds, so a test that fails does not hang) until `release` is sent a value.
pub struct Gate {
    pub entered: Mutex<Sender<()>>,
    pub release: Mutex<Receiver<()>>,
}

impl Gate {
    fn pass(&self) {
        let entered = self.entered.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = entered.send(());
        let release = self.release.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = release.recv_timeout(Duration::from_secs(30));
    }
}

/// The fake's effects, over a world the test also holds.
pub struct Handle(pub Arc<Mutex<World>>);

impl World {
    fn host_of(&self, id: &HostRef) -> Option<&HostView> {
        let machine = self.machines.iter().find(|held| held.id == *id)?;
        let name = machine.name.as_deref()?;
        self.hosts.iter().find(|view| view.id.as_str() == name)
    }

    /// A retire marks a host draining whether or not it holds a lease, unless it is releasing.
    fn retire(&mut self, host: &str) {
        for view in self.hosts.iter_mut().filter(|v| v.id.as_str() == host) {
            if view.mark != Some(HostMark::Releasing) {
                view.mark = Some(HostMark::Draining);
            }
        }
    }

    fn mark(&mut self, host: &str, from: &[Option<HostMark>], to: Option<HostMark>) {
        for view in self.hosts.iter_mut().filter(|v| v.id.as_str() == host) {
            let quiet = view.lease.is_none();
            if quiet && from.contains(&view.mark) {
                view.mark = to;
            }
        }
    }
}

impl Effects for Handle {
    fn registry(&self) -> (Vec<HostView>, Vec<Waiting>) {
        let world = self.world();
        (world.hosts.clone(), world.waiting.clone())
    }

    fn leased(&self, host: &str) -> bool {
        let world = self.world();
        let on = world.hosts.iter().find(|view| view.id.as_str() == host);
        on.is_some_and(|view| view.lease.is_some())
    }

    fn remove(&self, host: &str) -> Result<(), Busy> {
        let mut world = self.world();
        world.log.push(format!("remove:{host}"));
        match world.remove_ok {
            true => {
                world.hosts.retain(|view| view.id.as_str() != host);
                Ok(())
            }
            false => Err(Busy::Lease),
        }
    }

    fn checks(&self) -> (Health, Vec<Offer>) {
        let world = self.world();
        (world.health, world.offers.clone())
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        self.world().unresolved.clone()
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        let mut world = self.world();
        world.gone_asked.push(id.as_str().to_owned());
        world.gone.iter().any(|gone| gone == id.as_str())
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        let gate = self.world().gate.clone();
        if let Some(gate) = gate {
            gate.pass();
        }
        let world = self.world();
        match world.list_fails {
            true => Err(ProviderError::new("the list failed")),
            false => Ok(world.machines.clone()),
        }
    }

    fn save(&self, records: &Records) -> Result<(), String> {
        let mut world = self.world();
        world.log.push("save".to_owned());
        match world.save_ok {
            true => {
                world.saved = Some(records.clone());
                if let Some(hosts) = world.after_save.take() {
                    world.hosts = hosts;
                }
                Ok(())
            }
            false => Err("the disk is full".to_owned()),
        }
    }

    fn save_unremoved(&self, hosts: &BTreeSet<String>) -> Result<(), String> {
        let mut world = self.world();
        world.log.push("save-unremoved".to_owned());
        world.unremoved_writes.push(hosts.clone());
        match world.save_unremoved_ok {
            true => {
                world.saved_unremoved = Some(hosts.clone());
                Ok(())
            }
            false => Err("the disk is full".to_owned()),
        }
    }

    fn mint(&self, host: &str, _valid_for: Duration) -> Option<Secret> {
        let mut world = self.world();
        world.log.push(format!("mint:{host}"));
        world
            .mint_ok
            .then(|| Secret::new(format!("token-of-{host}")))
    }

    fn acquire(&self, plan: &AcquirePlan, token: Secret) -> Acquired {
        let mut world = self.world();
        let entry = format!("acquire:{}:{}", plan.name, token.expose());
        world.log.push(entry);
        world.lifetimes.push(plan.lifetime);
        let held = Held {
            id: HostRef(format!("i-{}", world.machines.len() + 1)),
            acquire: plan.acquire.as_str().to_owned(),
            name: Some(plan.name.clone()),
            offer: plan.offer.id.clone(),
            zone: plan.offer.zone.clone(),
            status: HostStatus::Booting,
        };
        world.machines.push(held.clone());
        Acquired::Accepted {
            held,
            extras: Vec::new(),
        }
    }

    fn release(&self, id: &HostRef) -> Released {
        let mut world = self.world();
        let name = world
            .machines
            .iter()
            .find(|held| held.id == *id)
            .and_then(|held| held.name.clone());
        world
            .log
            .push(format!("release:{}", name.unwrap_or_default()));
        // The registry must already have marked the host, and it must be quiet.
        let unsafe_release = world
            .host_of(id)
            .is_some_and(|view| view.mark != Some(HostMark::Releasing) || view.lease.is_some());
        if unsafe_release {
            world
                .log
                .push("UNSAFE: released a host that could take work".to_owned());
        }
        let at = world.machines.iter().position(|held| held.id == *id);
        if let Some(answer) = world.release_answer.clone() {
            return answer;
        }
        match at {
            None => Released::Gone,
            Some(at) if world.machines[at].status == HostStatus::Terminating => {
                world.machines.remove(at);
                Released::Gone
            }
            Some(at) => {
                world.machines[at].status = HostStatus::Terminating;
                Released::Terminating(HostStatus::Terminating)
            }
        }
    }

    fn revoke(&self, host: &str) -> bool {
        let mut world = self.world();
        world.log.push(format!("revoke:{host}"));
        world.revoke_ok
    }

    fn forget(&self, host: &str) {
        self.world().log.push(format!("forget:{host}"));
    }

    fn call(&self, call: Call, host: &str) {
        let mut world = self.world();
        world.log.push(format!("{call:?}:{host}"));
        let refused = world.marks_refused && matches!(call, Call::Drain | Call::ReleaseHost);
        match call {
            _ if refused => {}
            Call::Drain => world.mark(host, &[None], Some(HostMark::Draining)),
            Call::Retire => world.retire(host),
            Call::Undrain => world.mark(host, &[Some(HostMark::Draining)], None),
            Call::ReleaseHost => {
                world.mark(host, &[Some(HostMark::Draining)], Some(HostMark::Releasing));
            }
            Call::MarkEnding => {
                let ending = Some(HostMark::Releasing);
                world.mark(host, &[None, Some(HostMark::Draining)], ending);
            }
        }
    }
}

impl Handle {
    fn world(&self) -> MutexGuard<'_, World> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A scaler over a fake world, on a clock that moves when the test says.
pub struct Rig {
    pub world: Arc<Mutex<World>>,
    pub clock: ManualClock,
    pub scaler: Scaler,
}

impl Rig {
    /// The world, locked.
    pub fn world(&self) -> MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The log so far.
    pub fn log(&self) -> Vec<String> {
        self.world().log.clone()
    }

    /// Moves the clock on by `by`.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }
}

/// Whether every entry of `log` is a `save`, none else: the tick wrote the records and did
/// nothing to a machine or a host, as a tick does while a silent or missing time is pending
/// and a while has passed. Unlike an empty log it fails on any drain, release or revoke.
pub fn only_saves(log: &[String]) -> bool {
    log.iter().all(|entry| entry == "save")
}

/// Live settings with room: four machines, 1000 cents an hour, 100000 a day.
pub fn live() -> Settings {
    Settings {
        launch: LaunchMode::Live,
        ..Settings::new("gpu", 4, 1000, 100_000)
    }
}

/// A scaler for `settings` that starts from `records`, over a world with one a10 offer
/// at 60 cents an hour and no machines, hosts or requests.
pub fn rig(records: Records, settings: Settings) -> Rig {
    rig_with(records, settings, Box::new(SequentialSessions::new()))
}

/// A world with one a10 offer at 60 cents an hour and no machines, hosts or requests.
pub fn new_world() -> Arc<Mutex<World>> {
    trace_everything();
    Arc::new(Mutex::new(World {
        log: Vec::new(),
        waiting: Vec::new(),
        hosts: Vec::new(),
        health: Health::Available,
        offers: vec![offer()],
        unresolved: Vec::new(),
        gone: Vec::new(),
        gone_asked: Vec::new(),
        list_fails: false,
        machines: Vec::new(),
        saved: None,
        saved_unremoved: None,
        unremoved_writes: Vec::new(),
        save_unremoved_ok: true,
        save_ok: true,
        mint_ok: true,
        revoke_ok: true,
        remove_ok: true,
        marks_refused: false,
        after_save: None,
        release_answer: None,
        gate: None,
        lifetimes: Vec::new(),
    }))
}

/// [`rig`] with `entropy` for the random part of the acquire ids.
pub fn rig_with(records: Records, settings: Settings, entropy: Box<dyn SessionSource>) -> Rig {
    trace_everything();
    let world = new_world();
    let clock = ManualClock::new(Timestamp::from_millis(START));
    let scaler = Scaler::new(
        Box::new(Handle(world.clone())),
        SharedClock::new(clock.clone()),
        entropy,
        (SchedulerId::new("sched").unwrap(), ExecMode::Container),
        settings,
        records,
    );
    Rig {
        world,
        clock,
        scaler,
    }
}

/// Everything logged, as text.
static LOG: Mutex<String> = Mutex::new(String::new());

/// The writer of the subscriber: into [`LOG`].
struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LOG.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_str(&String::from_utf8_lossy(bytes));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Everything logged so far by every test in this binary. A test looks for text only it
/// can have caused (a host name of its own).
pub fn logged() -> String {
    LOG.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Installs a subscriber that takes every event into [`LOG`]: a disabled event does not
/// evaluate its fields, and the fields are code that must run once under test.
fn trace_everything() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(|| Capture)
            .try_init();
    });
}

/// An offer of a linux x86_64 machine with one a10 gpu, at 60 cents an hour.
pub fn offer() -> Offer {
    Offer {
        id: "a10".to_owned(),
        zone: Some("z1".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 64 << 30,
        disk_bytes: 512 << 30,
        gpus: 1,
        gpu_class: Some(GpuClass::parse("nvidia-a10").unwrap()),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
        price_cents_per_hour: Some(60),
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

/// A whole-node request for one a10.
pub fn request(id: &str) -> Waiting {
    Waiting {
        id: RequestId::new(id).unwrap(),
        demand: Requirements {
            resources: Resources::new(1000, 1 << 30, 1, Some("nvidia-a10".to_owned())).unwrap(),
            disk_bytes: None,
            os: None,
            arch: None,
            exec: None,
        },
        since: Timestamp::from_millis(START),
        reserved: None,
    }
}

/// A registered host: live, unmarked, holding nothing. The pool books its floor for it, and
/// it announced the size of [`offer`].
pub fn view(name: &str) -> HostView {
    let resources = Resources::new(1000, 1 << 30, 1, Some("nvidia-a10".to_owned())).unwrap();
    let announced = Resources::new(8000, 64 << 30, 1, Some("nvidia-a10".to_owned())).unwrap();
    HostView {
        id: HostId::new(name).unwrap(),
        state: HostState::Live,
        phase: WirePhase::Serving,
        lease: None,
        shape: Shape::new(resources, BTreeMap::new()),
        memory: MemoryHeadroom::full(1 << 30),
        claim: Some(Shape::new(announced, BTreeMap::new())),
        labels: BTreeMap::new(),
        agent_version: None,
        epoch: 1,
        heard_since_hello: true,
        releasing: false,
        taint: None,
        mark: None,
        reserved_for: None,
        drain_deadline: None,
        cordon_until: None,
        load: None,
    }
}

/// A record of a machine of the pool `gpu` in `phase`, launched at the start of the test
/// clock, named `name`, with the provider's id `i-9`.
pub fn record(name: &str, phase: Phase) -> Record {
    Record {
        pool: "gpu".to_owned(),
        acquire: Some(AcquireId::new(&format!("gpu-{name}")).unwrap()),
        name: name.to_owned(),
        offer: StoredOffer::from_offer(&offer()),
        price_cents_per_hour: 60,
        phase,
        since: Millis::new(START),
        launched_at: Millis::new(START),
        token_expires: None,
        cloud_id: Some("i-9".to_owned()),
        retry_at: None,
        attempt: 0,
        gone_at: None,
        registered: false,
        condemned: false,
        clean_lists: 0,
        last_clean: None,
        // A machine the step has seen live already: a step that sees it live again has
        // nothing new to write down.
        seen_live: true,
    }
}

pub fn records(each: Vec<Record>) -> Records {
    Records {
        instances: each,
        cooldowns: Default::default(),
        ..Default::default()
    }
}
