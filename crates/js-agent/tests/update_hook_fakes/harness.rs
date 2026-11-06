//! The fake host as the conformance harness of the hook contract runs it (`update_hook::
//! conformance`, a dev-dependency): what `js-update-hook-fake` reads from its environment, the
//! params it runs the steps under, the kill points spoken in the harness's
//! protocol, the faults a test arranges for a step, and the hang guard.
//!
//! The binary cannot use a dev-dependency, so it speaks the harness's kill-point protocol
//! itself ([`HarnessGate`]); a test pins the names and the count file against the harness's own
//! (`tests/agent/update_hook/conformance.rs`).
//!
//! A fault is read, never written: the ports the step sees are the world's, seen through
//! [`Faulted`], which makes the one call of the step fail that a real host would fail (a lease
//! that never goes, a taint, an agent that crashes). The steps then end as the fault asks
//! through their own code.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::CommandRunner;
use js_agent::update_hook::{
    Clock, Cordoned, Gate, HostReading, Lease, LeaseError, Link, PortError, RECORD_FILE, Record,
    StateView, Step, Uncordoned, UnitState, Units,
};
use js_agent_wire::Taint;

use super::World;
use super::watchdog;

/// Names the fakes' world (the harness's `world/`).
pub const WORLD_VAR: &str = "UPDATE_HOOK_FAKE_WORLD";
/// Names the fakes' own files: the clock, the releases, the faults. Outside the harness's
/// sandbox, which it guards.
pub const CONTROL_VAR: &str = "UPDATE_HOOK_FAKE_CONTROL";
/// Names the hook's state directory (the harness's `record/`).
pub const STATE_DIR_VAR: &str = "UPDATE_HOOK_FAKE_STATE_DIR";
/// Names a defect the binary adds to the hook, for the tests that require the harness to
/// refuse it ([`Defect`]).
pub const DEFECT_VAR: &str = "UPDATE_HOOK_FAKE_DEFECT";

/// The harness's kill-point protocol (`update_hook::conformance::gate`): the directory of the
/// count file, the point to stop at, the line a parked step writes and the count file.
pub const GATE_DIR_VAR: &str = "UPDATE_HOOK_GATE_DIR";
/// See [`GATE_DIR_VAR`].
pub const KILL_AT_VAR: &str = "UPDATE_HOOK_KILL_AT";
/// See [`GATE_DIR_VAR`].
pub const PARKED: &str = "update-hook-gate: parked at the kill point";
/// See [`GATE_DIR_VAR`].
pub const COUNT_FILE: &str = "gate-count";

/// The unit the params name.
pub const UNIT: &str = "js-agent-crun.service";
/// The release link the params name. The fake keeps it in the world (`link/current`); nothing
/// at this path is touched. The updater's part (moving it) is the binary's, before a step.
pub const RELEASE_LINK: &str = "/opt/js-agent/current";
/// The host the lease server knows.
pub const HOST: &str = "burst-1";

/// The `[hook.timeout_secs]` of the declaration: the updater's bound on `step`, which the hang
/// guard holds the step's waits to.
#[must_use]
pub const fn timeout_secs(step: Step) -> u64 {
    match step {
        Step::Prepare => 3900,
        Step::Cutover => 4320,
        Step::Verify => 615,
        Step::Rollback => 8220,
        Step::Cleanup => 120,
    }
}

/// The manifest's own `[params]` (the harness adds the `updater.*` keys). The state directory
/// is a placeholder: the binary runs the steps over the harness's record directory, which only
/// the binary's flag or variable names.
pub const PARAMS: [(&str, ParamValue); 17] = [
    ("unit", ParamValue::Text(UNIT)),
    ("state_dir", ParamValue::Text("/var/lib/js-agent-update")),
    ("release_link", ParamValue::Text(RELEASE_LINK)),
    ("flags_file", ParamValue::Text("/etc/js-agent/flags")),
    ("token_file", ParamValue::Text("/etc/js-agent/token")),
    ("state_root", ParamValue::Text("/var/lib/js-agent/state")),
    ("runtime", ParamValue::Text("crun")),
    ("cordon_lease_secs", ParamValue::Integer(300)),
    ("cordon_max_secs", ParamValue::Integer(17_355)),
    ("drain_wait_secs", ParamValue::Integer(3600)),
    ("defer_retry_secs", ParamValue::Integer(300)),
    ("prepare_timeout_secs", ParamValue::Integer(3900)),
    ("cutover_timeout_secs", ParamValue::Integer(4320)),
    ("verify_timeout_secs", ParamValue::Integer(615)),
    ("rollback_timeout_secs", ParamValue::Integer(8220)),
    ("cleanup_timeout_secs", ParamValue::Integer(120)),
    ("verify_deadline_secs", ParamValue::Integer(9000)),
];

/// [`PARAMS`] as the text of a params file.
#[must_use]
pub fn params_text() -> String {
    PARAMS
        .iter()
        .map(|(key, value)| match value {
            ParamValue::Integer(n) => format!("{key} = {n}\n"),
            ParamValue::Text(text) => format!("{key} = \"{text}\"\n"),
        })
        .collect()
}

/// A value of [`PARAMS`].
#[derive(Clone, Copy, Debug)]
pub enum ParamValue {
    /// A whole number.
    Integer(i64),
    /// Text.
    Text(&'static str),
}

/// The exit status of a step whose kill point could not be counted: none of the contract's, so
/// the harness refuses the run by name (`UnknownExit`) rather than count fewer points.
pub const EXIT_GATE_BROKEN: u8 = 70;

/// The kill points, in the harness's protocol: each point appends a byte to the count file in
/// the directory [`GATE_DIR_VAR`] names, and the point whose number is [`KILL_AT_VAR`]'s writes
/// [`PARKED`] to standard error and parks until the harness kills it. Without the directory a
/// point does nothing; with it, a count that cannot be kept ends the step with
/// [`EXIT_GATE_BROKEN`], since a point left out would be one the harness never kills at.
pub struct HarnessGate {
    dir: Option<PathBuf>,
    kill_at: u64,
    /// [`Defect::BrokenCount`]: the count file is put out of reach after the first point.
    pub break_after_first: bool,
}

impl HarnessGate {
    /// From the values of the two variables. A missing or unreadable number is `0`: count,
    /// never stop.
    #[must_use]
    pub fn new(dir: Option<OsString>, kill_at: Option<OsString>) -> Self {
        let kill_at = kill_at
            .and_then(|value| value.to_str()?.parse().ok())
            .unwrap_or(0);
        Self {
            dir: dir.map(PathBuf::from),
            kill_at,
            break_after_first: false,
        }
    }

    /// From this process's environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os(GATE_DIR_VAR),
            std::env::var_os(KILL_AT_VAR),
        )
    }

    /// Counts one point and returns its number; `None` outside the harness.
    ///
    /// # Errors
    /// The count file could not be opened, written or looked at.
    pub fn count(&self) -> std::io::Result<Option<u64>> {
        let Some(dir) = self.dir.as_ref() else {
            return Ok(None);
        };
        let path = dir.join(COUNT_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(b".")?;
        let n = file.metadata()?.len();
        if self.break_after_first && n == 1 {
            // In place of the count file, a directory: the next point cannot open it.
            fs::remove_file(&path)?;
            fs::create_dir(&path)?;
        }
        Ok(Some(n))
    }
}

impl Gate for HarnessGate {
    fn point(&self) {
        if watchdog::point() {
            watchdog::kill_step(&format!(
                "the step reached more than {} kill points",
                watchdog::MAX_POINTS
            ));
        }
        match self.count() {
            Ok(Some(n)) if n == self.kill_at => {
                eprintln!("{PARKED}");
                // The harness kills the process group on the line; a wake-up parks again.
                loop {
                    std::thread::park();
                }
            }
            Ok(_) => {}
            Err(why) => {
                eprintln!("js-update-hook-fake: the kill point could not be counted: {why}");
                std::process::exit(i32::from(EXIT_GATE_BROKEN));
            }
        }
    }
}

/// The world's view of the hook's record, for the conformance harness's crash-window
/// allowance: a file there exactly while the record holds the cordon as the hook's own and
/// confirmed (`cordon.placed` and not `provisional`). The record keeps that in one field of one
/// file, and the harness tells a recorded cordon from one by which paths exist, so
/// [`MirroringGate`] keeps this file as the record says at every kill point and at the end of
/// every step. The hook neither reads nor writes it.
pub const OWNED_CORDON: &str = "record-view/cordon-ours";

/// Makes [`OWNED_CORDON`] in `world` say what the record in `state_dir` says.
///
/// # Panics
/// The file cannot be written or removed.
pub fn mirror_ownership(world: &Path, state_dir: &Path) {
    let ours = fs::read(state_dir.join(RECORD_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
        .is_some_and(|record| record.cordon.placed && !record.cordon.provisional);
    let file = world.join(OWNED_CORDON);
    if ours {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"").unwrap();
    } else {
        match fs::remove_file(&file) {
            Err(why) if why.kind() != std::io::ErrorKind::NotFound => panic!("{why}"),
            _ => {}
        }
    }
}

/// [`HarnessGate`], with [`OWNED_CORDON`] brought up to date before each point.
pub struct MirroringGate {
    /// The kill points.
    pub inner: HarnessGate,
    /// The fakes' world.
    pub world: PathBuf,
    /// The hook's state directory.
    pub state_dir: PathBuf,
}

impl Gate for MirroringGate {
    fn point(&self) {
        mirror_ownership(&self.world, &self.state_dir);
        self.inner.point();
    }
}

/// How a test asks a step to end: what the [`Faulted`] ports make go wrong while that step
/// runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The step fails (1): the staged agent is another version (`prepare`), the agent crashes
    /// on its start (`cutover`) or after it (`verify`), the lease server does not answer
    /// (`cleanup` of an update, `rollback`), the state directory cannot be looked at (`cleanup`
    /// of a first install, which asks the lease server nothing).
    Fail,
    /// The host is busy (75): it holds a lease that never goes (`prepare`, `cutover`), or, on a
    /// first install, the join token file is not as the agent needs it (`prepare`).
    Defer,
    /// No version can repair the host (76): the agent's state root holds a taint (`prepare`,
    /// `cutover`, `verify`, `rollback`).
    Halt,
}

impl Fault {
    /// The word of the fault file.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Fault::Fail => "fail",
            Fault::Defer => "defer",
            Fault::Halt => "halt",
        }
    }

    fn from_word(word: &str) -> Option<Fault> {
        [Fault::Fail, Fault::Defer, Fault::Halt]
            .into_iter()
            .find(|fault| fault.word() == word.trim())
    }

    /// Whether this fake can make `step` end so, by the means [`Fault`] lists: `verify`,
    /// `cleanup` and `rollback` never defer (the contract gives them none), and `cleanup` reads
    /// no taint, so a taint does not halt it.
    #[must_use]
    pub const fn reachable(self, step: Step) -> bool {
        match (step, self) {
            (Step::Prepare | Step::Cutover, _) | (_, Fault::Fail) => true,
            (Step::Verify | Step::Rollback, Fault::Halt) => true,
            (Step::Verify | Step::Cleanup | Step::Rollback, _) => false,
        }
    }

    /// [`Fault::reachable`] on a first install: only `prepare` defers (the join token file is
    /// not right), since there is no host to drain and no lease to wait out.
    #[must_use]
    pub const fn reachable_on_first_install(self, step: Step) -> bool {
        match self {
            Fault::Defer => matches!(step, Step::Prepare),
            _ => self.reachable(step),
        }
    }
}

fn fault_file(control: &Path, step: Step) -> PathBuf {
    control.join("faults").join(step.name())
}

/// Makes `step` end as `fault`, from now on.
///
/// # Panics
/// The file cannot be written.
pub fn arrange(control: &Path, step: Step, fault: Fault) {
    fs::create_dir_all(control.join("faults")).unwrap();
    fs::write(fault_file(control, step), fault.word()).unwrap();
}

/// The fault arranged for `step`, if any.
#[must_use]
pub fn arranged(control: &Path, step: Step) -> Option<Fault> {
    let word = fs::read_to_string(fault_file(control, step)).ok()?;
    Fault::from_word(&word)
}

/// A defect the binary adds to the hook, so that a test can require the harness to name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Defect {
    /// No last line: the step's report is not written.
    NoLastLine,
    /// `cutover` starts the agent again on every run, even when it already runs on the staged
    /// release: a second run is not a no-op.
    CutoverRestarts,
    /// The clock stands still and the new agent never says hello: `verify` waits forever.
    Hang,
    /// `verify` blocks in its first look at the unit and never pauses, as a step stuck in a
    /// call that never returns: only the watchdog ends it.
    Stall,
    /// `cleanup` deletes the record without lifting the cordon it recorded as its own.
    ForgetsCordon,
    /// The kill-point count file is put out of reach after the first point.
    BrokenCount,
    /// `rollback` lifts any cordon the host holds when it ends, an operator's as well.
    LiftsAnyCordon,
    /// `verify` loops for ever reaching kill points, never pausing.
    PointLoop,
    /// `verify` blocks as [`Defect::Stall`] does, but never announces it.
    SilentStall,
}

impl Defect {
    /// The value of [`DEFECT_VAR`].
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Defect::NoLastLine => "no-last-line",
            Defect::CutoverRestarts => "cutover-restarts",
            Defect::Hang => "hang",
            Defect::Stall => "stall",
            Defect::ForgetsCordon => "forgets-cordon",
            Defect::BrokenCount => "broken-count",
            Defect::LiftsAnyCordon => "lifts-any-cordon",
            Defect::PointLoop => "point-loop",
            Defect::SilentStall => "silent-stall",
        }
    }

    /// Every defect.
    pub const ALL: [Defect; 9] = [
        Defect::NoLastLine,
        Defect::CutoverRestarts,
        Defect::Hang,
        Defect::Stall,
        Defect::ForgetsCordon,
        Defect::BrokenCount,
        Defect::LiftsAnyCordon,
        Defect::PointLoop,
        Defect::SilentStall,
    ];

    /// The defect a value names; `None` for none or a word no defect has.
    #[must_use]
    pub fn from_word(word: Option<&OsString>) -> Option<Defect> {
        let word = word?.to_str()?;
        Defect::ALL.into_iter().find(|defect| defect.word() == word)
    }
}

/// The hang guard: a clock that holds a step's waits to the declared bound of the step, in the
/// time the step asks to wait (not the time the clock shows, which may stand still). The
/// updater kills a step that runs past its `timeout_secs`; the guard does the same to a step
/// whose waits add up past it, so a step that would never return is ended, and the harness
/// sees a step that died of a signal. Nothing here reads a real clock.
pub struct Guard<'a> {
    clock: &'a dyn Clock,
    budget: Duration,
    asked: Cell<Duration>,
    pauses: Cell<u64>,
    fire: &'a dyn Fn(Duration),
}

impl<'a> Guard<'a> {
    /// Waits through `clock` until `budget` is spent, then calls `fire` with what was asked
    /// (it does not return in the binary: it kills the process).
    #[must_use]
    pub fn new(clock: &'a dyn Clock, budget: Duration, fire: &'a dyn Fn(Duration)) -> Self {
        Self {
            clock,
            budget,
            asked: Cell::new(Duration::ZERO),
            pauses: Cell::new(0),
            fire,
        }
    }

    /// The bound of `step` in the declaration.
    #[must_use]
    pub fn budget_of(step: Step) -> Duration {
        Duration::from_secs(timeout_secs(step))
    }

    /// The waits asked for so far.
    #[must_use]
    pub fn asked(&self) -> Duration {
        self.asked.get()
    }
}

impl Clock for Guard<'_> {
    fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Every pause counts toward the bound, and so does every call: a step that asks more
    /// pauses than its bound has seconds (zero-length ones, in a loop) is stopped as well.
    fn pause(&self, span: Duration) {
        watchdog::progress();
        let asked = self.asked.get().saturating_add(span);
        self.asked.set(asked);
        let pauses = self.pauses.get() + 1;
        self.pauses.set(pauses);
        if asked > self.budget || pauses > self.budget.as_secs() {
            (self.fire)(asked);
            return;
        }
        self.clock.pause(span);
    }
}

/// A clock that stands still: a pause counts nothing and moves no time ([`Defect::Hang`]).
pub struct Frozen<'a>(pub &'a dyn Clock);

impl Clock for Frozen<'_> {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }

    fn pause(&self, _: Duration) {}
}

/// The staged agent's `--version`: the version the updater installs, on the crun runtime, or
/// (a `prepare` fault) another version; and the unit manager's `systemctl show`, which reports
/// a stopped unit whose cgroup is empty.
pub struct StagedAgent {
    /// What the first line names.
    pub version: String,
}

impl CommandRunner for StagedAgent {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        match args {
            ["--version"] => Ok(format!("js-agent {}\nruntimes: crun\n", self.version)),
            ["show", "--property=TasksCurrent", "--value", _] => Ok("0\n".to_owned()),
            _ => Err(format!(
                "{program}: the fake runs --version and systemctl show only, not {args:?}"
            )),
        }
    }

    fn run_in(&self, program: &str, _: &[&str], _: &[(&str, &str)]) -> Result<String, String> {
        Err(format!("{program}: run_in is not used by the hook"))
    }
}

/// The world's ports as one step sees them, with the fault arranged for it and the defects the
/// binary adds.
pub struct Faulted<'a> {
    /// The fakes' world.
    pub world: &'a World,
    /// The same world without kill points: a start and the hello of the agent it starts are
    /// one change, behind one kill point. The agent says hello on its own once its unit runs;
    /// a hook killed after the start does not take the hello with it.
    pub quiet: &'a World,
    /// The step that runs.
    pub step: Step,
    /// The fault arranged for it.
    pub fault: Option<Fault>,
    /// The new agent never says hello ([`Defect::Hang`]).
    pub silent_start: bool,
    /// `verify`'s first look at the unit never returns ([`Defect::Stall`]).
    pub stall: bool,
    /// `verify`'s first look at the unit reaches kill points for ever ([`Defect::PointLoop`]).
    pub point_loop: bool,
    /// `verify`'s first look at the unit never returns, unannounced ([`Defect::SilentStall`]).
    pub silent_stall: bool,
    /// An uncordon changes nothing and says nothing was there ([`Defect::ForgetsCordon`]).
    pub keeps_cordon: bool,
    /// The fakes' own files, where [`DRAIN_LOOKS`] is kept.
    pub control: &'a Path,
}

/// The file in the fakes' own files that holds how many more readings of the host during
/// `prepare` still show a lease: a host whose last job ends while `prepare` waits, so that
/// `prepare` polls, pauses and renews its cordon before the host is drained. Absent: none.
pub const DRAIN_LOOKS: &str = "drain-looks";

impl Faulted<'_> {
    fn is(&self, step: Step, fault: Fault) -> bool {
        self.step == step && self.fault == Some(fault)
    }

    fn lease_down(&self) -> bool {
        self.is(Step::Cleanup, Fault::Fail) || self.is(Step::Rollback, Fault::Fail)
    }

    fn busy(&self) -> bool {
        self.fault == Some(Fault::Defer)
    }

    fn check(&self) -> Result<(), LeaseError> {
        if self.lease_down() {
            Err(LeaseError::Unreachable(format!(
                "the lease server does not answer during {}",
                self.step.name()
            )))
        } else {
            Ok(())
        }
    }

    /// Whether a reading during `prepare` still shows the job that is draining, counting the
    /// reading.
    fn draining(&self) -> bool {
        if self.step != Step::Prepare {
            return false;
        }
        let file = self.control.join(DRAIN_LOOKS);
        let left: u64 = fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        if left == 0 {
            return false;
        }
        fs::write(&file, (left - 1).to_string()).unwrap();
        true
    }

    /// A reading with a lease on the host while the step is to be deferred, or while a job
    /// drains.
    fn with_lease(&self, mut host: HostReading) -> HostReading {
        if self.busy() || self.draining() {
            host.leases = 1;
            host.lease = Some("lease-1".to_owned());
        }
        host
    }
}

impl Units for Faulted<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
        if self.stall && self.step == Step::Verify {
            watchdog::announce_stall();
            loop {
                std::thread::park();
            }
        }
        if self.silent_stall && self.step == Step::Verify {
            loop {
                std::thread::park();
            }
        }
        if self.point_loop && self.step == Step::Verify {
            loop {
                self.world.point();
            }
        }
        if self.is(Step::Verify, Fault::Fail) {
            // The new agent ran and died after the start.
            return Ok(UnitState {
                active: "failed".to_owned(),
                sub: "failed".to_owned(),
            });
        }
        self.world.show(unit)
    }

    fn stop_no_block(&self, unit: &str) -> Result<(), PortError> {
        self.world.stop_no_block(unit)
    }

    fn start(&self, unit: &str) -> Result<(), PortError> {
        if self.world.show(unit)?.active == "active" {
            return Ok(());
        }
        if self.is(Step::Cutover, Fault::Fail) {
            return self.world.crash(unit);
        }
        self.world.point();
        if self.silent_start {
            return self.quiet.start_silent(unit);
        }
        // The unit runs, and the agent's hello is on its way: a kill here leaves it pending,
        // and it arrives before the next step runs (`World::deliver_hello`).
        self.quiet.start_pending(unit)?;
        self.world.point();
        self.quiet.deliver_hello().map(drop)
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.show(unit).map(|state| state.active == "active")
    }
}

impl Link for Faulted<'_> {
    fn target(&self, link: &Path) -> Result<Option<PathBuf>, PortError> {
        self.world.resolved_target(link)
    }

    /// The updater moves the release link; the hook never does.
    fn relink(&self, link: &Path, target: &Path) -> Result<(), PortError> {
        panic!(
            "the hook moved the release link {} to {}",
            link.display(),
            target.display()
        );
    }
}

impl Lease for Faulted<'_> {
    fn cordon(&self, lease_secs: u64) -> Result<Cordoned, LeaseError> {
        self.check()?;
        let mut cordoned = self.world.cordon(lease_secs)?;
        cordoned.host = self.with_lease(cordoned.host);
        Ok(cordoned)
    }

    fn uncordon(&self) -> Result<Uncordoned, LeaseError> {
        self.check()?;
        if self.keeps_cordon {
            return Ok(Uncordoned {
                lifted: false,
                host: self.world.host()?,
            });
        }
        let mut uncordoned = self.world.uncordon()?;
        uncordoned.host = self.with_lease(uncordoned.host);
        Ok(uncordoned)
    }

    fn host(&self) -> Result<HostReading, LeaseError> {
        self.check()?;
        self.world.host().map(|host| self.with_lease(host))
    }
}

/// The agent's state root as the step sees it: a taint while the step is to halt.
pub struct FaultedState<'a> {
    /// The state root in the world.
    pub root: &'a dyn StateView,
    /// The step is to halt.
    pub tainted: bool,
}

impl StateView for FaultedState<'_> {
    fn taint(&self) -> Option<Taint> {
        if self.tainted {
            return Some(Taint {
                reason: "fault".to_owned(),
                run_id: None,
                check: None,
            });
        }
        self.root.taint()
    }

    fn owed(&self) -> Result<Vec<String>, PortError> {
        self.root.owed()
    }

    fn has_credential(&self) -> bool {
        self.root.has_credential()
    }
}
