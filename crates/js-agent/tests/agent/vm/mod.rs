//! The VM backend (`js_agent::vm`): a VM start runs in a fresh VM of its own, cloned,
//! booted, run, collected and destroyed, at most two at once. Every test drives the
//! recording driver of `js-vmm` (`FakeDriver`), so no VM is made; a test waits for the
//! driver's calls, never for time.

mod agent;
mod helper;
mod lifecycle;
mod rescue;
mod runtime;
mod split;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use js_agent::lifecycle::{Body, RunHandle, RunSpec, Runtime, Status, label};
use js_agent::vm::VmRuntime;
use js_agent_wire::Isolation;
use js_vm_lease::fake::FakeDriver;
use js_vm_lease::{Step, VmName};

/// The VM name of `run_id`.
pub fn vm(run_id: &str) -> VmName {
    VmName::new(run_id).unwrap()
}

/// The run of a VM start `run_id` that runs `args`: 1.5 CPUs and 1 GiB and one byte.
pub fn spec(run_id: &str, args: &[&str]) -> RunSpec {
    RunSpec {
        run_id: run_id.to_owned(),
        body: Body::Container {
            image:
                "macos:15@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        },
        env: vec![("LANG".to_owned(), "C".to_owned())],
        cpu_millis: 1500,
        memory_bytes: (1 << 30) + 1,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch: PathBuf::from("/nonexistent/scratch"),
        files: None,
        slot: 0,
        isolation: Isolation::Contained,
        labels: BTreeMap::from([(label::KIND.to_owned(), label::VM_KIND.to_owned())]),
        bundle: None,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
    }
}

/// A VM runtime over `driver`.
pub fn runtime(driver: &Arc<FakeDriver>) -> VmRuntime {
    VmRuntime::new(driver.clone())
}

/// What `rt` says of `id` once the driver has been asked to stop its VM: by then the VM's
/// thread has reported how the command ended.
pub fn ended(rt: &mut dyn Runtime, driver: &FakeDriver, id: &RunHandle) -> Status {
    driver.wait_for(&vm(&id.0), Step::Stop);
    rt.poll(id).unwrap()
}

/// The status of a run that exited with `code`.
pub fn runtime_exit(code: i32) -> Status {
    Status::Exited(js_agent::lifecycle::Exit {
        code,
        oom_killed: false,
    })
}
