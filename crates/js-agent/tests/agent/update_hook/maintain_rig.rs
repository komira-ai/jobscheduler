//! What the tests of `drain` and `resume` share: a directory tree with the fakes' world, the
//! params of a maintenance run, and a [`Probe`] that is every port at once over the world
//! (the lease server, the hold file of the world's state root, the unit manager, the host
//! facts, the clock and the kill points), keeps the log of the calls, fails a call a test names
//! and acts at a pause.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use js_agent::lifecycle::StateRoot;
use js_agent::lifecycle::hold::{HOLD_FILE, HOLD_RESUME_FILE, HOLD_TEXT};
use js_agent::update_hook::step::{self, MaintainEnv};
use js_agent::update_hook::{
    Class, Clock, Cordoned, Done, DrainParams, Facts, Gate, HoldState, Holds, HostReading, Lease,
    LeaseError, NoGate, Outcome, PortError, Reason, Report, Uncordoned, UnitState, Units,
};
use js_testkit::dir::TestDir;

use super::fakes::{START_MS, World};

/// The agent's unit.
pub const UNIT: &str = "js-agent-crun.service";
/// The host the server knows.
pub const HOST: &str = "burst-1";
/// The cordon's lease of the params, in seconds.
pub const LEASE_SECS: u64 = 300;
/// What a deferred `drain` asks the updater to wait, in seconds.
pub const RETRY_SECS: u64 = 600;

/// The tree of one test.
pub struct Rig {
    pub tmp: TestDir,
    pub world: World,
    pub params: DrainParams,
}

impl Rig {
    /// An idle host the server knows, with its unit running.
    pub fn new(name: &str) -> Rig {
        let rig = Rig::unseen(name);
        rig.world.seed_host(HOST, "1.0.0");
        rig
    }

    /// The unit runs and the server knows no host: the agent's hello has not reached it.
    pub fn unseen(name: &str) -> Rig {
        let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let root = tmp.path().to_owned();
        let world = World::new(&root.join("world"), &root.join("control"), Box::new(NoGate));
        world.seed_unit(UNIT, "active", "running");
        let state_root = world.state_root().path().to_owned();
        let params = DrainParams {
            unit: UNIT.to_owned(),
            flags_file: root.join("flags"),
            token_file: root.join("token"),
            state_root,
            cordon_lease_secs: LEASE_SECS,
            defer_retry_secs: RETRY_SECS,
            drain_deadline_unix: None,
        };
        Rig { tmp, world, params }
    }

    /// The deadline is `secs` after the fake clock's start.
    pub fn deadline_in(&mut self, secs: u64) {
        self.params.drain_deadline_unix = Some(START_MS / 1000 + secs);
    }

    /// The agent's state root, as a path.
    pub fn root(&self) -> PathBuf {
        self.params.state_root.clone()
    }

    /// The text of the hold file, `None` if there is none.
    pub fn hold_text(&self) -> Option<String> {
        fs::read_to_string(self.root().join(HOLD_FILE)).ok()
    }

    /// The text of the renamed hold file, `None` if there is none.
    pub fn resume_text(&self) -> Option<String> {
        fs::read_to_string(self.root().join(HOLD_RESUME_FILE)).ok()
    }

    /// The expiry of the cordon in force, `None` if there is none.
    pub fn expiry(&self) -> Option<u64> {
        self.world.host().ok()?.cordon.map(|c| c.expires_at_ms)
    }

    /// A job runs on the host.
    pub fn busy(&self) {
        set_job(&self.world, true);
    }

    /// The host as the server reads it.
    pub fn host(&self) -> HostReading {
        self.world.host().unwrap()
    }

    /// Renames the hold file as a `resume` killed after its first change would have.
    pub fn rename_hold(&self) {
        fs::rename(
            self.root().join(HOLD_FILE),
            self.root().join(HOLD_RESUME_FILE),
        )
        .unwrap();
    }
}

/// A job starts on the server's view of the host, or ends.
pub fn set_job(world: &World, running: bool) {
    world.alter_host(|host| {
        host.leases = u64::from(running);
        host.lease = running.then(|| "lease-1".to_owned());
    });
}

/// The agent says hello, as a pause at which the agent comes up.
pub fn hello(probe: &Probe<'_>) {
    probe.note("hello");
    probe.world.hello("1.0.0").unwrap();
}

/// What a step that ends well and has nothing to say ends as.
pub fn succeeded() -> Done {
    Done {
        outcome: Outcome::Success,
        report: Report::default(),
    }
}

/// A success that says `detail`.
pub fn succeeded_with(detail: &str) -> Done {
    Done {
        outcome: Outcome::Success,
        report: Report {
            detail: Some(detail.to_owned()),
            ..Report::default()
        },
    }
}

/// A failure of the environment, exit 1.
pub fn environment_failure(reason: Reason, detail: &str) -> Done {
    Done {
        outcome: Outcome::Failure,
        report: Report {
            class: Class::Environment,
            ..Report::failure(reason, detail)
        },
    }
}

/// The text `drain` writes in the hold file.
pub fn note_text(cordoned_before: bool, facts: Option<&str>) -> String {
    let mut text = format!("{HOLD_TEXT}cordoned_before={cordoned_before}\n");
    if let Some(facts) = facts {
        text.push_str(&format!("facts={facts}\n"));
    }
    text
}

type PauseHook<'a> = Box<dyn FnMut(&Probe<'a>, usize) + 'a>;

/// What a test reads the host facts with instead of the world's.
type FactsReader<'a> = Box<dyn Fn() -> Result<String, String> + 'a>;

/// The calls that change something, in the log's words.
const CHANGES: [&str; 8] = [
    "lease.cordon",
    "lease.uncordon",
    "units.stop",
    "units.start",
    "hold.hold",
    "hold.release",
    "hold.released",
    "hello",
];

/// Every port at once, over the world, with the log of the calls. A call that a test names fails
/// as the test says: the calls of one name are counted from the probe's start or the last
/// [`Probe::clear`], from 1.
pub struct Probe<'a> {
    pub world: &'a World,
    root: StateRoot,
    calls: RefCell<Vec<&'static str>>,
    on_pause: RefCell<Option<PauseHook<'a>>>,
    facts_reader: RefCell<Option<FactsReader<'a>>>,
    lease_faults: RefCell<BTreeMap<(&'static str, usize), LeaseError>>,
    text_faults: RefCell<BTreeMap<(&'static str, usize), String>>,
    /// The unit manager takes the stop and the unit stays as it was.
    pub stop_ignored: Cell<bool>,
}

impl<'a> Probe<'a> {
    pub fn new(rig: &'a Rig) -> Probe<'a> {
        Probe {
            world: &rig.world,
            root: rig.world.state_root(),
            calls: RefCell::new(Vec::new()),
            on_pause: RefCell::new(None),
            facts_reader: RefCell::new(None),
            lease_faults: RefCell::new(BTreeMap::new()),
            text_faults: RefCell::new(BTreeMap::new()),
            stop_ignored: Cell::new(false),
        }
    }

    /// Runs `drain` over the probe.
    pub fn drain(&self, params: &DrainParams) -> Done {
        step::drain(&self.env(), params)
    }

    /// Runs `resume` over the probe.
    pub fn resume(&self, params: &DrainParams) -> Done {
        step::resume(&self.env(), params)
    }

    fn env(&self) -> MaintainEnv<'_> {
        MaintainEnv {
            lease: self,
            holds: self,
            units: self,
            facts: self,
            clock: self,
            gate: self,
        }
    }

    /// `act` runs after each pause, with the number of pauses taken so far.
    pub fn at_pause(&self, act: impl FnMut(&Probe<'a>, usize) + 'a) {
        *self.on_pause.borrow_mut() = Some(Box::new(act));
    }

    /// The facts are read with `reader` (the host's own, over files the test changes) and not
    /// from the world.
    pub fn read_facts_with(&self, reader: impl Fn() -> Result<String, String> + 'a) {
        *self.facts_reader.borrow_mut() = Some(Box::new(reader));
    }

    /// The `nth` call named `call` (`lease.host`, `lease.cordon`, `lease.uncordon`) fails with
    /// `error`.
    pub fn fail_lease(&self, call: &'static str, nth: usize, error: LeaseError) {
        self.lease_faults.borrow_mut().insert((call, nth), error);
    }

    /// The `nth` call named `call` (`units.show`, `units.stop`, `units.start`, `hold.state`,
    /// `hold.hold`, `hold.release`, `hold.released`, `facts.read`) fails with `text`.
    pub fn fail(&self, call: &'static str, nth: usize, text: &str) {
        self.text_faults
            .borrow_mut()
            .insert((call, nth), text.to_owned());
    }

    /// Adds an event of the test's own to the log.
    pub fn note(&self, call: &'static str) {
        self.calls.borrow_mut().push(call);
    }

    /// The log.
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }

    /// How many calls named `call` the log holds.
    pub fn count(&self, call: &str) -> usize {
        self.calls.borrow().iter().filter(|&&c| c == call).count()
    }

    /// How many calls the lease server took.
    pub fn lease_calls(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("lease."))
            .count()
    }

    /// The calls that change something, in order.
    pub fn changes(&self) -> Vec<&'static str> {
        self.calls
            .borrow()
            .iter()
            .copied()
            .filter(|c| CHANGES.contains(c))
            .collect()
    }

    /// Asserts that each call named in `changes` follows a kill point at once, and that there
    /// is no other kill point.
    pub fn assert_gated(&self, changes: &[&str]) {
        let calls = self.calls();
        let mut made = 0;
        for (at, call) in calls.iter().enumerate() {
            if changes.contains(call) {
                made += 1;
                assert!(
                    at > 0 && calls[at - 1] == "gate",
                    "{call} at {at} follows no kill point: {calls:?}"
                );
            }
        }
        assert!(made > 0, "{calls:?}");
        assert_eq!(self.count("gate"), made, "{calls:?}");
    }

    /// Forgets the log, and the count of every call with it.
    pub fn clear(&self) {
        self.calls.borrow_mut().clear();
    }

    fn hit(&self, call: &'static str) -> usize {
        let mut calls = self.calls.borrow_mut();
        calls.push(call);
        calls.iter().filter(|&&c| c == call).count()
    }

    fn lease_fault(&self, call: &'static str) -> Option<LeaseError> {
        let nth = self.hit(call);
        self.lease_faults.borrow_mut().remove(&(call, nth))
    }

    fn text_fault(&self, call: &'static str) -> Option<String> {
        let nth = self.hit(call);
        self.text_faults.borrow_mut().remove(&(call, nth))
    }
}

impl Lease for Probe<'_> {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        match self.lease_fault("lease.cordon") {
            Some(error) => Err(error),
            None => self.world.cordon(lease_secs),
        }
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        match self.lease_fault("lease.uncordon") {
            Some(error) => Err(error),
            None => self.world.uncordon(),
        }
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        match self.lease_fault("lease.host") {
            Some(error) => Err(error),
            None => self.world.host(),
        }
    }
}

impl Units for Probe<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        match self.text_fault("units.show") {
            Some(text) => Err(PortError::Command(text)),
            None => self.world.show(unit),
        }
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        match self.text_fault("units.stop") {
            Some(text) => Err(PortError::Command(text)),
            None if self.stop_ignored.get() => Ok(()),
            None => self.world.stop_no_block(unit),
        }
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        match self.text_fault("units.start") {
            Some(text) => Err(PortError::Command(text)),
            None => self.world.start(unit),
        }
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.world.is_active(unit)
    }
}

impl Holds for Probe<'_> {
    fn state(&self) -> Result<HoldState, PortError> {
        match self.text_fault("hold.state") {
            Some(text) => Err(PortError::Io(text)),
            None => Holds::state(&self.root),
        }
    }

    fn hold(&self, text: &str) -> Result<(), PortError> {
        match self.text_fault("hold.hold") {
            Some(why) => Err(PortError::Io(why)),
            None => Holds::hold(&self.root, text),
        }
    }

    fn release(&self) -> Result<(), PortError> {
        match self.text_fault("hold.release") {
            Some(text) => Err(PortError::Io(text)),
            None => Holds::release(&self.root),
        }
    }

    fn released(&self) -> Result<(), PortError> {
        match self.text_fault("hold.released") {
            Some(text) => Err(PortError::Io(text)),
            None => Holds::released(&self.root),
        }
    }
}

impl Facts for Probe<'_> {
    fn read(&self) -> Result<String, String> {
        match self.text_fault("facts.read") {
            Some(text) => Err(text),
            None => match self.facts_reader.borrow().as_ref() {
                Some(reader) => reader(),
                None => Facts::read(self.world),
            },
        }
    }
}

impl Gate for Probe<'_> {
    fn point(&self) {
        self.note("gate");
    }
}

impl Clock for Probe<'_> {
    fn now_ms(&self) -> u64 {
        self.world.now_ms()
    }

    fn pause(&self, span: Duration) {
        self.note("clock.pause");
        self.world.pause(span);
        let taken = self.world.ticks();
        if let Some(act) = self.on_pause.borrow_mut().as_mut() {
            act(self, taken);
        }
    }
}
