//! A container runtime in memory that the test thread and the worker thread share, a
//! clock the test moves, and the machine that hands both to the agent. Nothing here starts
//! a process.
//!
//! The runtime is made on the worker thread (as the agent does), from a `World` the test
//! keeps a handle to: the test makes containers exit, makes calls fail, holds a call
//! until it lets go, and reads what was called. Every wait on the other thread has a hang
//! guard that fails the test with a message; none is a time the test expects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent::integrity::{FakeHost, HostView, Mount, Process, Watched};
use js_agent::lifecycle::{
    Clock, ContainerId, ContainerRuntime, ContainerSpec, Exit, Found, RuntimeError, Status,
};
use js_agent::{Machine, RuntimeBuild, SharedClock};

/// How long a test waits for the other thread before it fails.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

/// A clock that moves only when a test says so. Clones share one reading.
#[derive(Default)]
pub struct TestClock(AtomicU64);

impl TestClock {
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Container {
    run_id: String,
    spec: ContainerSpec,
    exit: Option<Exit>,
}

#[derive(Default)]
struct State {
    containers: BTreeMap<String, Container>,
    next: u32,
    /// Calls to fail: the name of the call and how many more times it fails.
    failing: BTreeMap<&'static str, u32>,
    /// `create_and_start` waits while this is set.
    hold_creates: bool,
    /// How many `create_and_start` calls have begun.
    entered: u32,
    /// The calls, in order: `create r1`, `stop r1`, `remove r1`, `list`.
    log: Vec<String>,
    /// The call that panics, once.
    panic_at: Option<&'static str>,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

/// What the runtime and the test both see. Clones share one world.
#[derive(Clone)]
pub struct World(Arc<Inner>);

impl World {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The container of `run_id` exits with `code`.
    pub fn exit(&self, run_id: &str, code: i32) {
        let mut state = self.lock();
        let found = state.containers.values_mut().find(|c| c.run_id == run_id);
        found.expect("no such container").exit = Some(Exit {
            code,
            oom_killed: false,
        });
    }

    /// The calls so far, in order.
    pub fn log(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    /// How many containers exist.
    pub fn alive(&self) -> usize {
        self.lock().containers.len()
    }

    /// The run ids of the containers that were created, in order.
    pub fn created(&self) -> Vec<String> {
        let log = self.log();
        let runs = log.iter().filter_map(|call| call.strip_prefix("create "));
        runs.map(str::to_owned).collect()
    }

    /// Makes the next `times` calls named `call` (`create`, `poll`, `stop`, `remove` or
    /// `list`) fail.
    pub fn fail(&self, call: &'static str, times: u32) {
        self.lock().failing.insert(call, times);
    }

    /// Makes the next call named `call` panic.
    pub fn panic_at(&self, call: &'static str) {
        self.lock().panic_at = Some(call);
    }

    /// From now on `create_and_start` waits for [`World::release`].
    pub fn hold_creates(&self) {
        self.lock().hold_creates = true;
    }

    /// Lets the held `create_and_start` calls go on.
    pub fn release(&self) {
        self.lock().hold_creates = false;
        self.0.changed.notify_all();
    }

    /// Waits until `n` `create_and_start` calls have begun.
    pub fn wait_entered(&self, n: u32) {
        let state = self.lock();
        let waited = self
            .0
            .changed
            .wait_timeout_while(state, HANG_GUARD, |state| state.entered < n)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            !waited.1.timed_out(),
            "the worker never reached the runtime"
        );
    }

    /// The runtime of this world.
    pub fn runtime(&self) -> Runtime {
        Runtime(self.clone())
    }
}

impl State {
    /// Whether the call `call` fails now (and counts it), and panics if it is to.
    fn fails(&mut self, call: &'static str) -> bool {
        if self.panic_at.take_if(|at| *at == call).is_some() {
            panic!("the {call} of the fake runtime panics");
        }
        match self.failing.get_mut(call) {
            Some(times) if *times > 0 => {
                *times -= 1;
                true
            }
            _ => false,
        }
    }
}

fn failed(call: &str) -> RuntimeError {
    RuntimeError::new(format!("{call} failed"))
}

/// The container runtime the worker thread makes.
pub struct Runtime(World);

impl ContainerRuntime for Runtime {
    fn create_and_start(&mut self, spec: &ContainerSpec) -> Result<ContainerId, RuntimeError> {
        let world = &self.0;
        let mut state = world.lock();
        state.entered += 1;
        world.0.changed.notify_all();
        while state.hold_creates {
            let (held, waited) = world
                .0
                .changed
                .wait_timeout(state, HANG_GUARD)
                .unwrap_or_else(PoisonError::into_inner);
            state = held;
            if waited.timed_out() {
                return Err(RuntimeError::new("the test never released the runtime"));
            }
        }
        state.log.push(format!("create {}", spec.run_id));
        if state.fails("create") {
            return Err(failed("create"));
        }
        state.next += 1;
        let id = format!("c{}", state.next);
        let container = Container {
            run_id: spec.run_id.clone(),
            spec: spec.clone(),
            exit: None,
        };
        state.containers.insert(id.clone(), container);
        Ok(ContainerId(id))
    }

    fn poll(&mut self, id: &ContainerId) -> Result<Status, RuntimeError> {
        let mut state = self.0.lock();
        if state.fails("poll") {
            return Err(failed("poll"));
        }
        Ok(match state.containers.get(&id.0) {
            None => Status::Gone,
            Some(container) => container.exit.map_or(Status::Running, Status::Exited),
        })
    }

    fn stop(&mut self, id: &ContainerId, _grace: Duration) -> Result<(), RuntimeError> {
        let mut state = self.0.lock();
        let run = state.containers.get(&id.0).map(|c| c.run_id.clone());
        state.log.push(format!("stop {}", run.unwrap_or_default()));
        if state.fails("stop") {
            return Err(failed("stop"));
        }
        if let Some(container) = state.containers.get_mut(&id.0) {
            container.exit.get_or_insert(Exit {
                code: 137,
                oom_killed: false,
            });
        }
        Ok(())
    }

    fn remove(&mut self, id: &ContainerId) -> Result<(), RuntimeError> {
        let mut state = self.0.lock();
        let run = state.containers.get(&id.0).map(|c| c.run_id.clone());
        state
            .log
            .push(format!("remove {}", run.unwrap_or_default()));
        if state.fails("remove") {
            return Err(failed("remove"));
        }
        state.containers.remove(&id.0);
        Ok(())
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        let mut state = self.0.lock();
        state.log.push("list".to_owned());
        if state.fails("list") {
            return Err(failed("list"));
        }
        Ok(state
            .containers
            .iter()
            .map(|(id, container)| Found {
                id: ContainerId(id.clone()),
                labels: container.spec.labels.clone(),
                running_ms: 0,
                never_started: false,
            })
            .collect())
    }
}

/// How a [`FakeMachine`] misbehaves when the worker builds the lifecycle on it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// It builds.
    Fine,
    /// The runtime cannot be made.
    RuntimeFails,
    /// The runtime panics while it is made.
    Panics,
    /// The agent cannot see the host the checks read.
    HostNotSeen,
}

/// The machine the agent is given in a test: the world's runtime, a quiet host and the test
/// clock.
#[derive(Clone)]
pub struct FakeMachine {
    pub world: World,
    pub clock: Arc<TestClock>,
    pub build: Build,
}

impl FakeMachine {
    pub fn new() -> Self {
        Self::of(&World::new())
    }

    /// A machine over `world`: a second process on the same engine.
    pub fn of(world: &World) -> Self {
        Self {
            world: world.clone(),
            clock: Arc::default(),
            build: Build::Fine,
        }
    }

    pub fn failing(build: Build) -> Self {
        Self {
            build,
            ..Self::new()
        }
    }
}

impl Machine for FakeMachine {
    fn clock(&self) -> SharedClock {
        self.clock.clone()
    }

    fn runtime(
        &self,
        _build: &RuntimeBuild<'_>,
    ) -> Result<(Box<dyn ContainerRuntime>, Vec<String>), String> {
        match self.build {
            Build::RuntimeFails => Err("the engine does not answer".to_owned()),
            Build::Panics => panic!("the runtime panics while it is made"),
            Build::Fine | Build::HostNotSeen => Ok((
                Box::new(self.world.runtime()),
                vec!["the fake engine is ready".to_owned()],
            )),
        }
    }

    fn view(&self) -> Box<dyn HostView> {
        let host = FakeHost::quiet();
        if self.build == Build::HostNotSeen {
            host.move_to_namespace("self", "pid", "pid:[4026539999]");
        }
        Box::new(AnyDirectory(host))
    }
}

/// A quiet host whose every watched directory exists and never changes.
struct AnyDirectory(FakeHost);

impl HostView for AnyDirectory {
    fn processes(&self) -> Result<Vec<Process>, String> {
        self.0.processes()
    }

    fn mounts(&self) -> Result<Vec<Mount>, String> {
        self.0.mounts()
    }

    fn users(&self) -> Result<BTreeSet<String>, String> {
        self.0.users()
    }

    fn groups(&self) -> Result<BTreeSet<String>, String> {
        self.0.groups()
    }

    fn modules(&self) -> Result<BTreeSet<String>, String> {
        self.0.modules()
    }

    fn watched(&self, _dir: &Path) -> Result<Watched, String> {
        Ok(Watched::Digest("quiet".to_owned()))
    }

    fn link(&self, path: &str) -> Result<String, String> {
        self.0.link(path)
    }

    fn file(&self, path: &str) -> Result<String, String> {
        self.0.file(path)
    }
}
