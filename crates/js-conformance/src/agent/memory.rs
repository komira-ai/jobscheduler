//! A container runtime that lives in memory, so the real agent lifecycle
//! (`js_agent::lifecycle::Lifecycle`) can run in one process with no engine: [`Memory`],
//! the runtime, and [`Time`], the clock the lifecycle reads, which its holder moves.
//!
//! It is a runtime fake by design. It models what the lifecycle asks of a
//! [`Runtime`] and nothing of a container engine: no image, no process, no limit, no
//! network, no log. A container runs from the moment it is created until its holder says
//! it exited ([`Memory::exit`]) or the lifecycle stops it (it then exits with
//! [`STOPPED_CODE`]), and is gone once removed. Every create makes a container, a second
//! one for a run id that has one too: the runtime keeps the record ([`Memory::created`])
//! and leaves it to the caller to count a run launched twice.
//!
//! [`Runtime::list_labelled`] lists every container that carries the run label
//! ([`label::RUN`]), running or exited, as a restarted agent finds it, with no age (the
//! runtime keeps no clock). A new runtime lists nothing: a host whose containers died
//! with the agent process (design section 8, row 4).
//!
//! A [`Memory`] is a handle: its clones share one set of containers, so a test keeps one
//! while the lifecycle owns another. A [`Time`] is the same: its clones share one time.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{
    Clock, Exit, Found, RunHandle, RunSpec, Runtime, RuntimeError, Status, label,
};

/// The status a stopped container exits with, as an engine reports a container it killed.
pub const STOPPED_CODE: i32 = 137;

/// A runtime whose containers exist in memory and exit when its holder says so or when
/// they are stopped.
#[derive(Clone, Default)]
pub struct Memory(Rc<RefCell<World>>);

/// The containers of one runtime, by id, and what was created.
#[derive(Default)]
struct World {
    containers: BTreeMap<String, Container>,
    next: u32,
    created: Vec<String>,
}

/// One container: the run it is for, its labels, and how it exited once it has.
struct Container {
    run: String,
    labels: BTreeMap<String, String>,
    exit: Option<Exit>,
}

impl Memory {
    /// Makes the running container of `run` exit with `code`, and returns whether there
    /// was one. A container that has exited, or was removed, is left as it is.
    pub fn exit(&self, run: &str, code: i32) -> bool {
        let mut world = self.0.borrow_mut();
        let running = world
            .containers
            .values_mut()
            .find(|container| container.run == run && container.exit.is_none());
        running
            .map(|container| {
                container.exit = Some(Exit {
                    code,
                    oom_killed: false,
                });
            })
            .is_some()
    }

    /// The run id of every container created, in the order they were created; removed
    /// ones too.
    pub fn created(&self) -> Vec<String> {
        self.0.borrow().created.clone()
    }

    /// How many containers there are, running or exited; removed ones are not.
    pub fn alive(&self) -> usize {
        self.0.borrow().containers.len()
    }

    /// The run ids whose containers run now.
    pub fn running(&self) -> BTreeSet<String> {
        let world = self.0.borrow();
        let running = world
            .containers
            .values()
            .filter(|container| container.exit.is_none());
        running.map(|container| container.run.clone()).collect()
    }
}

impl Runtime for Memory {
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let mut world = self.0.borrow_mut();
        world.next += 1;
        let id = format!("c{}", world.next);
        world.created.push(spec.run_id.clone());
        let container = Container {
            run: spec.run_id.clone(),
            labels: spec.labels.clone(),
            exit: None,
        };
        world.containers.insert(id.clone(), container);
        Ok(RunHandle(id))
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        let world = self.0.borrow();
        Ok(match world.containers.get(&id.0) {
            None => Status::Gone,
            Some(container) => container.exit.map_or(Status::Running, Status::Exited),
        })
    }

    fn stop(&mut self, id: &RunHandle, _grace: Duration) -> Result<(), RuntimeError> {
        let mut world = self.0.borrow_mut();
        if let Some(container) = world.containers.get_mut(&id.0) {
            container.exit.get_or_insert(Exit {
                code: STOPPED_CODE,
                oom_killed: false,
            });
        }
        Ok(())
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.0.borrow_mut().containers.remove(&id.0);
        Ok(())
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        let world = self.0.borrow();
        let labelled = world
            .containers
            .iter()
            .filter(|(_, container)| container.labels.contains_key(label::RUN));
        let found = labelled.map(|(id, container)| Found {
            id: RunHandle(id.clone()),
            labels: container.labels.clone(),
            running_ms: 0,
            never_started: false,
        });
        Ok(found.collect())
    }
}

/// The lifecycle's clock: the milliseconds its holder put in the cell. Clones share it.
#[derive(Clone, Debug, Default)]
pub struct Time(pub Rc<Cell<u64>>);

impl Clock for Time {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use js_agent::lifecycle::Body;

    use super::*;

    /// A run spec for `run` with `labels`, the rest empty.
    fn spec(run: &str, labels: &[(&str, &str)]) -> RunSpec {
        let labels = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()));
        RunSpec {
            run_id: run.to_owned(),
            body: Body::Container {
                image: "image".to_owned(),
                args: Vec::new(),
            },
            env: Vec::new(),
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            pids: None,
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            scratch: "scratch".into(),
            files: None,
            slot: 0,
            isolation: Default::default(),
            network: Default::default(),
            ports: Vec::new(),
            labels: labels.collect(),
            bundle: None,
            log_sink: None,
        }
    }

    fn exited(code: i32) -> Status {
        Status::Exited(Exit {
            code,
            oom_killed: false,
        })
    }

    /// Asserts a container's whole life in the runtime: created running, listed by its run
    /// label with the labels it was made with (one made without the label is not listed),
    /// exited with the code its holder names, a stopped one exits 137, a second stop keeps
    /// the first exit, and a removed one polls gone and is no longer listed or alive while
    /// `created` still names it. Catches a runtime that lists an unlabelled container or
    /// drops a label, a stop that overwrites an exit, and a removed container that lingers.
    #[test]
    fn the_in_memory_runtime_creates_polls_stops_and_lists_by_label() {
        let mut runtime = Memory::default();
        let a = runtime
            .create_and_start(&spec("a", &[(label::RUN, "a"), ("k", "v")]))
            .unwrap();
        let b = runtime.create_and_start(&spec("b", &[])).unwrap();
        assert_ne!(a, b);
        assert_eq!(runtime.poll(&a).unwrap(), Status::Running);
        let both = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        assert_eq!(runtime.running(), both);

        let listed = runtime.list_labelled().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, a);
        assert_eq!(listed[0].labels["k"], "v");
        assert_eq!(listed[0].labels[label::RUN], "a");
        assert_eq!((listed[0].running_ms, listed[0].never_started), (0, false));

        assert!(runtime.exit("a", 3));
        assert_eq!(runtime.poll(&a).unwrap(), exited(3));
        runtime.stop(&b, Duration::ZERO).unwrap();
        assert_eq!(runtime.poll(&b).unwrap(), exited(STOPPED_CODE));
        runtime.stop(&a, Duration::ZERO).unwrap();
        assert_eq!(runtime.poll(&a).unwrap(), exited(3));
        assert!(runtime.running().is_empty());
        assert_eq!(
            runtime.list_labelled().unwrap().len(),
            1,
            "exited is listed"
        );

        runtime.remove(&a).unwrap();
        assert_eq!(runtime.poll(&a).unwrap(), Status::Gone);
        assert!(runtime.list_labelled().unwrap().is_empty());
        assert_eq!(runtime.alive(), 1);
        assert_eq!(runtime.created(), ["a", "b"]);
    }

    /// Asserts that `exit` acts only on a running container: none for an unknown run, none
    /// for one that has exited (its code is kept), none once it is removed. Catches an exit
    /// that rewrites how a stopped container ended, or reports one it did not end.
    #[test]
    fn an_exit_ends_only_a_running_container() {
        let mut runtime = Memory::default();
        assert!(!runtime.exit("a", 0));
        let a = runtime.create_and_start(&spec("a", &[])).unwrap();
        runtime.stop(&a, Duration::ZERO).unwrap();
        assert!(!runtime.exit("a", 0));
        assert_eq!(runtime.poll(&a).unwrap(), exited(STOPPED_CODE));
        runtime.remove(&a).unwrap();
        assert!(!runtime.exit("a", 0));
    }

    /// Asserts that a stop or a removal of a container that is gone succeeds and changes
    /// nothing, and that a second create for one run id makes a second container, both
    /// recorded. Catches a runtime that fails a cleanup of what is already gone (the
    /// lifecycle would retry it forever) and one that hides a second launch.
    #[test]
    fn a_gone_container_stops_and_removes_and_a_second_create_is_recorded() {
        let mut runtime = Memory::default();
        let gone = RunHandle("c9".to_owned());
        runtime.stop(&gone, Duration::ZERO).unwrap();
        runtime.remove(&gone).unwrap();
        assert_eq!(runtime.alive(), 0);
        let first = runtime.create_and_start(&spec("a", &[])).unwrap();
        let second = runtime.create_and_start(&spec("a", &[])).unwrap();
        assert_ne!(first, second);
        assert_eq!(runtime.created(), ["a", "a"]);
        assert_eq!(runtime.alive(), 2);
    }

    /// Asserts that clones of a runtime share its containers and clones of a time share it.
    /// Catches a handle that copies its state, so a test's view and the lifecycle's drift.
    #[test]
    fn clones_share_the_containers_and_the_time() {
        let mut runtime = Memory::default();
        let held = runtime.clone();
        runtime.create_and_start(&spec("a", &[])).unwrap();
        assert_eq!(held.created(), ["a"]);
        assert!(held.exit("a", 0));
        assert!(runtime.running().is_empty());

        let time = Time::default();
        let clock: Box<dyn Clock> = Box::new(time.clone());
        assert_eq!(clock.now_ms(), 0);
        time.0.set(42);
        assert_eq!(clock.now_ms(), 42);
    }
}
