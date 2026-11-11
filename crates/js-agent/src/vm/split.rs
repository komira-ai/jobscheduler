//! [`WithVms`]: the agent's runtime with VM runs beside it. A run whose spec carries the
//! VM kind ([`label::KIND`]) goes to the [`VmRuntime`]; every other run, and every
//! question about the host, goes to the runtime the agent had without VMs.

use std::path::PathBuf;
use std::time::Duration;

use js_agent_wire::{LogReport, LogTail, NativeSpec, NetworkCeiling, PortRange};

use super::VmRuntime;
use crate::lifecycle::{
    Found, NativeRefusal, Prepared, RunHandle, RunSpec, Runtime, RuntimeError, Status, Ticket,
    label,
};

/// The agent's runtime and its VM runtime; see the [module](self).
pub struct WithVms {
    vms: VmRuntime,
    other: Box<dyn Runtime>,
}

impl WithVms {
    /// `other`, with VM runs going to `vms`.
    pub fn new(vms: VmRuntime, other: Box<dyn Runtime>) -> Self {
        Self { vms, other }
    }

    /// The runtime that owns the run `id`.
    fn owner(&mut self, id: &RunHandle) -> &mut dyn Runtime {
        match self.vms.holds(id) {
            true => &mut self.vms,
            false => self.other.as_mut(),
        }
    }
}

impl Runtime for WithVms {
    fn prepare(&mut self, image: &str, waited: Option<Ticket>) -> Prepared {
        self.other.prepare(image, waited)
    }

    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        match spec.labels.get(label::KIND).map(String::as_str) {
            Some(label::VM_KIND) => self.vms.create_and_start(spec),
            _ => self.other.create_and_start(spec),
        }
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        self.owner(id).poll(id)
    }

    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.owner(id).stop(id, grace)
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.owner(id).remove(id)
    }

    /// The other runtime's: a VM is never adopted.
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.other.list_labelled()
    }

    fn list_ids(&mut self) -> Result<Vec<RunHandle>, RuntimeError> {
        self.other.list_ids()
    }

    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        self.owner(id).log_tail(id)
    }

    fn logs_report(&mut self, id: &RunHandle) -> Option<LogReport> {
        self.owner(id).logs_report(id)
    }

    fn check_native(&self, native: &NativeSpec) -> Result<(), NativeRefusal> {
        self.other.check_native(native)
    }

    fn check_env(&self, env: &[(String, String)]) -> Result<(), NativeRefusal> {
        self.other.check_env(env)
    }

    fn check_image(&self) -> Result<(), NativeRefusal> {
        self.other.check_image()
    }

    fn check_vm(&self) -> Result<(), NativeRefusal> {
        self.vms.check_vm()
    }

    fn runs_image_runs(&self) -> bool {
        self.other.runs_image_runs()
    }

    /// The other runtime's leftovers, then the VMs an earlier process left.
    fn reclaim(&mut self) -> Vec<String> {
        let mut lines = self.other.reclaim();
        lines.extend(self.vms.reclaim());
        lines
    }

    fn supports_gpus(&self) -> bool {
        self.other.supports_gpus()
    }

    fn check_egress(&self) -> Result<(), RuntimeError> {
        self.other.check_egress()
    }

    fn network_ceiling(&self) -> NetworkCeiling {
        self.other.network_ceiling()
    }

    fn host_ports(&self) -> Vec<PortRange> {
        self.other.host_ports()
    }

    /// The other runtime's: a VM is never adopted.
    fn host_port_lost(&self, id: &RunHandle) -> Option<String> {
        self.other.host_port_lost(id)
    }

    fn check_native_egress(&self, native: &NativeSpec) -> Result<(), RuntimeError> {
        self.other.check_native_egress(native)
    }

    fn cgroup(&mut self, id: &RunHandle) -> Option<PathBuf> {
        self.owner(id).cgroup(id)
    }

    fn lease_dir(&self, run_id: &str) -> Option<PathBuf> {
        self.other.lease_dir(run_id)
    }

    fn places_in_cgroup(&self) -> bool {
        self.other.places_in_cgroup()
    }
}
