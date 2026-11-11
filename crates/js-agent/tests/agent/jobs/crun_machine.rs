//! The machine of the agents that run on the crun node of `crun_e2e`: the real [`System`]
//! for the clock, the runtime and the start check, and the fake machine for what a test that
//! is not root cannot read.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::crun_host::CrunHost;
use js_agent::integrity::HostView;
use js_agent::lifecycle::{
    Found, Prepared, RunHandle, RunSpec, Runtime, RuntimeError, Status, Ticket,
};
use js_agent::quota::ScratchQuota;
use js_agent::{Backend, FileReader, Machine, RuntimeBuild, SharedClock, System};
use js_agent_wire::{LogTail, PortRange};

use super::crun_e2e::seams;
use super::fake::FakeMachine;

/// The real [`System`] for the clock, the runtime and the start check, over the node; the
/// view of the host, the cgroup files and the quota are the fake machine's, as a test that is
/// not root cannot read the namespaces of process 1.
pub(super) struct CrunMachine {
    pub(super) system: System,
    pub(super) fake: FakeMachine,
    /// The grace of every stop the runtime was asked for.
    pub(super) stops: Arc<Mutex<Vec<Duration>>>,
    /// The live-log capture cap (`--log-file-cap`) of the crun settings of every runtime
    /// opened, in order.
    pub(super) log_file_caps: Arc<Mutex<Vec<u64>>>,
    /// The image budget (`--image-budget-gib`, in bytes) of the crun settings of every runtime
    /// opened, in order.
    pub(super) image_budgets: Arc<Mutex<Vec<Option<u64>>>>,
    /// The host ports (`--host-port`) of the crun settings of every runtime opened, beside the
    /// ones the runtime opened from them allows ([`Runtime::host_ports`]), in order.
    pub(super) host_ports: Arc<Mutex<Vec<HostPortsSeen>>>,
}

/// The host ports a crun runtime was opened with, and the ones it then allowed.
pub(super) type HostPortsSeen = (Vec<PortRange>, Vec<PortRange>);

impl CrunMachine {
    pub(super) fn new() -> Arc<Self> {
        Self::with_system(System::new().with_crun(seams()))
    }

    /// The machine over `system`, with the fake machine's view, cgroup files and quota.
    pub(super) fn with_system(system: System) -> Arc<Self> {
        Arc::new(Self {
            system,
            fake: FakeMachine::new(),
            stops: Arc::default(),
            log_file_caps: Arc::default(),
            image_budgets: Arc::default(),
            host_ports: Arc::default(),
        })
    }
}

/// A runtime that notes the grace of each stop and leaves everything to the one it wraps.
struct Noting {
    inner: Box<dyn Runtime>,
    stops: Arc<Mutex<Vec<Duration>>>,
}

impl Runtime for Noting {
    fn prepare(&mut self, image: &str, waited: Option<Ticket>) -> Prepared {
        self.inner.prepare(image, waited)
    }

    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        self.inner.create_and_start(spec)
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        self.inner.poll(id)
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.stops.lock().expect("stops").push(grace);
        self.inner.stop(id, grace)
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.inner.remove(id)
    }

    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.inner.list_labelled()
    }

    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        self.inner.log_tail(id)
    }

    fn supports_gpus(&self) -> bool {
        self.inner.supports_gpus()
    }

    fn lease_dir(&self, run_id: &str) -> Option<PathBuf> {
        self.inner.lease_dir(run_id)
    }

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        self.inner.cgroup(id)
    }

    fn host_ports(&self) -> Vec<PortRange> {
        self.inner.host_ports()
    }

    fn host_port_lost(&self, id: &RunHandle) -> Option<String> {
        self.inner.host_port_lost(id)
    }
}

impl Machine for CrunMachine {
    fn clock(&self) -> SharedClock {
        self.system.clock()
    }

    fn runtime(&self, build: &RuntimeBuild<'_>) -> Result<(Box<dyn Runtime>, Vec<String>), String> {
        let mut planned = None;
        if let Backend::Container(settings) = build.backend {
            let caps = settings.crun().map(|plan| plan.logs.file_cap);
            self.log_file_caps.lock().expect("caps").extend(caps);
            let budgets = settings.crun().map(|plan| plan.image_budget);
            self.image_budgets.lock().expect("budgets").extend(budgets);
            planned = settings.crun().map(|plan| plan.host_ports.clone());
        }
        let (inner, warnings) = self.system.runtime(build)?;
        if let Some(planned) = planned {
            let allowed = inner.host_ports();
            self.host_ports
                .lock()
                .expect("host ports")
                .push((planned, allowed));
        }
        let stops = self.stops.clone();
        Ok((Box::new(Noting { inner, stops }), warnings))
    }

    fn view(&self) -> Box<dyn HostView> {
        self.fake.view()
    }

    fn cgroup_files(&self) -> Box<dyn FileReader> {
        self.fake.cgroup_files()
    }

    fn scratch_quota(&self, root: &Path) -> Arc<dyn ScratchQuota> {
        self.fake.scratch_quota(root)
    }

    fn crun_host(&self) -> CrunHost {
        self.system.crun_host()
    }
}
