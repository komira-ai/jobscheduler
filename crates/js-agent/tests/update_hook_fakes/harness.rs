//! The fake host as the conformance harness of the hook contract runs it (`update_hook::
//! conformance`, a dev-dependency): what `js-update-hook-fake` reads from its environment, the
//! declaration and params it runs the steps under, the kill points spoken in the harness's
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
    Clock, Cordoned, Declaration, Gate, HostReading, Lease, LeaseError, Link, PortError, StateView,
    Step, Uncordoned, UnitState, Units,
};
use js_agent_wire::Taint;

use super::World;

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

/// The unit the declaration names.
pub const UNIT: &str = "js-agent-crun.service";
/// The release link the params name. The fake keeps it in the world (`link/current`); nothing
/// at this path is touched.
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

/// The declaration the hook runs under, with its record in `state_dir`.
#[must_use]
pub fn declaration_text(state_dir: &Path) -> String {
    let bound = timeout_secs;
    format!(
        concat!(
            "state_dir = \"{state}\"\n[hook]\nunits = [\"{unit}\"]\n",
            "write_paths = [\"/opt/js-agent\", \"{state}\"]\n",
            "[hook.timeout_secs]\nprepare = {prepare}\ncutover = {cutover}\nverify = {verify}\n",
            "rollback = {rollback}\ncleanup = {cleanup}\n"
        ),
        state = state_dir.display(),
        unit = UNIT,
        prepare = bound(Step::Prepare),
        cutover = bound(Step::Cutover),
        verify = bound(Step::Verify),
        rollback = bound(Step::Rollback),
        cleanup = bound(Step::Cleanup),
    )
}

/// The declaration's own `[hook.params]` (the harness adds the `updater.*` keys).
pub const PARAMS: [(&str, ParamValue); 5] = [
    ("cordon_lease_secs", ParamValue::Integer(300)),
    ("cordon_max_secs", ParamValue::Integer(17_355)),
    ("drain_wait_secs", ParamValue::Integer(3600)),
    ("defer_retry_secs", ParamValue::Integer(300)),
    ("release_link", ParamValue::Text(RELEASE_LINK)),
];

/// A value of [`PARAMS`].
#[derive(Clone, Copy, Debug)]
pub enum ParamValue {
    /// A whole number.
    Integer(i64),
    /// Text.
    Text(&'static str),
}

/// The kill points, in the harness's protocol: each point appends a byte to the count file in
/// the directory [`GATE_DIR_VAR`] names, and the point whose number is [`KILL_AT_VAR`]'s writes
/// [`PARKED`] to standard error and parks until the harness kills it. Without the directory a
/// point does nothing.
pub struct HarnessGate {
    dir: Option<PathBuf>,
    kill_at: u64,
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

    /// Counts one point and returns its number; `None` outside the harness or when the count
    /// cannot be kept (the harness then sees fewer points and says so).
    #[must_use]
    pub fn count(&self) -> Option<u64> {
        let dir = self.dir.as_ref()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(COUNT_FILE))
            .ok()?;
        file.write_all(b".").ok()?;
        file.metadata().ok().map(|meta| meta.len())
    }
}

impl Gate for HarnessGate {
    fn point(&self) {
        if self.count() == Some(self.kill_at) {
            eprintln!("{PARKED}");
            // The harness kills the process group on the line; a wake-up parks again.
            loop {
                std::thread::park();
            }
        }
    }
}

/// How a test asks a step to end: what the [`Faulted`] ports make go wrong while that step
/// runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The step fails (1): the staged agent is another version (`prepare`), the agent crashes
    /// on its start (`cutover`) or after it (`verify`), the lease server does not answer
    /// (`cleanup`, `rollback`).
    Fail,
    /// The host is busy (75): it holds a lease that never goes (`prepare`, `cutover`).
    Defer,
    /// No version can repair the host (76): the agent's state root holds a taint (`prepare`,
    /// `cutover`, `verify`).
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

    /// Whether the steps of the hook can be made to end so at `step`: `verify`, `cleanup` and
    /// `rollback` never defer (the contract gives them none), and `cleanup` and `rollback`
    /// have no check that halts.
    #[must_use]
    pub const fn reachable(self, step: Step) -> bool {
        match (step, self) {
            (Step::Prepare | Step::Cutover, _) | (Step::Verify, Fault::Fail | Fault::Halt) => true,
            (Step::Cleanup | Step::Rollback, Fault::Fail) => true,
            (Step::Verify | Step::Cleanup | Step::Rollback, _) => false,
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
}

impl Defect {
    /// The value of [`DEFECT_VAR`].
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Defect::NoLastLine => "no-last-line",
            Defect::CutoverRestarts => "cutover-restarts",
            Defect::Hang => "hang",
        }
    }

    /// The defect a value names; `None` for none or a word no defect has.
    #[must_use]
    pub fn from_word(word: Option<&OsString>) -> Option<Defect> {
        let word = word?.to_str()?;
        [Defect::NoLastLine, Defect::CutoverRestarts, Defect::Hang]
            .into_iter()
            .find(|defect| defect.word() == word)
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

    fn pause(&self, span: Duration) {
        let asked = self.asked.get().saturating_add(span);
        self.asked.set(asked);
        if asked > self.budget {
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
/// (a `prepare` fault) another version.
pub struct StagedAgent {
    /// What the first line names.
    pub version: String,
}

impl CommandRunner for StagedAgent {
    fn run(&self, _program: &str, args: &[&str]) -> Result<String, String> {
        match args {
            ["--version"] => Ok(format!("js-agent {}\nruntimes: crun\n", self.version)),
            _ => Err(format!(
                "the staged agent takes --version only, not {args:?}"
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
}

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

    /// A reading with a lease on the host while the step is to be deferred.
    fn with_lease(&self, mut host: HostReading) -> HostReading {
        if self.busy() {
            host.leases = 1;
            host.lease = Some("lease-1".to_owned());
        }
        host
    }
}

impl Units for Faulted<'_> {
    fn show(&self, unit: &str) -> Result<UnitState, PortError> {
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
        self.quiet.start(unit)
    }

    fn is_active(&self, unit: &str) -> Result<bool, PortError> {
        self.show(unit).map(|state| state.active == "active")
    }
}

impl Link for Faulted<'_> {
    fn target(&self, link: &Path) -> Result<Option<PathBuf>, PortError> {
        self.world.resolved_target(link)
    }

    fn relink(&self, link: &Path, target: &Path) -> Result<(), PortError> {
        self.world.relink_relative(link, target)
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
}

/// Declaration of the hook over the state directory `state_dir`.
///
/// # Errors
/// The text does not parse (a path the declaration refuses).
pub fn declaration(state_dir: &Path) -> Result<Declaration, String> {
    Declaration::parse(&declaration_text(state_dir)).map_err(|why| why.to_string())
}
