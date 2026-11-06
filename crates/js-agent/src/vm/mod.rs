//! The VM backend: a VM start ([`js_agent_wire::StartKind::Vm`]) runs in a fresh macOS VM
//! of its own, cloned from the base image for that run and destroyed when it ends, whatever
//! its end (`docs/design/vm-backend.md`, 5.14). The machine is reused, the VM never.
//!
//! The backend is off unless the agent is given `--vm-driver` (the signed `js-vmm` binary
//! that runs each VM) and `--vm-store` (the directory of the base image, `base`, and the
//! clones, `runs/`); both are flags, never environment. It runs on a Mac only, beside the
//! native jobs (`--native-helper-socket`). With it off the hello says nothing of VMs and a
//! VM start is refused [`js_agent_wire::refusal::VM_UNAVAILABLE`]; with it on the hello
//! reports `vm.slots` ([`VM_SLOTS`]), `vm.max_cpus` and `vm.max_mem_gib` ([`report`]).
//!
//! - [`VmSlots`]: the gate of the two VMs a host runs at once. A third is refused
//!   ([`NoVmSlot`]), never queued, and a slot is given back on every way a run ends.
//! - [`VmRuntime`]: the [`Runtime`] of VM runs, a thread per VM over a
//!   [`js_vmm::lease::VmDriver`].
//! - [`WithVms`]: the agent's runtime with the VM runtime beside it.

mod runtime;
mod split;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use js_agent_wire::Hello;
use js_vmm::lease::VmDriver;

pub use self::runtime::VmRuntime;
pub use self::split::WithVms;
use crate::cli::Cli;
use crate::facts::Platform;
use crate::lifecycle::Runtime;
use crate::system::Machine;

/// How many VMs a host runs at once: the most macOS's virtualization lets one host run.
pub const VM_SLOTS: usize = 2;

/// The hello capability that says how many VMs the host runs at once.
pub const SLOTS_CAPABILITY: &str = "vm.slots";
/// The hello capability that says how many CPUs one VM may be given.
pub const MAX_CPUS_CAPABILITY: &str = "vm.max_cpus";
/// The hello capability that says how many GiB of memory one VM may be given.
pub const MAX_MEM_GIB_CAPABILITY: &str = "vm.max_mem_gib";

/// A VM start found both VM slots held: it is refused, never queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("both VM slots are in use: a third VM is refused, never queued")]
pub struct NoVmSlot;

/// The gate of the host's VMs: [`VM_SLOTS`] slots, each held by one VM from its clone to
/// its destroy ([`VmSlot`]).
#[derive(Debug, Clone, Default)]
pub struct VmSlots(Arc<Mutex<usize>>);

impl VmSlots {
    /// Takes a slot, which is given back when the [`VmSlot`] drops.
    ///
    /// # Errors
    ///
    /// [`NoVmSlot`] when every slot is held.
    pub fn acquire(&self) -> Result<VmSlot, NoVmSlot> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match *held < VM_SLOTS {
            true => {
                *held += 1;
                Ok(VmSlot(self.clone()))
            }
            false => Err(NoVmSlot),
        }
    }

    /// How many slots are free.
    pub fn free(&self) -> usize {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        VM_SLOTS.saturating_sub(*held)
    }
}

/// One held VM slot; dropping it gives the slot back.
#[derive(Debug)]
pub struct VmSlot(VmSlots);

impl Drop for VmSlot {
    fn drop(&mut self) {
        let mut held = (self.0).0.lock().unwrap_or_else(PoisonError::into_inner);
        *held = held.saturating_sub(1);
    }
}

/// The VM flags, checked: the backend is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmSettings {
    driver: PathBuf,
    store: PathBuf,
}

impl VmSettings {
    /// The VM flags of `cli` on `platform`, checked; `None` when neither is given (the
    /// backend is off). `native` says whether the agent runs native jobs.
    ///
    /// # Errors
    ///
    /// The flag at fault and why: one of the two without the other, a platform other than
    /// macOS, no native jobs, a driver that is not an absolute path to a file, or a store
    /// that is not an absolute path to a directory holding the base image `base`.
    pub fn from_cli(cli: &Cli, platform: &Platform, native: bool) -> Result<Option<Self>, String> {
        let (driver, store) = match (&cli.vm_driver, &cli.vm_store) {
            (None, None) => return Ok(None),
            (Some(driver), Some(store)) => (driver, store),
            (None, Some(_)) => return Err("--vm-store: needs --vm-driver".to_owned()),
            (Some(_), None) => return Err("--vm-driver: needs --vm-store".to_owned()),
        };
        if !platform.is_macos() {
            return Err("--vm-driver: VM leases run on macOS only".to_owned());
        }
        if !native {
            return Err(
                "--vm-driver: needs --native-helper-socket (the VMs run beside the native jobs)"
                    .to_owned(),
            );
        }
        if !driver.is_absolute() || !driver.is_file() {
            return Err("--vm-driver: must be an absolute path to the js-vmm binary".to_owned());
        }
        if !store.is_absolute() || !store.join("base").is_dir() {
            return Err(
                "--vm-store: must be an absolute path to a directory holding the base image `base`"
                    .to_owned(),
            );
        }
        Ok(Some(Self {
            driver: driver.clone(),
            store: store.clone(),
        }))
    }
}

/// Puts the VM capabilities into `hello`: [`VM_SLOTS`] VMs at once, and as the most one VM
/// may be given, the CPUs and the memory the host offers its jobs in all (a VM books both
/// from the same shape as every other job).
pub fn report(hello: &mut Hello) {
    let cpus = hello.shape.cpu_millis / 1000;
    let mem_gib = hello.shape.memory_bytes >> 30;
    let entries = [
        (SLOTS_CAPABILITY, VM_SLOTS.to_string()),
        (MAX_CPUS_CAPABILITY, cpus.to_string()),
        (MAX_MEM_GIB_CAPABILITY, mem_gib.to_string()),
    ];
    for (key, value) in entries {
        hello.capabilities.insert(key.to_owned(), value);
    }
}

/// `runtime`, with the VM runtime of `vm` beside it when the backend is on; its VMs are
/// the machine's ([`Machine::vm_driver`]).
///
/// # Errors
///
/// The machine cannot run VMs.
pub fn with_vms(
    machine: &dyn Machine,
    vm: Option<&VmSettings>,
    runtime: Box<dyn Runtime>,
) -> Result<Box<dyn Runtime>, String> {
    match vm {
        None => Ok(runtime),
        Some(settings) => {
            let driver = machine.vm_driver(settings)?;
            Ok(Box::new(WithVms::new(VmRuntime::new(driver), runtime)))
        }
    }
}

/// The VMs of this machine: `js-vmm`'s driver over the flags' binary and store.
///
/// # Errors
///
/// None on macOS.
#[cfg(target_os = "macos")]
pub fn system_driver(settings: &VmSettings) -> Result<Arc<dyn VmDriver>, String> {
    let driver = js_vmm::lease_system::real(settings.driver.clone(), settings.store.clone());
    Ok(Arc::new(driver))
}

/// The VMs of this machine: none, off macOS.
///
/// # Errors
///
/// Always: VMs run on macOS only.
#[cfg(not(target_os = "macos"))]
pub fn system_driver(_settings: &VmSettings) -> Result<Arc<dyn VmDriver>, String> {
    Err("VM leases run on macOS only".to_owned())
}
