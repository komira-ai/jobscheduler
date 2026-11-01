//! What the tests of `prepare` and `cleanup` share: a directory tree with a state directory,
//! the staged and the previous release, the fakes' world, and a [`Spy`] that is every port at
//! once, keeps the call log and can fail a call or act at a pause.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use js_agent::CommandRunner;
use js_agent::lifecycle::StateRoot;
use js_agent::update_hook::step::{self, Env};
use js_agent::update_hook::{
    Clock, Cordoned, Declaration, DirStat, Done, Gate, HostReading, Lease, LeaseError, Link,
    Params, PortError, Ports, Record, StateView, Uncordoned, UnitState, Units, UpdateInfo,
};
use js_agent_wire::Taint;
use js_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

use super::fakes::{START_MS, World};

pub const UNIT: &str = "js-agent-crun.service";
/// The launchd label of the agent on a Mac, and what the Mac declaration says it runs.
pub const MAC_UNIT: &str = "com.komira.js-agent";
pub const HOST: &str = "burst-1";
pub const LEASE_SECS: u64 = 300;
pub const MAX_SECS: u64 = 17_355;
/// The bound of the drain wait and the poll interval the steps use.
pub const DRAIN_WAIT_SECS: u64 = 3600;
pub const POLL_SECS: u64 = 10;
/// The retry hint a deferred `prepare` gives.
pub const RETRY_AFTER_SECS: u64 = 300;
/// What the staged `js-agent` file holds: the digest in the record is the hash of it.
pub const BINARY: &[u8] = b"not really an agent";

/// A state directory that is as the hook requires, whoever runs the test.
pub fn as_root(_: &Path, _: BorrowedFd<'_>) -> io::Result<DirStat> {
    Ok(DirStat {
        is_symlink: false,
        is_dir: true,
        uid: 0,
        gid: 0,
        mode: 0o700,
    })
}

/// The kill points: counts them, and panics at the one it is armed for (a process killed
/// there, from the point of view of the step: nothing after it runs).
#[derive(Default)]
pub struct Kill {
    points: Cell<u32>,
    at: Cell<Option<u32>>,
}

impl Kill {
    pub fn points(&self) -> u32 {
        self.points.get()
    }

    /// Counts from zero again and dies at the `n`th point of the next run.
    pub fn arm(&self, n: u32) {
        self.points.set(0);
        self.at.set(Some(n));
    }

    /// Counts from zero again and never dies.
    pub fn disarm(&self) {
        self.points.set(0);
        self.at.set(None);
    }
}

impl Gate for Kill {
    fn point(&self) {
        let n = self.points.get() + 1;
        self.points.set(n);
        assert!(self.at.get() != Some(n), "killed at kill point {n}");
    }
}

struct Shared(Rc<Kill>);

impl Gate for Shared {
    fn point(&self) {
        self.0.point();
    }
}

/// The tree of one test.
pub struct Rig {
    pub tmp: TestDir,
    pub kill: Rc<Kill>,
    pub world: World,
    pub declaration: Declaration,
    pub params: Params,
    pub link: PathBuf,
    /// The unit the declaration names.
    pub unit: String,
}

impl Rig {
    /// An update from 1.3.0 to 1.4.0 on a host that holds no lease.
    pub fn new(name: &str) -> Rig {
        Rig::build(name, true, LEASE_SECS, MAX_SECS)
    }

    /// The first install: no previous release, no host the server knows.
    pub fn first_install(name: &str) -> Rig {
        Rig::build(name, false, LEASE_SECS, MAX_SECS)
    }

    /// An update with the cordon's lease and cap set.
    pub fn with_cordon(name: &str, lease_secs: u64, max_secs: u64) -> Rig {
        Rig::build(name, true, lease_secs, max_secs)
    }

    /// The same update on a Mac: the launchd label for the unit, the declaration naming the
    /// `native` runtime, and an agent whose hello says `sandbox-exec`.
    pub fn mac(name: &str) -> Rig {
        Rig::build_for(name, true, LEASE_SECS, MAX_SECS, Some(MAC_UNIT))
    }

    /// The first install on a Mac.
    pub fn mac_first_install(name: &str) -> Rig {
        Rig::build_for(name, false, LEASE_SECS, MAX_SECS, Some(MAC_UNIT))
    }

    fn build(name: &str, previous: bool, lease_secs: u64, max_secs: u64) -> Rig {
        Rig::build_for(name, previous, lease_secs, max_secs, None)
    }

    fn build_for(
        name: &str,
        previous: bool,
        lease_secs: u64,
        max_secs: u64,
        mac: Option<&str>,
    ) -> Rig {
        let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let root = tmp.path().to_owned();
        let (staged, old) = (root.join("releases/1.4.0"), root.join("releases/1.3.0"));
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("js-agent"), BINARY).unwrap();
        fs::create_dir_all(root.join("state")).unwrap();
        let unit = mac.unwrap_or(UNIT);
        let runtime = if mac.is_some() {
            "runtime = \"native\"\n"
        } else {
            ""
        };
        let declaration = Declaration::parse(&format!(
            concat!(
                "state_dir = \"{root}/state\"\n[hook]\nunits = [\"{unit}\"]\n{runtime}",
                "write_paths = [\"{root}/releases\", \"{root}/link\", \"{root}/state\"]\n",
                "[hook.timeout_secs]\nprepare = 3900\ncutover = 4320\nverify = 615\n",
                "rollback = 8220\ncleanup = 120\n"
            ),
            root = root.display(),
            unit = unit,
            runtime = runtime,
        ))
        .unwrap();
        let previous = if previous {
            format!(
                "\"updater.previous_release\" = \"{}\"\n\"updater.previous_version\" = \"1.3.0\"\n",
                old.display()
            )
        } else {
            String::new()
        };
        let link = root.join("link/current");
        let release_link = format!("\"release_link\" = \"{}\"\n", link.display());
        let params = Params::parse(&format!(
            concat!(
                "cordon_lease_secs = {lease}\ncordon_max_secs = {max}\n{release_link}",
                "drain_wait_secs = {wait}\ndefer_retry_secs = {retry}\n",
                "\"updater.artifact_digest\" = \"{digest}\"\n",
                "\"updater.staged_release\" = \"{staged}\"\n",
                "\"updater.target_version\" = \"1.4.0\"\n{previous}"
            ),
            lease = lease_secs,
            max = max_secs,
            wait = DRAIN_WAIT_SECS.min(max_secs),
            retry = RETRY_AFTER_SECS,
            digest = digest(),
            staged = staged.display(),
            previous = previous,
            release_link = release_link,
        ))
        .unwrap();
        let kill = Rc::new(Kill::default());
        let world = World::new(
            &root.join("world"),
            &root.join("control"),
            Box::new(Shared(Rc::clone(&kill))),
        );
        if mac.is_some() {
            world.set_exec_mode("sandbox-exec");
        }
        world.seed_update(&link, HOST, &params);
        Rig {
            tmp,
            kill,
            world,
            declaration,
            params,
            link,
            unit: unit.to_owned(),
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    pub fn record_path(&self) -> PathBuf {
        self.state_dir().join("record.json")
    }

    /// The record on disk, `None` if there is none.
    pub fn record(&self) -> Option<Record> {
        self.record_bytes()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    }

    pub fn record_bytes(&self) -> Option<Vec<u8>> {
        fs::read(self.record_path()).ok()
    }

    /// Puts a record in place, as an earlier step would have left it.
    pub fn put_record(&self, record: &Record) {
        fs::write(
            self.record_path(),
            serde_json::to_vec_pretty(record).unwrap(),
        )
        .unwrap();
    }

    /// The record a `prepare` of this update starts from.
    pub fn fresh_record(&self) -> Record {
        Record::new(UpdateInfo {
            target: self.params.target_version.clone(),
            previous: self.params.previous_version.clone(),
            staged: self.params.staged_release.clone(),
            previous_target: self.params.previous_release.clone(),
            unit: self.unit.clone(),
            artifact_digest: self.params.artifact_digest.clone(),
        })
    }

    /// The host as the server has it now.
    pub fn host(&self) -> HostReading {
        self.world.host().unwrap()
    }

    pub fn cordon_expiry(&self) -> Option<u64> {
        self.world.host().ok()?.cordon.map(|c| c.expires_at_ms)
    }

    /// Runs (or stops) the leases of the host.
    pub fn set_leases(&self, leases: u64) {
        self.world.alter_host(|host| {
            host.leases = leases;
            host.lease = (leases > 0).then(|| "lease-1".to_owned());
        });
    }

    /// The agent's state root.
    pub fn state_root(&self) -> StateRoot {
        self.world.state_root()
    }

    pub fn taint(&self) {
        self.state_root()
            .write(&Taint {
                reason: "residue".to_owned(),
                run_id: None,
                check: None,
            })
            .unwrap();
    }

    /// The server sees the operator's cordon, for 900 s.
    pub fn operator_cordons(&self) -> u64 {
        self.world
            .cordon(900)
            .unwrap()
            .host
            .cordon
            .unwrap()
            .expires_at_ms
    }
}

/// The digest the record holds for the staged release these tests make.
pub fn digest() -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(BINARY)))
}

/// The last line of a step, parsed.
pub fn line(done: &Done) -> serde_json::Value {
    serde_json::from_str(&done.report.to_line()).unwrap()
}

type PauseHook<'a> = Box<dyn FnMut(&Spy<'a>, usize) + 'a>;

/// A change to the unit or the link, with the record's marks (`stop_begun`, `link_moved`,
/// `start_issued`) as they were on disk when the step reached it and the kill points it had
/// passed: killing at `points` ends the step just before the act.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Act {
    pub call: String,
    pub marks: [bool; 3],
    pub points: u32,
}

/// Every port at once, over the fakes, with the log of what was done.
pub struct Spy<'a> {
    pub world: &'a World,
    pub kill: &'a Kill,
    state: StateRoot,
    pub log: RefCell<Vec<String>>,
    on_pause: RefCell<Option<PauseHook<'a>>>,
    /// The calls that fail, by name: `host`, `cordon`, `uncordon`, `owed`.
    pub fail: RefCell<BTreeSet<&'static str>>,
    /// The operator cordons just before the next cordon call reaches the server.
    pub operator_races: Cell<bool>,
    /// What the staged agent prints to `--version`.
    pub version: RefCell<Result<String, String>>,
    /// The changes made to the unit and the link, in order.
    pub acts: RefCell<Vec<Act>>,
    /// The unit state a stop leaves behind instead of `inactive`, `(ActiveState, SubState)`.
    pub stop_leaves: RefCell<Option<(&'static str, &'static str)>>,
    /// The unit state a start leaves behind instead of `active`.
    pub start_leaves: RefCell<Option<(&'static str, &'static str)>>,
    record_path: PathBuf,
}

impl<'a> Spy<'a> {
    pub fn new(rig: &'a Rig) -> Spy<'a> {
        Spy {
            world: &rig.world,
            kill: &rig.kill,
            state: rig.world.state_root(),
            log: RefCell::new(Vec::new()),
            on_pause: RefCell::new(None),
            fail: RefCell::new(BTreeSet::new()),
            operator_races: Cell::new(false),
            version: RefCell::new(Ok(format!("js-agent {}\n", rig.params.target_version))),
            acts: RefCell::new(Vec::new()),
            stop_leaves: RefCell::new(None),
            start_leaves: RefCell::new(None),
            record_path: rig.record_path(),
        }
    }

    /// Calls `hook(spy, ticks)` after each pause, `ticks` being the pauses so far.
    pub fn on_pause(&self, hook: impl FnMut(&Spy<'a>, usize) + 'a) {
        *self.on_pause.borrow_mut() = Some(Box::new(hook));
    }

    pub fn fail(&self, call: &'static str) {
        self.fail.borrow_mut().insert(call);
    }

    pub fn heal(&self, call: &'static str) {
        self.fail.borrow_mut().remove(call);
    }

    pub fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    fn push(&self, entry: impl Into<String>) {
        self.log.borrow_mut().push(entry.into());
    }

    /// Logs a change to the unit or the link, with the marks on disk at this moment.
    fn act(&self, call: String) {
        let marks = fs::read(&self.record_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            .map_or([false; 3], |r| [r.stop_begun, r.link_moved, r.start_issued]);
        self.acts.borrow_mut().push(Act {
            call: call.clone(),
            marks,
            points: self.kill.points(),
        });
        self.push(call);
    }

    /// The acts made so far, by name.
    pub fn act_names(&self) -> Vec<String> {
        self.acts.borrow().iter().map(|a| a.call.clone()).collect()
    }

    /// Fails a call of the unit manager or the link that is set to fail.
    fn port_check(&self, call: &'static str) -> Result<(), PortError> {
        if self.fail.borrow().contains(call) {
            Err(PortError::Command(format!("{call} is down")))
        } else {
            Ok(())
        }
    }

    fn check(&self, call: &'static str) -> Result<(), LeaseError> {
        if self.fail.borrow().contains(call) {
            Err(LeaseError::Unreachable(format!("{call} is down")))
        } else {
            Ok(())
        }
    }

    pub fn env<'b>(
        &'b self,
        rig: &'b Rig,
        stat: &'b dyn Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat>,
    ) -> Env<'b> {
        Env {
            ports: Ports {
                units: self,
                link: self,
                lease: self,
                state: self,
                clock: self,
                gate: self.kill,
            },
            runner: self,
            declaration: &rig.declaration,
            stat,
        }
    }

    pub fn prepare(&self, rig: &Rig) -> Done {
        step::prepare(&self.env(rig, &as_root), &rig.params)
    }

    pub fn cleanup(&self, rig: &Rig) -> Done {
        step::cleanup(&self.env(rig, &as_root), &rig.params)
    }

    pub fn cutover(&self, rig: &Rig) -> Done {
        step::cutover(&self.env(rig, &as_root), &rig.params)
    }

    pub fn verify(&self, rig: &Rig) -> Done {
        step::verify(&self.env(rig, &as_root), &rig.params)
    }
}

impl Units for Spy<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        self.push(format!("units.show({unit})"));
        self.port_check("show")?;
        self.world.show(unit)
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        self.act(format!("units.stop({unit})"));
        self.port_check("stop")?;
        match *self.stop_leaves.borrow() {
            Some((active, sub)) => {
                self.world.seed_unit(unit, active, sub);
                Ok(())
            }
            None => self.world.stop_no_block(unit),
        }
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        self.act(format!("units.start({unit})"));
        let leaves = *self.start_leaves.borrow();
        if let Some((active, sub)) = leaves {
            self.world.seed_unit(unit, active, sub);
        }
        self.port_check("start")?;
        match leaves {
            Some(_) => Ok(()),
            None => self.world.start(unit),
        }
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.push(format!("units.is_active({unit})"));
        self.world.is_active(unit)
    }
}

impl Link for Spy<'_> {
    fn target(&self, link: &Path) -> Result<Option<PathBuf>, PortError> {
        self.world.target(link)
    }

    fn relink(&self, link: &Path, target: &Path) -> Result<(), PortError> {
        self.act("link.relink".to_owned());
        self.port_check("relink")?;
        self.world.relink(link, target)
    }
}

impl Lease for Spy<'_> {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        self.check("cordon")?;
        if self.operator_races.replace(false) {
            self.world.cordon(900)?;
        }
        self.push(format!("cordon({lease_secs})"));
        self.world.cordon(lease_secs)
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        self.check("uncordon")?;
        self.push("uncordon");
        self.world.uncordon()
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        self.check("host")?;
        self.world.host()
    }
}

impl StateView for Spy<'_> {
    fn taint(&self) -> Option<Taint> {
        StateView::taint(&self.state)
    }

    fn owed(&self) -> Result<Vec<String>, PortError> {
        if self.fail.borrow().contains("owed") {
            return Err(PortError::Io("the state root cannot be listed".to_owned()));
        }
        StateView::owed(&self.state)
    }
}

impl Clock for Spy<'_> {
    fn now_ms(&self) -> u64 {
        self.world.now_ms()
    }

    fn pause(&self, span: Duration) {
        self.world.pause(span);
        let ticks = self.world.ticks();
        // Taken out while it runs, so that it may change the spy.
        let hook = self.on_pause.borrow_mut().take();
        if let Some(mut hook) = hook {
            hook(self, ticks);
            *self.on_pause.borrow_mut() = Some(hook);
        }
    }
}

impl CommandRunner for Spy<'_> {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.push(format!("run({program} {})", args.join(" ")));
        self.version.borrow().clone()
    }

    fn run_in(&self, program: &str, _: &[&str], _: &[(&str, &str)]) -> Result<String, String> {
        Err(format!("{program}: run_in is not used by the hook"))
    }
}

/// Runs `work`, which may be killed at a kill point: `None` when it was.
pub fn killable<T>(work: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(work)).ok()
}

/// The time the fakes' world starts at.
pub const START: u64 = START_MS;

/// The host as `prepare` leaves it for `cutover`: the agent running on the previous release,
/// the cordon ours and the record written.
pub fn running(rig: Rig) -> Rig {
    rig.world.seed_unit(&rig.unit, "active", "running");
    let done = Spy::new(&rig).prepare(&rig);
    assert!(
        done.report.reason.is_none(),
        "prepare of the setup: {done:?}"
    );
    // The setup passed kill points; the count is of the step's.
    rig.kill.disarm();
    rig
}

/// Everything a `cutover` leaves behind, without the directories of the rig: the record, the
/// link, the unit and the cordon.
pub fn end_state(rig: &Rig) -> serde_json::Value {
    let unit = rig.world.show(&rig.unit).unwrap();
    let state = serde_json::json!({
        "record": rig.record(),
        "link": rig.world.target(&rig.link).unwrap(),
        "unit": format!("{} {}", unit.active, unit.sub),
        "cordon": rig.cordon_expiry(),
    });
    let root = rig.tmp.path().display().to_string();
    serde_json::from_str(&state.to_string().replace(&root, "<root>")).unwrap()
}

/// The session of the host as the server showed it just before the stop, as the record on disk
/// holds it.
pub fn pre_stop(rig: &Rig) -> Option<u64> {
    rig.record().expect("a record").pre_stop_session_ms
}

/// The `stop_begun`, `link_moved` and `start_issued` marks on disk.
pub fn marks(rig: &Rig) -> [bool; 3] {
    let record = rig.record().unwrap();
    [record.stop_begun, record.link_moved, record.start_issued]
}
