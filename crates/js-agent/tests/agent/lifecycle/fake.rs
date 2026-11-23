//! A container runtime that lives in memory, and a clock a test moves. Nothing here
//! starts a process or waits.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{
    Exit, Found, FsPrepared, JobFs, NativeRefusal, Prepared, RunHandle, RunSpec, Runtime,
    RuntimeError, Status, Ticket, Wake, Whereabouts, label,
};
use js_agent_wire::{
    CpuSeries, HostPort, JobUsage, LogReport, LogTail, MemorySeries, NativeSpec, NetworkCeiling,
    PortRange,
};

#[path = "fake_checks.rs"]
mod checks;
#[path = "fake_clock.rs"]
mod clock;
#[path = "fake_fs.rs"]
mod fs;
pub use checks::ScriptedChecks;
pub use clock::{ManualClock, NoUnixClock, UNIX_ORIGIN_MS};

/// The agent label of every container the fake makes.
const AGENT: &str = "fake-host";

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
    /// Whether it was started: a staged one (see [`FakeRuntime::stage`]) was only created.
    started: bool,
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
    /// What `places_in_cgroup` answers.
    places: bool,
    /// The log tail `log_tail` answers for the container of a run.
    tails: BTreeMap<String, LogTail>,
    /// What `logs_report` answers for a run, by run id.
    reports: BTreeMap<String, LogReport>,
    /// What `usage` answers for a run, by run id.
    usages: BTreeMap<String, JobUsage>,
    /// What `cpu_series` answers for a run, by run id.
    series: BTreeMap<String, CpuSeries>,
    /// What `memory_series` answers for a run, by run id.
    memory_series: BTreeMap<String, MemorySeries>,
    /// The control group directory of the container of each run.
    cgroups: BTreeMap<String, PathBuf>,
    /// The control group the first process of the container of a run runs in, outside the
    /// group made for it (see [`FakeRuntime::run_elsewhere`]).
    elsewhere: BTreeMap<String, String>,
    /// How many times `whereabouts` was asked about each run, by run id.
    where_asks: BTreeMap<String, u32>,
    /// The group `whereabouts` names for a run (and `cgroup` from then on) though `cgroup`
    /// named none before it was asked (see [`FakeRuntime::group_when_asked`]).
    grouped_when_asked: BTreeMap<String, PathBuf>,
    /// How many times the scripted checks were told the host is idle.
    idles: u32,
    /// When set, the log notes at every create whether this says the scratch directory has
    /// its quota (see [`FakeRuntime::watch_quota`]).
    quota_applied: Option<QuotaWatch>,
    /// The images whose pull is scripted: absent ones are here.
    pulls: BTreeMap<String, Pull>,
    /// The tickets `prepare` was asked with for each scripted image, in order.
    asks: BTreeMap<String, Vec<Option<Ticket>>>,
    /// The ticket the next scripted pull is given.
    generation: u64,
    /// Whether the runtime cannot use a GPU (`supports_gpus` says so).
    gpus_off: bool,
    /// Whether the runtime carries no job start's environment (`check_env` refuses one).
    env_off: bool,
    /// The exit code of a container that handles SIGTERM, by run id: a stop ends it with
    /// that code instead of the kill's 137.
    term_exits: BTreeMap<String, i32>,
    /// The directory the runtime keeps each container's own directory in (see
    /// [`FakeRuntime::keep_lease_dirs_in`]).
    lease_root: Option<PathBuf>,
    /// The directory it keeps each job's temporary directory in (see
    /// [`FakeRuntime::keep_tmp_dirs_in`]).
    tmp_root: Option<PathBuf>,
    /// Whether the directory it names has no name to make it by.
    lease_unnamable: bool,
    /// What `network_ceiling` answers; `none` by default, as the trait's default.
    ceiling: NetworkCeiling,
    /// How many times `network_ceiling` was asked.
    ceiling_asks: u32,
    /// How many times `check_egress` was asked.
    egress_asks: u32,
    /// What `start_steps` answers, once, by run id.
    steps: BTreeMap<String, Vec<(&'static str, u64)>>,
    /// What the runtime was given to ring (`set_wake`).
    wake: Wake,
    /// What `host_ports` answers; none by default, as the trait's default.
    host_ports: Vec<PortRange>,
    /// Why each run named here lost its host ports (`host_port_lost`), by run id.
    lost: BTreeMap<String, String>,
    /// The runs whose end the runtime holds once they exit ([`FakeRuntime::hold_end`]).
    held_ends: Vec<String>,
    /// How the runtime answers a native body: `None` runs none, as the trait's default.
    native: Option<NativeScript>,
    /// What `check_image` refuses a start named by its image with; none: it runs them, as the
    /// trait's default.
    image_refusal: Option<(&'static str, String)>,
    /// What a job's file system answers (see [`FakeRuntime::give_layers`]); none: the
    /// runtime gives no layers, as the trait's default, and says nothing of them.
    fs: Option<FsScript>,
    /// The file system each run's lookups were last asked with, by run id.
    looked_up: BTreeMap<String, JobFs>,
}

/// What the fake answers of a native body.
#[derive(Default)]
struct NativeScript {
    /// Whether a start named by its image with a `run` is run as a native body
    /// (`runs_image_runs`).
    image_runs: bool,
    /// What `check_native` refuses with; none: it accepts the body.
    refusal: Option<(&'static str, String)>,
}

/// What the fake answers of a job's file system.
#[derive(Default)]
struct FsScript {
    /// What `resolve_fs` answers for a run; ready when not named.
    resolved: BTreeMap<String, FsPrepared>,
    /// What `assemble_fs` answers for a run; ready when not named.
    assembled: BTreeMap<String, FsPrepared>,
    /// The images of a layer `check_layer_image` refuses.
    refused_images: Vec<String>,
    /// What `check_job_fs` refuses with; none: it accepts.
    job_fs_refusal: Option<(&'static str, String)>,
    /// What `check_job_fs` was asked: the layers, mounts and host ports of each call.
    job_fs_checks: Vec<(usize, usize, usize)>,
    /// The runs whose sources did not answer after they ended (`input_failure`).
    unreadable: Vec<String>,
    /// The directory the runs' mounts are under (`mounts_of`).
    mounts: Option<PathBuf>,
}

/// Where a scripted pull stands.
enum Pull {
    Pending(Ticket),
    Failed(String),
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

    /// From now on the runtime runs native bodies (`check_native` accepts them), as the Mac
    /// runtime does; its network check for one stays the trait's default.
    pub fn run_native(&self) {
        self.state.borrow_mut().native = Some(NativeScript::default());
    }

    /// From now on the runtime also runs a start named by its image with a `run` as a native
    /// body (`runs_image_runs`), and runs native bodies.
    pub fn run_image_runs(&self) {
        self.state.borrow_mut().native = Some(NativeScript {
            image_runs: true,
            refusal: None,
        });
    }

    /// From now on `check_image` refuses every start named by its image with `code` and `why`,
    /// as the Mac runtime does without a bundle store.
    pub fn refuse_images(&self, code: &'static str, why: &str) {
        self.state.borrow_mut().image_refusal = Some((code, why.to_owned()));
    }

    /// From now on `check_native` refuses every body with `code` and `why`; the runtime runs
    /// native bodies otherwise ([`FakeRuntime::run_native`]).
    pub fn refuse_native(&self, code: &'static str, why: &str) {
        let script = NativeScript {
            image_runs: false,
            refusal: Some((code, why.to_owned())),
        };
        self.state.borrow_mut().native = Some(script);
    }

    /// Once `run_id` exits (or now, if `held` is false, no more) the runtime holds its end:
    /// `poll` answers running and `ends_held` says so.
    pub fn hold_end(&self, run_id: &str, held: bool) {
        let mut state = self.state.borrow_mut();
        state.held_ends.retain(|run| run != run_id);
        if held {
            state.held_ends.push(run_id.to_owned());
        }
    }

    /// The same runtime, which cannot use a GPU: `supports_gpus` is false.
    #[must_use]
    pub fn without_gpus(self) -> Self {
        self.state.borrow_mut().gpus_off = true;
        self
    }

    /// From now on the runtime carries no job start's environment, as the Mac runtime: its
    /// `check_env` answers a non-empty one `env-unsupported`.
    pub fn refuse_env(&self) {
        self.state.borrow_mut().env_off = true;
    }

    /// The container of `run_id` handles SIGTERM: a stop ends it with `code` (the kill of a
    /// program that ignores the signal ends it with 137, which is what a stop does by
    /// default).
    pub fn on_term(&self, run_id: &str, code: i32) {
        self.state
            .borrow_mut()
            .term_exits
            .insert(run_id.to_owned(), code);
    }

    /// Creates the container of `spec` and does not start it, as a runtime does that is
    /// stopped between the two.
    pub fn stage(&self, spec: &RunSpec) -> RunHandle {
        let mut state = self.state.borrow_mut();
        state.next += 1;
        let id = format!("c{}", state.next);
        let mut labels = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), AGENT.to_owned());
        state.containers.insert(
            id.clone(),
            Container {
                run_id: spec.run_id.clone(),
                scratch: spec.scratch.clone(),
                labels,
                running_ms: 0,
                exit: None,
                started: false,
            },
        );
        RunHandle(id)
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

    /// The capture of `run_id` saw `report` of its streams: `logs_report` answers it.
    pub fn report_logs(&self, run_id: &str, report: LogReport) {
        self.state
            .borrow_mut()
            .reports
            .insert(run_id.to_owned(), report);
    }

    /// The control group of `run_id` counted `usage`: `usage` answers it.
    pub fn report_usage(&self, run_id: &str, usage: JobUsage) {
        self.state
            .borrow_mut()
            .usages
            .insert(run_id.to_owned(), usage);
    }

    /// The runtime recorded `series` for `run_id`: `cpu_series` answers it.
    pub fn report_cpu_series(&self, run_id: &str, series: CpuSeries) {
        let mut state = self.state.borrow_mut();
        state.series.insert(run_id.to_owned(), series);
    }

    /// The runtime recorded `series` for `run_id`: `memory_series` answers it.
    pub fn report_memory_series(&self, run_id: &str, series: MemorySeries) {
        let mut state = self.state.borrow_mut();
        state.memory_series.insert(run_id.to_owned(), series);
    }

    /// Makes the next `times` calls named `call` (`create`, `poll`, `stop`, `remove`, `logs`,
    /// `list` or `egress`, the egress check) fail; `u32::MAX` for as long as the test wants.
    pub fn fail(&self, call: &'static str, times: u32) {
        self.state.borrow_mut().failing.insert(call, times);
    }

    /// The network ceiling the runtime answers (`Runtime::network_ceiling`).
    pub fn set_ceiling(&self, ceiling: NetworkCeiling) {
        self.state.borrow_mut().ceiling = ceiling;
    }

    /// The host ports the runtime allows (`Runtime::host_ports`).
    pub fn set_host_ports(&self, allowed: &[PortRange]) {
        self.state.borrow_mut().host_ports = allowed.to_vec();
    }

    /// Makes the container of `run_id` one whose host ports were lost for `why`
    /// (`Runtime::host_port_lost`), as a runtime says of a run it adopted.
    pub fn lose_host_ports(&self, run_id: &str, why: &str) {
        let mut state = self.state.borrow_mut();
        state.lost.insert(run_id.to_owned(), why.to_owned());
    }

    /// How many times the network ceiling was asked for.
    pub fn ceiling_asks(&self) -> u32 {
        self.state.borrow().ceiling_asks
    }

    /// Rings the wake the runtime was given, as work it did off the worker would.
    pub fn ring_wake(&self) {
        let wake = self.state.borrow().wake.clone();
        wake.wake();
    }

    /// `start_steps` answers `steps` for `run_id`, once.
    pub fn steps_of(&self, run_id: &str, steps: &[(&'static str, u64)]) {
        let mut state = self.state.borrow_mut();
        state.steps.insert(run_id.to_owned(), steps.to_vec());
    }

    /// How many times the egress check was asked for.
    pub fn egress_asks(&self) -> u32 {
        self.state.borrow().egress_asks
    }

    /// Whether the runtime says it puts every container in a control group the agent can
    /// read (`Runtime::places_in_cgroup`), as the direct runner does. Off by default.
    pub fn promise_cgroups(&self, on: bool) {
        self.state.borrow_mut().places = on;
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

    /// From now the runtime keeps a directory of its own for each container in `root`
    /// (`.lease-fake-<run id>`), as the crun runtime does beside the runs' directories: the
    /// lifecycle makes it, bounds it with the run and removes it.
    pub fn keep_lease_dirs_in(&self, root: &Path) {
        self.state.borrow_mut().lease_root = Some(root.to_owned());
    }

    /// From now the runtime gives each job a temporary directory in `root`
    /// (`.tmp-fake-<run id>`), as the crun runtime does beside the runs' directories: the
    /// lifecycle makes it, bounds it in the slot's temporary project and removes it.
    pub fn keep_tmp_dirs_in(&self, root: &Path) {
        self.state.borrow_mut().tmp_root = Some(root.to_owned());
    }

    /// From now the directory the runtime names for a container is one that has no name (the
    /// parent of the scratch root's own), so that it cannot be made.
    pub fn keep_unnamable_lease_dirs(&self) {
        self.state.borrow_mut().lease_unnamable = true;
    }

    /// The note for the log: nothing when no marker is watched.
    fn marker_note(&self) -> String {
        match &self.state.borrow().marker {
            None => String::new(),
            Some(path) if path.exists() => " [marker present]".to_owned(),
            Some(_) => " [no marker]".to_owned(),
        }
    }

    /// The image is being pulled: `prepare` answers `Pending` (with a new ticket, which is
    /// returned) until [`FakeRuntime::pull_done`]. Calling it again for the image starts a
    /// new pull, with a new ticket.
    pub fn pull_pending(&self, image: &str) -> Ticket {
        let mut state = self.state.borrow_mut();
        state.generation += 1;
        let ticket = Ticket(state.generation);
        state.pulls.insert(image.to_owned(), Pull::Pending(ticket));
        ticket
    }

    /// The pull of the image ended: it is here.
    pub fn pull_done(&self, image: &str) {
        self.state.borrow_mut().pulls.remove(image);
    }

    /// The pull of the image failed: `prepare` answers `Failed(why)` (every time).
    pub fn pull_fails(&self, image: &str, why: &str) {
        self.state
            .borrow_mut()
            .pulls
            .insert(image.to_owned(), Pull::Failed(why.to_owned()));
    }

    /// The tickets the runtime was asked about a scripted image with, in order (none for the
    /// ask of a start).
    pub fn asks(&self, image: &str) -> Vec<Option<Ticket>> {
        self.state
            .borrow()
            .asks
            .get(image)
            .cloned()
            .unwrap_or_default()
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

    /// The first process of the container of `run_id` runs in the control group `group`,
    /// not in the one made for it: `whereabouts` answers `Elsewhere(group)` while `cgroup`
    /// names no group for the run. Without this, a run with no group has no process to find
    /// (`NoProcess`).
    pub fn run_elsewhere(&self, run_id: &str, group: &str) {
        self.state
            .borrow_mut()
            .elsewhere
            .insert(run_id.to_owned(), group.to_owned());
    }

    /// The group of the container of `run_id` is made when `whereabouts` is asked about it:
    /// `cgroup` names none until then, `whereabouts` answers `Grouped(dir)`, and `cgroup`
    /// names `dir` from then on.
    pub fn group_when_asked(&self, run_id: &str, dir: &str) {
        self.state
            .borrow_mut()
            .grouped_when_asked
            .insert(run_id.to_owned(), PathBuf::from(dir));
    }

    /// How many times `whereabouts` was asked about `run_id`.
    pub fn where_asks(&self, run_id: &str) -> u32 {
        self.state
            .borrow()
            .where_asks
            .get(run_id)
            .copied()
            .unwrap_or(0)
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
                started: true,
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

    /// Whether the container of `run_id` was started and has not exited: a job's one write
    /// to its store succeeds only then.
    pub fn running(&self, run_id: &str) -> bool {
        let state = self.state.borrow();
        let mut live = state.containers.values().filter(|c| c.started);
        live.any(|c| c.run_id == run_id && c.exit.is_none())
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
    fn layered_rootfs(&self) -> bool {
        self.state.borrow().fs.is_some()
    }

    fn check_layer_image(&self, image: &str) -> Result<(), String> {
        let state = self.state.borrow();
        let refused = state
            .fs
            .as_ref()
            .map(|fs| fs.refused_images.iter().any(|it| it == image));
        match refused.unwrap_or_default() {
            true => Err(format!("{image} is on no registry entry")),
            false => Ok(()),
        }
    }

    fn check_job_fs(&self, fs: &JobFs, host_ports: &[HostPort]) -> Result<(), NativeRefusal> {
        let mut state = self.state.borrow_mut();
        let Some(script) = state.fs.as_mut() else {
            return Ok(());
        };
        script
            .job_fs_checks
            .push((fs.layers.len(), fs.mounts.len(), host_ports.len()));
        let refused = script.job_fs_refusal.clone();
        refused.map_or(Ok(()), |(code, why)| Err(NativeRefusal { code, why }))
    }

    fn binds_named_mounts(&self) -> bool {
        self.state.borrow().fs.is_none()
    }

    fn resolve_fs(&mut self, run_id: &str, image: &str, fs: &JobFs, _now_ms: u64) -> FsPrepared {
        self.log.borrow_mut().push(format!(
            "resolve {run_id} ({} layers, {} mounts, image {image:?})",
            fs.layers.len(),
            fs.mounts.len()
        ));
        self.state
            .borrow_mut()
            .looked_up
            .insert(run_id.to_owned(), fs.clone());
        let answer = self.script().resolved.get(run_id).cloned();
        answer.unwrap_or(FsPrepared::Ready)
    }

    fn assemble_fs(&mut self, run_id: &str, _now_ms: u64) -> FsPrepared {
        self.log.borrow_mut().push(format!("assemble {run_id}"));
        let answer = self.script().assembled.get(run_id).cloned();
        answer.unwrap_or(FsPrepared::Ready)
    }

    fn discard_fs(&mut self, run_id: &str) -> Result<(), RuntimeError> {
        if self.state.borrow().fs.is_none() {
            return Ok(());
        }
        self.log.borrow_mut().push(format!("discard {run_id}"));
        match self.fails("discard") {
            true => Err(err("discard")),
            false => Ok(()),
        }
    }

    fn mounts_of(&self, run_id: &str) -> Option<PathBuf> {
        let state = self.state.borrow();
        Some(state.fs.as_ref()?.mounts.as_ref()?.join(run_id))
    }

    fn input_failure(&self, id: &RunHandle) -> Option<String> {
        let state = self.state.borrow();
        let run = &state.containers.get(&id.0)?.run_id;
        let gone = state.fs.as_ref()?.unreadable.contains(run);
        gone.then(|| js_agent_wire::refusal::INPUT_UNREADABLE.to_owned())
    }

    fn host_ports(&self) -> Vec<PortRange> {
        self.state.borrow().host_ports.clone()
    }

    fn host_port_lost(&self, id: &RunHandle) -> Option<String> {
        let state = self.state.borrow();
        let run = &state.containers.get(&id.0)?.run_id;
        state.lost.get(run).cloned()
    }

    fn network_ceiling(&self) -> NetworkCeiling {
        let mut state = self.state.borrow_mut();
        state.ceiling_asks += 1;
        state.ceiling.clone()
    }

    fn check_egress(&self) -> Result<(), RuntimeError> {
        self.state.borrow_mut().egress_asks += 1;
        match self.fails("egress") {
            true => Err(err("egress")),
            false => Ok(()),
        }
    }

    fn lease_dir(&self, run_id: &str) -> Option<PathBuf> {
        let state = self.state.borrow();
        if state.lease_unnamable {
            return Some(PathBuf::from("/.."));
        }
        let root = state.lease_root.clone();
        root.map(|root| root.join(format!(".lease-fake-{run_id}")))
    }

    fn tmp_dir(&self, run_id: &str) -> Option<PathBuf> {
        let root = self.state.borrow().tmp_root.clone();
        root.map(|root| root.join(format!(".tmp-fake-{run_id}")))
    }

    fn prepare(&mut self, image: &str, waited: Option<Ticket>) -> Prepared {
        let mut state = self.state.borrow_mut();
        let answer = match state.pulls.get(image) {
            None => return Prepared::Ready,
            Some(Pull::Pending(ticket)) => Prepared::Pending(*ticket),
            Some(Pull::Failed(why)) => Prepared::Failed(RuntimeError::new(why.clone())),
        };
        state.asks.entry(image.to_owned()).or_default().push(waited);
        answer
    }

    fn set_wake(&mut self, wake: Wake) {
        self.state.borrow_mut().wake = wake;
    }

    fn start_steps(&mut self, id: &RunHandle) -> Vec<(&'static str, u64)> {
        let mut state = self.state.borrow_mut();
        let run_id = state.containers.get(&id.0).map(|c| c.run_id.clone());
        let steps = run_id.and_then(|run_id| state.steps.remove(&run_id));
        steps.unwrap_or_default()
    }

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
        // Refused as a real runtime refuses them, before anything is made.
        if !spec.gpu_indices.is_empty() && state.gpus_off {
            return Err(RuntimeError::new("this runtime cannot use a GPU"));
        }
        if state.containers.values().any(|c| c.run_id == spec.run_id) {
            return Err(RuntimeError::new("a container of this run exists"));
        }
        state.next += 1;
        let id = format!("c{}", state.next);
        state.creates.push(spec.clone());
        let mut labels = spec.labels.clone();
        labels.insert(label::AGENT.to_owned(), AGENT.to_owned());
        state.containers.insert(
            id.clone(),
            Container {
                run_id: spec.run_id.clone(),
                scratch: spec.scratch.clone(),
                labels,
                running_ms: 0,
                exit: None,
                started: true,
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
            Some(c) if state.held_ends.contains(&c.run_id) => Status::Running,
            Some(c) => c.exit.map_or(Status::Running, Status::Exited),
        })
    }

    fn ends_held(&self, id: &RunHandle) -> bool {
        let state = self.state.borrow();
        let ended = state.containers.get(&id.0).filter(|c| c.exit.is_some());
        ended.is_some_and(|c| state.held_ends.contains(&c.run_id))
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.log.borrow_mut().push(format!("stop {}", id.0));
        if self.fails("stop") {
            return Err(err("stop"));
        }
        let mut state = self.state.borrow_mut();
        state.stops.push((id.0.clone(), grace));
        let exits = state.stop_exits;
        let term = state
            .containers
            .get(&id.0)
            .and_then(|c| state.term_exits.get(&c.run_id))
            .copied()
            .unwrap_or(137);
        let container = state.containers.get_mut(&id.0);
        container.into_iter().for_each(|c| {
            if exits && c.exit.is_none() {
                c.exit = Some(Exit {
                    code: term,
                    oom_killed: false,
                });
            }
        });
        Ok(())
    }

    fn logs_report(&mut self, id: &RunHandle) -> Option<LogReport> {
        let state = self.state.borrow();
        let run = state.containers.get(&id.0).map(|c| c.run_id.clone());
        run.and_then(|run| state.reports.get(&run).copied())
    }

    fn usage(&mut self, id: &RunHandle) -> Option<JobUsage> {
        let state = self.state.borrow();
        let run = state.containers.get(&id.0).map(|c| c.run_id.clone());
        run.and_then(|run| state.usages.get(&run).copied())
    }

    fn cpu_series(&mut self, id: &RunHandle) -> Option<CpuSeries> {
        let state = self.state.borrow();
        state
            .series
            .get(&state.containers.get(&id.0)?.run_id)
            .cloned()
    }

    fn memory_series(&mut self, id: &RunHandle) -> Option<MemorySeries> {
        let state = self.state.borrow();
        let run = &state.containers.get(&id.0)?.run_id;
        state.memory_series.get(run).cloned()
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

    fn places_in_cgroup(&self) -> bool {
        self.state.borrow().places
    }

    fn check_env(&self, env: &[(String, String)]) -> Result<(), NativeRefusal> {
        match env.is_empty() || !self.state.borrow().env_off {
            true => Ok(()),
            false => Err(NativeRefusal {
                code: js_agent_wire::refusal::ENV_UNSUPPORTED,
                why: "this runtime carries no environment".to_owned(),
            }),
        }
    }

    fn check_native(&self, _native: &NativeSpec) -> Result<(), NativeRefusal> {
        match &self.state.borrow().native {
            None => Err(NativeRefusal {
                code: js_agent_wire::refusal::NATIVE_UNSUPPORTED,
                why: "this agent runs no native jobs".to_owned(),
            }),
            Some(script) => match &script.refusal {
                Some((code, why)) => Err(NativeRefusal {
                    code,
                    why: why.clone(),
                }),
                None => Ok(()),
            },
        }
    }

    fn check_image(&self) -> Result<(), NativeRefusal> {
        match &self.state.borrow().image_refusal {
            Some((code, why)) => Err(NativeRefusal {
                code,
                why: why.clone(),
            }),
            None => Ok(()),
        }
    }

    fn runs_image_runs(&self) -> bool {
        self.state
            .borrow()
            .native
            .as_ref()
            .is_some_and(|script| script.image_runs)
    }

    fn supports_gpus(&self) -> bool {
        !self.state.borrow().gpus_off
    }

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        let state = self.state.borrow();
        let run = &state.containers.get(&id.0)?.run_id;
        state.cgroups.get(run).cloned()
    }

    fn whereabouts(&mut self, id: &RunHandle) -> Whereabouts {
        let mut state = self.state.borrow_mut();
        let Some(run) = state.containers.get(&id.0).map(|c| c.run_id.clone()) else {
            return Whereabouts::NoProcess;
        };
        *state.where_asks.entry(run.clone()).or_default() += 1;
        if let Some(dir) = state.grouped_when_asked.remove(&run) {
            state.cgroups.insert(run.clone(), dir);
        }
        if let Some(dir) = state.cgroups.get(&run) {
            return Whereabouts::Grouped(dir.clone());
        }
        match state.elsewhere.get(&run) {
            Some(group) => Whereabouts::Elsewhere(group.clone()),
            None => Whereabouts::NoProcess,
        }
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
                never_started: !c.started,
            })
            .collect())
    }
}
