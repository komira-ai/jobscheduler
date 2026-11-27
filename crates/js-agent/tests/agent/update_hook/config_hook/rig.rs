//! What the tests of the config hook's steps share: a tree with a state directory, params, a
//! unit manager that follows a script, a lease server that answers from a script and a clock
//! that waits for nothing, with a log of the calls made to each.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::update_hook::config::{Done, Env, Params, Record, prepare};
use js_agent::update_hook::{Clock, HostReading, Lease, LeaseError, PortError, UnitState, Units};
use js_agent::update_hook::{Cordoned, Uncordoned};

use crate::support::Scratch;

use super::super::step_rig::as_root;

pub const UNIT: &str = "js-agent.service";

/// What the hook says when the host's flags file names no host certificate and key, so that the
/// lease cannot be made ([`Rig::without_a_host_certificate`]).
pub const NO_CERTIFICATE: &str = "flags_file: the flags file names no --host-cert and --host-key: a host is identified by its certificate";

/// What the lease server says at a look.
#[derive(Clone, Copy, Debug)]
pub enum Say {
    /// The host in `state`, in the session that began at the stamp.
    Host(&'static str, Option<u64>),
    /// 409 unknown-host.
    Unknown,
    /// A refusal.
    Refused(&'static str),
    /// The server cannot be reached.
    Down(&'static str),
}

/// A lease that answers `host` as its script says; the last entry is repeated.
#[derive(Default)]
pub struct LeaseScript {
    script: RefCell<VecDeque<Say>>,
    last: Cell<Option<Say>>,
    pub looks: Cell<u32>,
}

impl LeaseScript {
    pub fn say(&self, says: &[Say]) {
        *self.script.borrow_mut() = says.iter().copied().collect();
        self.last.set(None);
    }
}

fn reading(state: &str, session: Option<u64>) -> HostReading {
    HostReading {
        id: "web-04".to_owned(),
        state: state.to_owned(),
        epoch: 1,
        session_started_ms: session,
        leases: 0,
        lease: None,
        releasing: false,
        mark: None,
        cordon: None,
        taint: None,
        agent_version: Some("1.4.0".to_owned()),
        runtime: Some("crun".to_owned()),
    }
}

impl Lease for &LeaseScript {
    fn cordon(&self, _: u64) -> Result<Cordoned, LeaseError> {
        panic!("the config hook never cordons");
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        panic!("the config hook never uncordons");
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        self.looks.set(self.looks.get() + 1);
        let next = self.script.borrow_mut().pop_front();
        let said = next
            .or_else(|| self.last.get())
            .unwrap_or(Say::Down("no script"));
        self.last.set(Some(said));
        match said {
            Say::Host(state, session) => Ok(reading(state, session)),
            Say::Unknown => Err(LeaseError::UnknownHost),
            Say::Refused(code) => Err(LeaseError::Refused(code.to_owned())),
            Say::Down(why) => Err(LeaseError::Unreachable(why.to_owned())),
        }
    }
}

/// A unit manager: the unit shows its state, which moves on through a script at each look.
pub struct UnitScript {
    pub state: RefCell<(String, String)>,
    pending: RefCell<VecDeque<(String, String)>>,
    /// What the unit goes through after a stop.
    pub on_stop: RefCell<Vec<(&'static str, &'static str)>>,
    /// What the unit goes through after a start.
    pub on_start: RefCell<Vec<(&'static str, &'static str)>>,
    pub fail: RefCell<Vec<&'static str>>,
    pub log: RefCell<Vec<String>>,
    /// The file read at each start, as the agent reads its flags then.
    pub watch: RefCell<Option<PathBuf>>,
    /// What the watched file held at each start; `None` when it could not be read.
    pub started_on: RefCell<Vec<Option<Vec<u8>>>>,
}

impl Default for UnitScript {
    fn default() -> Self {
        Self {
            state: RefCell::new(("active".to_owned(), "running".to_owned())),
            pending: RefCell::default(),
            on_stop: RefCell::new(vec![("inactive", "dead")]),
            on_start: RefCell::new(vec![("active", "running")]),
            fail: RefCell::default(),
            log: RefCell::default(),
            watch: RefCell::default(),
            started_on: RefCell::default(),
        }
    }
}

impl UnitScript {
    pub fn is(&self, active: &str, sub: &str) {
        *self.state.borrow_mut() = (active.to_owned(), sub.to_owned());
    }

    fn broken(&self, call: &str) -> Result<(), PortError> {
        if self.fail.borrow().contains(&call) {
            Err(PortError::Command(format!("{call} failed")))
        } else {
            Ok(())
        }
    }

    /// The unit goes through `script` at its next looks, whatever is asked of it.
    pub fn follow(&self, script: &[(&'static str, &'static str)]) {
        self.then(script);
    }

    fn then(&self, script: &[(&'static str, &'static str)]) {
        *self.pending.borrow_mut() = script
            .iter()
            .map(|(active, sub)| ((*active).to_owned(), (*sub).to_owned()))
            .collect();
    }
}

impl Units for UnitScript {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        assert_eq!(unit, UNIT);
        self.log.borrow_mut().push("show".to_owned());
        self.broken("show")?;
        if let Some(next) = self.pending.borrow_mut().pop_front() {
            *self.state.borrow_mut() = next;
        }
        let (active, sub) = self.state.borrow().clone();
        Ok(UnitState { active, sub })
    }

    fn stop_no_block(&self, _: &str) -> Result<(), PortError> {
        self.log.borrow_mut().push("stop".to_owned());
        self.broken("stop")?;
        self.then(&self.on_stop.borrow());
        Ok(())
    }

    fn start(&self, _: &str) -> Result<(), PortError> {
        self.log.borrow_mut().push("start".to_owned());
        self.broken("start")?;
        let read = self
            .watch
            .borrow()
            .as_ref()
            .and_then(|path| fs::read(path).ok());
        self.started_on.borrow_mut().push(read);
        self.then(&self.on_start.borrow());
        Ok(())
    }

    fn is_active(&self, _: &str) -> Result<bool, PortError> {
        panic!("the config hook reads the unit with show");
    }
}

/// A clock that waits for nothing: a pause moves its time on.
#[derive(Default)]
pub struct Ticks {
    now: Cell<u64>,
    pub pauses: Cell<u32>,
}

impl Clock for Ticks {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn pause(&self, span: Duration) {
        self.pauses.set(self.pauses.get() + 1);
        self.now.set(self.now.get() + span.as_millis() as u64);
    }
}

/// The tree of one test.
pub struct Rig {
    _files: Scratch,
    pub params: Params,
    /// The host's root the steps place files below: the params' paths are in it.
    pub root: PathBuf,
    /// The staged release directory `prepare` is given (its package's files below `tree`).
    pub staged: PathBuf,
    pub units: UnitScript,
    pub lease: LeaseScript,
    pub clock: Ticks,
    /// Why the flags file cannot be used, when it cannot.
    pub flags: RefCell<Option<String>>,
}

impl Rig {
    pub fn new(name: &str) -> Rig {
        let files = Scratch::new(name);
        let root = files.path("tree");
        fs::create_dir_all(&root).unwrap();
        Rig::at(files, &root, "")
    }

    /// A rig whose params are of an update from the packaged version 1.4.0, not a first install.
    pub fn update(name: &str) -> Rig {
        let files = Scratch::new(name);
        let root = files.path("tree");
        fs::create_dir_all(&root).unwrap();
        Rig::at(files, &root, "\"updater.previous_version\" = \"1.4.0\"\n")
    }

    /// A rig whose every path is under `var/lib`, where `var` is a link to `private/var` as on
    /// a Mac (`/var` is a link into `/private`).
    pub fn behind_a_system_link(name: &str) -> Rig {
        let files = Scratch::new(name);
        let top = files.path("tree");
        fs::create_dir_all(top.join("private/var")).unwrap();
        std::os::unix::fs::symlink("private/var", top.join("var")).unwrap();
        Rig::at(files, &top.join("var/lib"), "")
    }

    fn at(files: Scratch, root: &Path, extra: &str) -> Rig {
        let table = text(&root.display().to_string(), 600, 300, 600) + extra;
        let params = Params::parse(&table).unwrap();
        let units = UnitScript::default();
        *units.watch.borrow_mut() = Some(params.flags_file.clone());
        Rig {
            staged: files.path("staged"),
            _files: files,
            params,
            root: root.to_owned(),
            units,
            lease: LeaseScript::default(),
            clock: Ticks::default(),
            flags: RefCell::default(),
        }
    }

    /// The flags file names no host certificate and key: the lease cannot be made.
    pub fn without_a_host_certificate(&self) {
        *self.flags.borrow_mut() = Some(NO_CERTIFICATE.to_owned());
    }

    pub fn record_path(&self) -> std::path::PathBuf {
        self.params.state_dir.join("record.json")
    }

    /// The record on disk, `None` when there is none.
    pub fn record(&self) -> Option<Record> {
        fs::read(self.record_path())
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    }

    /// Writes `record` as the record.
    pub fn put(&self, record: &Record) {
        fs::create_dir_all(&self.params.state_dir).unwrap();
        fs::write(self.record_path(), serde_json::to_vec(record).unwrap()).unwrap();
    }

    /// A record of this unit with nothing done.
    pub fn fresh(&self) -> Record {
        Record::new(&self.params)
    }

    pub fn calls(&self) -> Vec<String> {
        self.units
            .log
            .borrow()
            .iter()
            .filter(|call| *call != "show")
            .cloned()
            .collect()
    }

    /// Runs `step` on the fakes.
    pub fn run(&self, step: impl FnOnce(&Env<'_>, &Params) -> Done) -> Done {
        let lease = |_: &Params| match self.flags.borrow().clone() {
            Some(why) => Err(why),
            None => Ok(Box::new(&self.lease) as Box<dyn Lease + '_>),
        };
        let env = Env {
            units: &self.units,
            clock: &self.clock,
            stat: &as_root,
            lease: &lease,
            root: &self.root,
        };
        step(&env, &self.params)
    }

    /// Runs `prepare` of the release staged in [`Rig::staged`] on the fakes.
    pub fn prepare(&self) -> Done {
        self.run(|env, params| prepare(env, params, &self.staged))
    }
}

/// The params table of a tree at `root`, with the three bounds.
pub fn text(root: &str, cutover: u64, verify: u64, rollback: u64) -> String {
    format!(
        concat!(
            "unit = \"{unit}\"\nflags_file = \"{root}/flags\"\n",
            "state_root = \"{root}/state-root\"\nstate_dir = \"{root}/state\"\n",
            "cutover_timeout_secs = {cutover}\nverify_timeout_secs = {verify}\n",
            "rollback_timeout_secs = {rollback}\n"
        ),
        unit = UNIT,
        root = root,
        cutover = cutover,
        verify = verify,
        rollback = rollback,
    )
}

/// A step of the hook run on a rig.
pub type StepFn = fn(&Rig) -> Done;

/// The five steps.
pub fn all_steps() -> [StepFn; 5] {
    use js_agent::update_hook::config::{cleanup, cutover, rollback, verify};
    [
        Rig::prepare,
        |rig| rig.run(cutover),
        |rig| rig.run(verify),
        |rig| rig.run(rollback),
        |rig| rig.run(cleanup),
    ]
}
