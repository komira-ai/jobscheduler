//! The rescue of an agent whose worker is stuck, with the VM backend on: it destroys the VMs
//! it finds as it stops the containers.

use std::sync::Arc;

use clap::Parser as _;
use js_agent::vm::VmSettings;
use js_agent::{Cli, Platform, Worker};
use js_vm_lease::fake::{Call, FakeDriver};
use js_vm_lease::{Step, VmDriver};

use super::agent::vm_flags;
use super::vm;
use crate::jobs::fake::FakeMachine;
use crate::jobs::{STEP_NEVER, plan};
use crate::support::{Scratch, flags};

/// The VM settings of flags over a driver and a store in `dir`.
fn settings(dir: &Scratch) -> VmSettings {
    let mut args = vec!["js-agent".to_owned()];
    args.extend(flags(&[]).iter().map(|flag| (*flag).to_owned()));
    args.extend(vm_flags(dir));
    let cli = Cli::try_parse_from(&args).unwrap();
    let mac = Platform {
        os: "macos".to_owned(),
        arch: "arm64".to_owned(),
    };
    VmSettings::from_cli(&cli, &mac, true).unwrap().unwrap()
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
    let mut with_vms = plan(&dir, &machine, STEP_NEVER);
    with_vms.vm = Some(settings(&dir));
    let worker = match Worker::start(with_vms) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    };
    assert_eq!(driver.steps_of(&vm("before")), [Step::Stop, Step::Destroy]);

    // A VM made while the worker ran, and left when it got stuck.
    driver.clone_base(&vm("stuck")).unwrap();
    let lines = worker.rescue();
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
    let worker = match Worker::start(plan(&plain, &machine, STEP_NEVER)) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    };
    let _ = worker.rescue();
    assert_eq!(untouched.calls(), Vec::<Call>::new());
}
