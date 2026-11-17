//! The rescue of an agent whose worker is stuck, with the VM backend on: it destroys the VMs
//! it finds as it stops the jobs of the build's runtime.

use std::sync::Arc;

use js_agent::Worker;
use js_agent::vm::VmSettings;
use js_vm_lease::fake::{Call, FakeDriver};
use js_vm_lease::{Step, VmDriver};

use super::vm;
use crate::jobs::fake::FakeMachine;
use crate::jobs::{STEP_NEVER, plan};
#[cfg(target_os = "macos")]
use crate::native::fake_helper::FakeHelper;
use crate::support::Scratch;

/// The VM settings over a driver binary and a store with a base image, both in `dir`.
fn settings(dir: &Scratch) -> VmSettings {
    let driver = dir.write("js-vmm", "#!/bin/sh\n", 0o755);
    let store = dir.path("vm-store");
    std::fs::create_dir_all(store.join("base")).unwrap();
    VmSettings::new(Some(&driver), Some(&store))
        .unwrap()
        .unwrap()
}

/// A worker and what its jobs backend needs while it runs.
struct Running {
    worker: Worker,
    /// The fake helper the native backend talks to, kept until the test ends.
    #[cfg(target_os = "macos")]
    _helper: FakeHelper,
}

/// A worker over `machine` under `dir`, with `vm` as its VM backend. Its jobs backend is the
/// build's: the container runtime of `plan` on Linux, the native runtime on macOS, over a fake
/// helper, since a worker asks the helper for its boot reset when it starts.
fn start(dir: &Scratch, machine: &FakeMachine, vm: Option<VmSettings>) -> Running {
    #[cfg(target_os = "macos")]
    let helper = FakeHelper::start(dir);
    let mut plan = plan(dir, machine, STEP_NEVER);
    plan.vm = vm;
    #[cfg(target_os = "macos")]
    {
        plan.backend = helper.backend(dir);
    }
    match Worker::start(plan) {
        Ok(worker) => Running {
            worker,
            #[cfg(target_os = "macos")]
            _helper: helper,
        },
        Err(why) => panic!("the worker did not start: {why}"),
    }
}

/// Asserts that a worker with the VM backend reclaims the VMs an earlier process left when it
/// starts, and that its rescue (the agent's last resort when the worker is stuck) stops and
/// destroys a VM it finds then, saying so in its lines; and that a rescue without the backend
/// asks no VM driver anything. Catches a stuck agent that leaves its VMs running on the host,
/// holding both of its slots.
#[test]
fn the_rescue_stops_and_destroys_the_vms_it_finds() {
    let dir = Scratch::new("vm-rescue");
    let driver = Arc::new(FakeDriver::new().leaving(&[vm("before")]));
    let machine = FakeMachine::new().with_vms(&driver);
    let running = start(&dir, &machine, Some(settings(&dir)));
    assert_eq!(driver.steps_of(&vm("before")), [Step::Stop, Step::Destroy]);

    // A VM made while the worker ran, and left when it got stuck.
    driver.clone_base(&vm("stuck")).unwrap();
    let lines = running.worker.rescue();
    assert!(
        lines.contains(&"rescue: vm: destroyed the VM stuck an earlier process left".to_owned()),
        "{lines:?}"
    );
    assert_eq!(
        driver.steps_of(&vm("stuck")),
        [Step::Clone, Step::Stop, Step::Destroy]
    );
    assert!(driver.clones().is_empty());

    let plain = Scratch::new("vm-rescue-off");
    let untouched = Arc::new(FakeDriver::new().leaving(&[vm("other")]));
    let machine = FakeMachine::new().with_vms(&untouched);
    let running = start(&plain, &machine, None);
    let _ = running.worker.rescue();
    assert_eq!(untouched.calls(), Vec::<Call>::new());
}
