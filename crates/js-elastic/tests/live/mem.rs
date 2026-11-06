//! The in-memory provider and cloud the live driver is run against, and the helpers that
//! run it and read what it printed.

use std::collections::{BTreeMap, BTreeSet};
use std::future::ready;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_elastic::conformance::live::{LiveArgs, LiveCloud, run};
use js_elastic::{
    AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture, Claim, Held,
    Hello, HostRef, HostStatus, Observed, Offer, PriceUnit, ProviderCaps, ProviderError, Purchase,
    Reason, Refusal, RefusalCode, Released, SchedulerId,
};
use js_types::{ExecMode, ProviderId};

/// What the stand-in's release does.
#[derive(Clone, Copy)]
pub enum Release {
    /// The machine is terminating and stays listed so.
    Terminating,
    /// The machine is terminated and stays listed so.
    Terminated,
    /// The machine disappears.
    Gone,
    /// The answer says terminating and nothing changes: the machine keeps running.
    Ignored,
    /// The answer is a refusal and nothing changes.
    Refused,
}

/// What the stand-in's list says of a machine's name.
#[derive(Clone, Copy)]
pub enum Named {
    Real,
    Missing,
    Wrong,
}

/// The payload a panic carries.
#[derive(Clone, Copy)]
pub enum Boom {
    Formatted,
    Literal,
    Neither,
}

/// A machine of the stand-in's cloud.
#[derive(Clone)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub acquire: String,
    pub offer: String,
    pub zone: Option<String>,
    pub status: HostStatus,
}

/// The pool, labels and name of one acquire the stand-in received.
pub type Seen = (String, Vec<(String, String)>, String);

/// The stand-in's cloud, shared by every provider the cloud builds, as a real cloud is.
pub struct State {
    pub machines: Vec<Machine>,
    pub catalog: Result<Vec<Offer>, String>,
    pub arches: BTreeSet<String>,
    pub release: Release,
    pub launch_every_time: bool,
    pub named: Named,
    pub omit_from_list: bool,
    pub observe_absent: bool,
    pub caps_panic: Option<Boom>,
    pub requests: Vec<Seen>,
}

pub fn offer(
    id: &str,
    zone: Option<&str>,
    arch: &str,
    price: Option<u64>,
    available: bool,
) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: zone.map(str::to_owned),
        cpu_millis: 2000,
        memory_bytes: 1 << 33,
        disk_bytes: 1 << 35,
        accelerator: None,
        os: "linux".to_owned(),
        arch: arch.to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: price,
        price_unit: PriceUnit::UsCents,
        available,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// The catalog most tests use: one offer that is chosen, with a zone and a purchase.
pub fn one_offer() -> Vec<Offer> {
    let mut chosen = offer("cheap", Some("us-east-1b"), "x86_64", Some(10), true);
    chosen.purchase = Some(Purchase::OnDemand);
    vec![chosen]
}

pub fn state(catalog: Vec<Offer>) -> Arc<Mutex<State>> {
    Arc::new(Mutex::new(State {
        machines: Vec::new(),
        catalog: Ok(catalog),
        arches: ["x86_64".to_owned(), "arm64".to_owned()].into(),
        release: Release::Terminating,
        launch_every_time: false,
        named: Named::Real,
        omit_from_list: false,
        observe_absent: false,
        caps_panic: None,
        requests: Vec::new(),
    }))
}

/// A provider over the shared state, with room for `room` machines, whose list calls in
/// `fail_calls` (counted from 0 for this provider) fail and in `panic_calls` panic.
pub struct Mem {
    pub state: Arc<Mutex<State>>,
    pub room: usize,
    pub fail_calls: BTreeSet<usize>,
    pub panic_calls: BTreeSet<usize>,
    pub list_calls: AtomicUsize,
}

impl Mem {
    fn held(&self, machine: &Machine) -> Held {
        Held {
            id: HostRef(machine.id.clone()),
            acquire: machine.acquire.clone(),
            name: Some(machine.name.clone()),
            offer: machine.offer.clone(),
            zone: machine.zone.clone(),
            status: machine.status.clone(),
        }
    }
}

impl CapacityProvider for Mem {
    fn id(&self) -> ProviderId {
        ProviderId::known("mem")
    }

    fn can_launch(&self) -> bool {
        true
    }

    fn capabilities(&self) -> ProviderCaps {
        let (boom, arches, held) = {
            let state = self.state.lock().unwrap();
            (state.caps_panic, state.arches.clone(), state.machines.len())
        };
        match boom {
            Some(Boom::Formatted) => panic!("capabilities broke: {held}"),
            Some(Boom::Literal) => panic!("capabilities broke"),
            Some(Boom::Neither) => std::panic::panic_any(7_u8),
            None => {}
        }
        ProviderCaps {
            os: ["linux".to_owned()].into(),
            arch: arches,
            ..ProviderCaps::default()
        }
    }

    fn catalog(&self) -> CatalogFuture<'_> {
        let catalog = self.state.lock().unwrap().catalog.clone();
        Box::pin(ready(catalog.map_err(ProviderError::new)))
    }

    fn capacity(&self) -> Capacity {
        Capacity {
            supplied: 0,
            can_add: None,
        }
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        0
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        let mut state = self.state.lock().unwrap();
        state.requests.push((
            request.pool.clone(),
            request.labels.clone(),
            request.name.to_string(),
        ));
        let known = state
            .machines
            .iter()
            .find(|machine| machine.acquire == request.acquire.as_str())
            .cloned();
        if let (Some(machine), false) = (known, state.launch_every_time) {
            return Acquired::Accepted {
                held: self.held(&machine),
                extras: Vec::new(),
            };
        }
        // A list that hides machines also hides them from the provider's own bound.
        if !state.omit_from_list && state.machines.len() >= self.room {
            return Acquired::Refused(Refusal {
                code: RefusalCode::Capacity,
                reason: Reason::AtMaxMachines {
                    held: state.machines.len(),
                    max_machines: self.room,
                },
            });
        }
        let machine = Machine {
            id: format!("i-{:017x}", state.machines.len() + 1),
            name: request.name.to_string(),
            acquire: request.acquire.to_string(),
            offer: request.offer.id.clone(),
            zone: request.offer.zone.clone(),
            status: HostStatus::Booting,
        };
        let held = self.held(&machine);
        state.machines.push(machine);
        Acquired::Accepted {
            held,
            extras: Vec::new(),
        }
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        let call = self.list_calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_calls.contains(&call),
            "the list {call} panicked"
        );
        if self.fail_calls.contains(&call) {
            return Err(ProviderError::new("the list was refused"));
        }
        let state = self.state.lock().unwrap();
        if state.omit_from_list {
            return Ok(Vec::new());
        }
        Ok(state
            .machines
            .iter()
            .map(|machine| {
                let mut held = self.held(machine);
                held.name = match state.named {
                    Named::Real => held.name,
                    Named::Missing => None,
                    Named::Wrong => Some("other".to_owned()),
                };
                held
            })
            .collect())
    }

    fn release(&self, id: &HostRef) -> Released {
        let mut state = self.state.lock().unwrap();
        let release = state.release;
        match release {
            Release::Gone => {
                state.machines.retain(|machine| machine.id != id.as_str());
                Released::Gone
            }
            Release::Refused => Released::Refused(Claim::Foreign),
            Release::Ignored => Released::Terminating(HostStatus::Terminating),
            Release::Terminating | Release::Terminated => {
                let status = if matches!(release, Release::Terminated) {
                    HostStatus::Terminated
                } else {
                    HostStatus::Terminating
                };
                for machine in &mut state.machines {
                    if machine.id == id.as_str() {
                        machine.status = status.clone();
                    }
                }
                Released::Terminating(status)
            }
        }
    }

    fn observe(&self, id: &HostRef) -> Observed {
        let state = self.state.lock().unwrap();
        let found = state.machines.iter().find(|m| m.id == id.as_str());
        match found {
            Some(machine) if !state.observe_absent => Observed::Present(machine.status.clone()),
            _ => Observed::Absent,
        }
    }

    fn verify_instance(&self, hello: &Hello) -> Option<HostRef> {
        let state = self.state.lock().unwrap();
        let found = state.machines.iter().find(|m| m.name == hello.name);
        found.map(|machine| HostRef(machine.id.clone()))
    }
}

/// A cloud that builds `Mem` providers over one state and notes the room each was asked
/// for, in order.
pub struct MemCloud {
    pub state: Arc<Mutex<State>>,
    pub rooms: Mutex<Vec<usize>>,
    pub fail_builds: BTreeSet<usize>,
    pub fail_lists: BTreeMap<usize, BTreeSet<usize>>,
    pub panic_lists: BTreeMap<usize, BTreeSet<usize>>,
    pub unlaunchable: Option<&'static str>,
}

impl MemCloud {
    pub fn new(state: &Arc<Mutex<State>>) -> Self {
        Self {
            state: Arc::clone(state),
            rooms: Mutex::new(Vec::new()),
            fail_builds: BTreeSet::new(),
            fail_lists: BTreeMap::new(),
            panic_lists: BTreeMap::new(),
            unlaunchable: None,
        }
    }

    pub fn rooms(&self) -> Vec<usize> {
        self.rooms.lock().unwrap().clone()
    }

    fn build(&self, room: NonZeroUsize) -> Result<Mem, String> {
        let mut rooms = self.rooms.lock().unwrap();
        let index = rooms.len();
        rooms.push(room.get());
        if self.fail_builds.contains(&index) {
            return Err(format!("build {index} was refused"));
        }
        Ok(Mem {
            state: Arc::clone(&self.state),
            room: room.get(),
            fail_calls: self.fail_lists.get(&index).cloned().unwrap_or_default(),
            panic_calls: self.panic_lists.get(&index).cloned().unwrap_or_default(),
            list_calls: AtomicUsize::new(0),
        })
    }
}

impl LiveCloud for MemCloud {
    fn provider(
        &self,
        _scheduler: &SchedulerId,
        machines: NonZeroUsize,
    ) -> Result<Box<dyn CapacityProvider>, String> {
        self.build(machines)
            .map(|provider| Box::new(provider) as Box<dyn CapacityProvider>)
    }

    fn launchable(&self, offer: &Offer) -> bool {
        self.unlaunchable != Some(offer.arch.as_str())
    }
}

/// A cloud that keeps the default `launchable`: every offer.
pub struct Plain<'a>(pub &'a MemCloud);

impl LiveCloud for Plain<'_> {
    fn provider(
        &self,
        scheduler: &SchedulerId,
        machines: NonZeroUsize,
    ) -> Result<Box<dyn CapacityProvider>, String> {
        self.0.provider(scheduler, machines)
    }
}

pub fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// The arguments of a dry run in `us-east-1` for the shape `cheap`, with `more` after them;
/// a `--shape` in `more` replaces the default one.
pub fn args_of(more: &[&str]) -> LiveArgs {
    let mut all = vec!["--run-id", "run-1", "--region", "us-east-1"];
    if !more.contains(&"--shape") {
        all.extend_from_slice(&["--shape", "cheap"]);
    }
    all.extend_from_slice(more);
    LiveArgs::parse(&strings(&all), &["key-file", "image"]).unwrap()
}

/// Runs `args` over `cloud`, returning the exit status and the lines printed.
pub fn go(args: &LiveArgs, cloud: &dyn LiveCloud) -> (i32, Vec<String>) {
    let mut out = Vec::new();
    let status = run(args, cloud, &mut out);
    let text = String::from_utf8(out).unwrap();
    (status, text.lines().map(str::to_owned).collect())
}

/// The plan hash a dry run printed.
pub fn printed_hash(lines: &[String]) -> String {
    let line = lines.iter().find(|l| l.starts_with("plan hash: ")).unwrap();
    line.strip_prefix("plan hash: ").unwrap().to_owned()
}

pub fn spend_args(hash: &str) -> LiveArgs {
    args_of(&["--spend", "--plan-hash", hash])
}

/// The plan hash a dry run over `state` prints, from a cloud of its own so that the cloud
/// a spend run uses counts only its own builds.
pub fn hash_over(state: &Arc<Mutex<State>>) -> String {
    printed_hash(&go(&args_of(&[]), &MemCloud::new(state)).1)
}

/// A spend run over `cloud` with the hash a dry run over `state` printed.
pub fn spend(state: &Arc<Mutex<State>>, cloud: &dyn LiveCloud) -> (i32, Vec<String>) {
    go(&spend_args(&hash_over(state)), cloud)
}

pub fn at(lines: &[String], line: &str) -> usize {
    lines
        .iter()
        .position(|l| l == line)
        .unwrap_or_else(|| panic!("no line {line:?} in {lines:#?}"))
}

pub fn statuses(state: &Arc<Mutex<State>>) -> Vec<HostStatus> {
    let state = state.lock().unwrap();
    state.machines.iter().map(|m| m.status.clone()).collect()
}

pub fn seed_machine(state: &Arc<Mutex<State>>, id: &str) {
    state.lock().unwrap().machines.push(Machine {
        id: id.to_owned(),
        name: "js-old".to_owned(),
        acquire: "old".to_owned(),
        offer: "cheap".to_owned(),
        zone: None,
        status: HostStatus::Booting,
    });
}
