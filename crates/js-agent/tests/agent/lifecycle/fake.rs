//! A container runtime that lives in memory, and a clock a test moves. Nothing here
//! starts a process or waits.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{
    CheckFinding, CheckHook, Clock, Exit, Finished, Found, RunHandle, RunSpec, Runtime,
    RuntimeError, Status, label,
};
use js_agent_wire::LogTail;

/// A clock that moves only when a test says so. Clones share one reading.
#[derive(Clone, Default)]
pub struct ManualClock(Rc<Cell<u64>>);

impl ManualClock {
    pub fn advance(&self, ms: u64) {
        self.0.set(self.0.get() + ms);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

/// Says whether a scratch directory has its quota.
type QuotaWatch = Box<dyn Fn(&Path) -> bool>;

/// The calls the tests compare in order, shared by the runtime and the check hook.
pub type Log = Rc<RefCell<Vec<String>>>;

struct Container {
    run_id: String,
    scratch: PathBuf,
    labels: BTreeMap<String, String>,
    running_ms: u64,
    exit: Option<Exit>,
}

#[derive(Default)]
struct State {
    containers: BTreeMap<String, Container>,
    next: u32,
    /// Calls to fail: name of the call, how many more times it fails.
    failing: BTreeMap<&'static str, u32>,
    /// Whether a stop makes the container exit (with 137).
    stop_exits: bool,
    /// Milliseconds a `create_and_start` takes on the test's clock.
    create_takes_ms: u64,
    stops: Vec<(String, Duration)>,
    creates: Vec<RunSpec>,
    /// When set, the log notes whether this file exists at each call.
    marker: Option<PathBuf>,
    /// Whether the next create fails as a start that shows the host is not seen.
    blind: bool,
    /// The log tail `log_tail` answers for the container of a run.
    tails: BTreeMap<String, LogTail>,
    /// The control group directory of the container of each run.
    cgroups: BTreeMap<String, PathBuf>,
    /// How many times the scripted checks were told the host is idle.
    idles: u32,
    /// When set, the log notes at every create whether this says the scratch directory has
    /// its quota (see [`FakeRuntime::watch_quota`]).
    quota_applied: Option<QuotaWatch>,
}

/// A runtime whose containers exit when a test says so, whose calls fail when a test
/// says so, and which records the order of its calls in the shared [`Log`].
#[derive(Clone)]
pub struct FakeRuntime {
    state: Rc<RefCell<State>>,
    log: Log,
    clock: ManualClock,
}

fn err(call: &str) -> RuntimeError {
    RuntimeError::new(format!("{call} failed"))
}

impl FakeRuntime {
    pub fn new(log: &Log, clock: &ManualClock) -> Self {
        let state = State {
            stop_exits: true,
            ..State::default()
        };
        Self {
            state: Rc::new(RefCell::new(state)),
            log: Rc::clone(log),
            clock: clock.clone(),
        }
    }

    /// The container of `run_id` has printed `tail`. The log notes `logs <id>` when it is
    /// read, for a run with a tail or when the read fails and for no other run, so the logs
    /// of tests that do not care are unchanged.
    pub fn print(&self, run_id: &str, tail: LogTail) {
        self.state
            .borrow_mut()
            .tails
            .insert(run_id.to_owned(), tail);
    }

    /// Makes the next `times` calls named `call` (`create`, `poll`, `stop`, `remove`, `logs` or
    /// `list`) fail; `u32::MAX` for as long as the test wants.
    pub fn fail(&self, call: &'static str, times: u32) {
        self.state.borrow_mut().failing.insert(call, times);
    }

    /// How many times the scripted checks were told the host is idle (`CheckHook::idle`).
    pub fn idles(&self) -> u32 {
        self.state.borrow().idles
    }

    /// The next `create_and_start` fails as a start does when the agent cannot see the host
    /// its containers run on (the container is not made).
    pub fn blind_next_create(&self) {
        self.state.borrow_mut().blind = true;
    }

    /// Whether a stop makes the container exit. On by default.
    pub fn stop_exits(&self, on: bool) {
        self.state.borrow_mut().stop_exits = on;
    }

    /// From now on the log notes, on every create, remove and check, whether `marker`
    /// exists at that moment.
    pub fn watch_marker(&self, marker: PathBuf) {
        self.state.borrow_mut().marker = Some(marker);
    }

    /// From now on the log notes, on every create, whether `applied` says the container's
    /// scratch directory already has its quota: the real runtime gives that directory to the
    /// slot's user in the create, so a quota set later would come after the chown.
    pub fn watch_quota(&self, applied: impl Fn(&Path) -> bool + 'static) {
        self.state.borrow_mut().quota_applied = Some(Box::new(applied));
    }

    /// The note for the log: nothing when no marker is watched.
    fn marker_note(&self) -> String {
        match &self.state.borrow().marker {
            None => String::new(),
            Some(path) if path.exists() => " [marker present]".to_owned(),
            Some(_) => " [no marker]".to_owned(),
        }
    }

    /// Every `create_and_start` takes `ms` on the test's clock.
    pub fn create_takes(&self, ms: u64) {
        self.state.borrow_mut().create_takes_ms = ms;
    }

    /// The runtime reports `dir` as the control group of the container of `run_id`.
    pub fn set_cgroup(&self, run_id: &str, dir: &str) {
        let dir = PathBuf::from(dir);
        self.state
            .borrow_mut()
            .cgroups
            .insert(run_id.to_owned(), dir);
    }

    /// The container of `run_id` exits with `code`.
    pub fn exit(&self, run_id: &str, code: i32) {
        self.set_exit(
            run_id,
            Exit {
                code,
                oom_killed: false,
            },
        );
    }

    /// The container of `run_id` is killed by the memory controller.
    pub fn exit_oom(&self, run_id: &str) {
        self.set_exit(
            run_id,
            Exit {
                code: 137,
                oom_killed: true,
            },
        );
    }

    fn set_exit(&self, run_id: &str, exit: Exit) {
        let mut state = self.state.borrow_mut();
        let found = state.containers.values_mut().find(|c| c.run_id == run_id);
        found.expect("no such container").exit = Some(exit);
    }

    /// The container of `run_id` disappears while it runs, as if removed from outside.
    pub fn vanish(&self, run_id: &str) {
        self.state
            .borrow_mut()
            .containers
            .retain(|_, c| c.run_id != run_id);
    }

    /// Adds a container that was there before the agent, with exactly these labels.
    pub fn leak(&self, labels: &[(&str, &str)], running_ms: u64) -> String {
        let mut state = self.state.borrow_mut();
        state.next += 1;
        let id = format!("leaked-{}", state.next);
        let labels: BTreeMap<String, String> = labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        let run_id = labels.get(label::RUN).cloned().unwrap_or_default();
        state.containers.insert(
            id.clone(),
            Container {
                run_id,
                scratch: PathBuf::new(),
                labels,
                running_ms,
                exit: None,
            },
        );
        id
    }

    /// The ids of the containers that exist.
    pub fn existing(&self) -> Vec<String> {
        self.state.borrow().containers.keys().cloned().collect()
    }

    /// Every container created, in order.
    pub fn created(&self) -> Vec<RunSpec> {
        self.state.borrow().creates.clone()
    }

    /// How long each stop gave the container, by container id.
    pub fn stops(&self) -> Vec<(String, Duration)> {
        self.state.borrow().stops.clone()
    }

    /// Whether the container of `run_id` is gone.
    pub fn gone(&self, run_id: &str) -> bool {
        let state = self.state.borrow();
        state.containers.values().all(|c| c.run_id != run_id)
    }

    fn fails(&self, call: &'static str) -> bool {
        let mut state = self.state.borrow_mut();
        match state.failing.get_mut(call) {
            Some(left) if *left > 0 => {
                *left -= 1;
                true
            }
            _ => false,
        }
    }
}

impl Runtime for FakeRuntime {
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let quota = match &self.state.borrow().quota_applied {
            None => "",
            Some(applied) if applied(&spec.scratch) => " [quota set]",
            Some(_) => " [no quota]",
        };
        self.log.borrow_mut().push(format!(
            "create {}{}{quota}",
            spec.run_id,
            self.marker_note()
        ));
        self.clock.advance(self.state.borrow().create_takes_ms);
        if self.fails("create") {
            return Err(err("create"));
        }
        if std::mem::take(&mut self.state.borrow_mut().blind) {
            return Err(RuntimeError::blind(
                "create: the agent does not see the host",
            ));
        }
        let mut state = self.state.borrow_mut();
        state.next += 1;
        let id = format!("c{}", state.next);
        state.creates.push(spec.clone());
        state.containers.insert(
            id.clone(),
            Container {
                run_id: spec.run_id.clone(),
                scratch: spec.scratch.clone(),
                labels: spec.labels.clone(),
                running_ms: 0,
                exit: None,
            },
        );
        Ok(RunHandle(id))
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        if self.fails("poll") {
            return Err(err("poll"));
        }
        let state = self.state.borrow();
        Ok(match state.containers.get(&id.0) {
            None => Status::Gone,
            Some(c) => c.exit.map_or(Status::Running, Status::Exited),
        })
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.log.borrow_mut().push(format!("stop {}", id.0));
        if self.fails("stop") {
            return Err(err("stop"));
        }
        let mut state = self.state.borrow_mut();
        state.stops.push((id.0.clone(), grace));
        let exits = state.stop_exits;
        let container = state.containers.get_mut(&id.0);
        container.into_iter().for_each(|c| {
            if exits && c.exit.is_none() {
                c.exit = Some(Exit {
                    code: 137,
                    oom_killed: false,
                });
            }
        });
        Ok(())
    }

    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        let tail = {
            let state = self.state.borrow();
            let run = state.containers.get(&id.0).map(|c| c.run_id.clone());
            run.and_then(|run| state.tails.get(&run).cloned())
        };
        let failing = self.fails("logs");
        if tail.is_some() || failing {
            self.log.borrow_mut().push(format!("logs {}", id.0));
        }
        match failing {
            true => Err(err("logs")),
            false => Ok(tail),
        }
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        let scratch = self
            .state
            .borrow()
            .containers
            .get(&id.0)
            .map(|c| c.scratch.exists());
        let seen = match scratch {
            Some(true) => "scratch present",
            _ => "scratch absent",
        };
        self.log
            .borrow_mut()
            .push(format!("remove {} ({seen}){}", id.0, self.marker_note()));
        if self.fails("remove") {
            return Err(err("remove"));
        }
        self.state.borrow_mut().containers.remove(&id.0);
        Ok(())
    }

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        let state = self.state.borrow();
        let run = &state.containers.get(&id.0)?.run_id;
        state.cgroups.get(run).cloned()
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        if self.fails("list") {
            return Err(err("list"));
        }
        let state = self.state.borrow();
        Ok(state
            .containers
            .iter()
            .map(|(id, c)| Found {
                id: RunHandle(id.clone()),
                labels: c.labels.clone(),
                running_ms: c.running_ms,
                never_started: false,
            })
            .collect())
    }
}

/// Post-job checks that pass until a test makes one find something, and record when
/// they ran and what the host looked like then.
pub struct ScriptedChecks {
    log: Log,
    runtime: FakeRuntime,
    scratch_root: PathBuf,
    findings: BTreeMap<String, CheckFinding>,
}

impl ScriptedChecks {
    pub fn new(log: &Log, runtime: &FakeRuntime, scratch_root: &Path) -> Self {
        Self {
            log: Rc::clone(log),
            runtime: runtime.clone(),
            scratch_root: scratch_root.to_path_buf(),
            findings: BTreeMap::new(),
        }
    }

    pub fn find(mut self, run_id: &str, finding: CheckFinding) -> Self {
        self.findings.insert(run_id.to_owned(), finding);
        self
    }
}

impl CheckHook for ScriptedChecks {
    fn idle(&mut self) {
        self.runtime.state.borrow_mut().idles += 1;
    }

    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        let run_id = job.run_id;
        let container = match self.runtime.gone(run_id) {
            true => "container gone",
            false => "container present",
        };
        let scratch = match self.scratch_root.join(run_id).exists() {
            true => "scratch present",
            false => "scratch gone",
        };
        self.log.borrow_mut().push(format!(
            "check {run_id} ({container}, {scratch}){}",
            self.runtime.marker_note()
        ));
        if !job.live.is_empty() {
            let live: Vec<&str> = job.live.iter().map(|id| id.0.as_str()).collect();
            self.log
                .borrow_mut()
                .push(format!("live beside {run_id}: {}", live.join(",")));
        }
        self.findings
            .get(run_id)
            .map_or(Ok(()), |finding| Err(finding.clone()))
    }
}
