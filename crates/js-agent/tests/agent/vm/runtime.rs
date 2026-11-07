//! [`VmRuntime`] alone: the order of a VM's life, its end on every path, and the gate of
//! two VMs at once.

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::lifecycle::{Body, Exit, RunHandle, Runtime, Status};
use js_agent::vm::{NoVmSlot, VM_SLOTS, VmRuntime};
use js_agent_wire::limits::{MAX_LOG_STREAM_LEN, MAX_WALL_MS};
use js_agent_wire::{LogTail, refusal};
use js_vm_lease::fake::{Call, FakeDriver};
use js_vm_lease::{Collected, ExecEnd, ExecRequest, Step, VmDriver, VmError, VmName, VmSize};

use super::{ended, runtime, spec, vm};

/// A grace far above what a fake guest takes to end its command once killed.
const GRACE: Duration = Duration::from_secs(120);

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// Asserts that a run clones the base image, boots the clone with its booking rounded up
/// (1.5 CPUs to 2, 1 GiB and a byte to 1025 MiB), waits for the guest, runs its command
/// with its environment, collects the output and then stops and destroys the VM, in that
/// order and each once; that the run ends with the guest's exit status and its output as
/// the tail; and that the slot is free once the run is removed. Catches a VM that is
/// reused, left behind, booted before it is cloned, or sized below what was booked.
#[test]
fn a_run_clones_boots_execs_collects_and_destroys_in_that_order() {
    let driver = Arc::new(
        FakeDriver::new()
            .holding()
            .answering(ExecEnd::Exited(3), "out", "err"),
    );
    let mut rt = runtime(&driver);
    let id = rt
        .create_and_start(&spec("run-1", &["make", "all"]))
        .unwrap();
    assert_eq!(id, RunHandle("run-1".to_owned()));
    assert!(rt.holds(&id));
    driver.wait_for(&vm("run-1"), Step::Exec);
    assert_eq!(rt.slots().free(), VM_SLOTS - 1);
    assert_eq!(rt.poll(&id).unwrap(), Status::Running);
    driver.finish(&vm("run-1"));

    assert_eq!(ended(&mut rt, &driver, &id), exited(3));
    assert_eq!(
        rt.log_tail(&id).unwrap(),
        Some(LogTail {
            stdout: "out".to_owned(),
            stderr: "err".to_owned(),
            truncated: false,
        })
    );
    rt.remove(&id).unwrap();
    assert!(!rt.holds(&id));
    assert_eq!(rt.slots().free(), VM_SLOTS);
    let one = vm("run-1");
    assert_eq!(
        driver.calls(),
        [
            Call::Clone(one.clone()),
            Call::Boot(
                one.clone(),
                VmSize {
                    cpus: 2,
                    memory_mib: 1025
                }
            ),
            Call::WaitReady(one.clone()),
            Call::Exec(
                one.clone(),
                ExecRequest {
                    argv: vec!["make".to_owned(), "all".to_owned()],
                    env: vec![("LANG".to_owned(), "C".to_owned())],
                    timeout_ms: MAX_WALL_MS,
                }
            ),
            Call::Collect(one.clone(), MAX_LOG_STREAM_LEN),
            Call::Stop(one.clone()),
            Call::Destroy(one),
        ]
    );
    assert_eq!(driver.live(), 0);
    assert!(driver.clones().is_empty());
}

/// Asserts that a run whose step fails (the exec, the boot, the clone, the wait for the
/// guest) ends as gone, and that its VM is still stopped and destroyed after the failing
/// step and its slot given back. Catches a failed run that leaves its VM running or its
/// clone on disk, or holds a slot for ever.
#[test]
fn a_failing_step_still_destroys_the_vm_and_frees_the_slot() {
    for (failing, steps) in [
        (
            Step::Exec,
            &[Step::Clone, Step::Boot, Step::Ready, Step::Exec][..],
        ),
        (Step::Ready, &[Step::Clone, Step::Boot, Step::Ready][..]),
        (Step::Boot, &[Step::Clone, Step::Boot][..]),
        (Step::Clone, &[Step::Clone][..]),
    ] {
        let driver = Arc::new(FakeDriver::new().failing(failing));
        let mut rt = runtime(&driver);
        let id = rt.create_and_start(&spec("run-1", &["make"])).unwrap();
        assert_eq!(ended(&mut rt, &driver, &id), Status::Gone, "{failing}");
        assert_eq!(rt.log_tail(&id).unwrap(), None, "{failing}");
        rt.remove(&id).unwrap();
        let mut expected = steps.to_vec();
        expected.extend([Step::Stop, Step::Destroy]);
        assert_eq!(driver.steps_of(&vm("run-1")), expected, "{failing}");
        assert_eq!(rt.slots().free(), VM_SLOTS, "{failing}");
        assert_eq!(driver.live(), 0, "{failing}");
    }
}

/// Asserts that a stop while the command runs kills it (the run ends 137, SIGKILL's), and
/// that the VM is then stopped, destroyed and its slot given back; and that a stop before
/// the command runs keeps it from running at all. Catches a cancel that leaves the VM, or
/// a stop that waits for the command to end by itself.
#[test]
fn a_cancelled_run_is_killed_destroyed_and_frees_its_slot() {
    let driver = Arc::new(FakeDriver::new().holding());
    let mut rt = runtime(&driver);
    let id = rt.create_and_start(&spec("run-1", &["sleep"])).unwrap();
    driver.wait_for(&vm("run-1"), Step::Exec);
    assert_eq!(rt.poll(&id).unwrap(), Status::Running);
    rt.stop(&id, GRACE).unwrap();
    assert_eq!(ended(&mut rt, &driver, &id), exited(128 + 9));
    rt.remove(&id).unwrap();
    assert_eq!(
        driver.steps_of(&vm("run-1")),
        [
            Step::Clone,
            Step::Boot,
            Step::Ready,
            Step::Exec,
            Step::Collect,
            Step::Stop,
            Step::Destroy
        ]
    );
    assert!(driver.calls().contains(&Call::Kill(vm("run-1"))));
    assert_eq!(rt.slots().free(), VM_SLOTS);
    assert_eq!(driver.live(), 0);
}

/// Asserts that with two VMs in flight a third start is refused with the typed no-slot
/// error (and the admission check says `no-capacity`), that nothing of it is cloned, and
/// that once one of the two has ended and been removed a new start runs. Catches a third
/// VM made beside two (the host refuses it, or worse), a refused start that is queued, and
/// a slot that is never given back.
#[test]
fn a_third_vm_is_refused_and_a_new_one_runs_once_one_ends() {
    let driver = Arc::new(FakeDriver::new().holding());
    let mut rt = runtime(&driver);
    let a = rt.create_and_start(&spec("run-a", &["a"])).unwrap();
    let b = rt.create_and_start(&spec("run-b", &["b"])).unwrap();
    driver.wait_for(&vm("run-a"), Step::Exec);
    driver.wait_for(&vm("run-b"), Step::Exec);
    assert_eq!(rt.slots().free(), 0);

    let refused = rt.check_vm().unwrap_err();
    assert_eq!(refused.code, refusal::NO_CAPACITY);
    assert_eq!(refused.why, NoVmSlot.to_string());
    let third = rt.create_and_start(&spec("run-c", &["c"])).unwrap_err();
    assert_eq!(
        third.to_string(),
        "both VM slots are in use: a third VM is refused, never queued"
    );
    assert!(!rt.holds(&RunHandle("run-c".to_owned())));
    assert!(driver.steps_of(&vm("run-c")).is_empty());

    driver.finish(&vm("run-a"));
    assert_eq!(ended(&mut rt, &driver, &a), exited(0));
    rt.remove(&a).unwrap();
    assert_eq!(rt.check_vm(), Ok(()));
    let c = rt.create_and_start(&spec("run-c", &["c"])).unwrap();
    driver.wait_for(&vm("run-c"), Step::Exec);
    assert_eq!(driver.peak(), 2);

    for (id, name) in [(b, "run-b"), (c, "run-c")] {
        driver.finish(&vm(name));
        assert_eq!(ended(&mut rt, &driver, &id), exited(0));
        rt.remove(&id).unwrap();
    }
    assert_eq!(rt.slots().free(), VM_SLOTS);
}

/// Asserts how each end the guest reports reads as an exit status: its own status, 128
/// plus a signal, 124 for the guest's own timeout. Catches a signal or a timeout that
/// reads as success.
#[test]
fn each_end_of_the_command_reads_as_an_exit_status() {
    for (end, code) in [
        (ExecEnd::Exited(0), 0),
        (ExecEnd::Exited(7), 7),
        (ExecEnd::Signaled(15), 128 + 15),
        (ExecEnd::TimedOut, 124),
        (ExecEnd::Killed, 137),
    ] {
        let driver = Arc::new(FakeDriver::new().answering(end, "", ""));
        let mut rt = runtime(&driver);
        let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
        assert_eq!(ended(&mut rt, &driver, &id), exited(code), "{end:?}");
        rt.remove(&id).unwrap();
    }
}

/// Asserts that output the driver cannot collect costs the run its tail and nothing else:
/// it still ends with the command's status, and its VM is destroyed. Catches a collect
/// failure that turns a finished job into a failed one.
#[test]
fn output_that_cannot_be_collected_costs_only_the_tail() {
    let driver = Arc::new(
        FakeDriver::new()
            .answering(ExecEnd::Exited(5), "out", "")
            .failing(Step::Collect),
    );
    let mut rt = runtime(&driver);
    let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
    assert_eq!(ended(&mut rt, &driver, &id), exited(5));
    assert_eq!(rt.log_tail(&id).unwrap(), None);
    // A stop of a run that has ended waits for nothing.
    rt.stop(&id, GRACE).unwrap();
    rt.remove(&id).unwrap();
    assert_eq!(driver.live(), 0);
}

/// Asserts that a destroy (or a stop) that fails makes the removal fail with the driver's
/// reason, which the lifecycle reads as a host that may hold the VM, and that the slot is
/// given back all the same. Catches a VM left on the host that is reported as removed.
#[test]
fn a_failed_destroy_or_stop_is_a_failed_removal() {
    for (failing, said) in [
        (
            Step::Destroy,
            "VM run-1: destroy failed: the fake was told to fail",
        ),
        (
            Step::Stop,
            "VM run-1: stop failed: the fake was told to fail",
        ),
    ] {
        let driver = Arc::new(FakeDriver::new().failing(failing));
        let mut rt = runtime(&driver);
        let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
        assert_eq!(rt.remove(&id).unwrap_err().to_string(), said);
        assert_eq!(rt.slots().free(), VM_SLOTS);
        assert_eq!(
            driver.steps_of(&vm("run-1")).last(),
            Some(&Step::Destroy),
            "{failing}"
        );
    }
}

/// Asserts that the removal of a run that is still running waits for its VM to be destroyed
/// (the command ends, then the VM goes), so nothing of it is left when it returns. Catches
/// a removal that returns while the VM still runs.
#[test]
fn a_removal_waits_for_the_destroy() {
    let driver = Arc::new(FakeDriver::new().holding());
    let mut rt = runtime(&driver);
    let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
    driver.wait_for(&vm("run-1"), Step::Exec);
    rt.stop(&id, GRACE).unwrap();
    rt.remove(&id).unwrap();
    assert_eq!(driver.live(), 0);
    assert_eq!(driver.steps_of(&vm("run-1")).last(), Some(&Step::Destroy));
}

/// A driver over the fake whose clone waits until the test opens a gate (when it holds
/// one), and which panics in one step (when told).
struct Wrapped {
    inner: FakeDriver,
    gate: Mutex<Option<Receiver<()>>>,
    panics_in: Option<Step>,
}

impl Wrapped {
    fn new(inner: FakeDriver) -> Self {
        Self {
            inner,
            gate: Mutex::new(None),
            panics_in: None,
        }
    }

    fn panic_if(&self, step: Step, vm: &VmName) {
        if self.panics_in == Some(step) {
            panic!("the driver of {vm} panics in {step}");
        }
    }
}

impl VmDriver for Wrapped {
    fn clone_base(&self, vm: &VmName) -> Result<(), VmError> {
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.recv().unwrap();
        }
        self.inner.clone_base(vm)
    }
    fn boot(&self, vm: &VmName, size: VmSize) -> Result<(), VmError> {
        self.inner.boot(vm, size)
    }
    fn wait_ready(&self, vm: &VmName) -> Result<(), VmError> {
        self.inner.wait_ready(vm)
    }
    fn run_command(&self, vm: &VmName, request: &ExecRequest) -> Result<ExecEnd, VmError> {
        self.panic_if(Step::Exec, vm);
        self.inner.run_command(vm, request)
    }
    fn kill(&self, vm: &VmName) {
        self.inner.kill(vm);
    }
    fn collect(&self, vm: &VmName, tail: usize) -> Result<Collected, VmError> {
        self.inner.collect(vm, tail)
    }
    fn stop(&self, vm: &VmName) -> Result<(), VmError> {
        self.inner.stop(vm)
    }
    fn destroy(&self, vm: &VmName) -> Result<(), VmError> {
        self.panic_if(Step::Destroy, vm);
        self.inner.destroy(vm)
    }
    fn leftovers(&self) -> Result<Vec<VmName>, VmError> {
        self.inner.leftovers()
    }
}

/// Asserts that a stop that comes while the clone is still being made keeps the command
/// from running: nothing boots, the run ends killed and the VM is destroyed. Catches a
/// cancel lost because the driver did not know the VM yet (the run would go on to its wall
/// limit).
#[test]
fn a_stop_before_the_clone_exists_keeps_the_command_from_running() {
    let (open, gate) = channel();
    let driver = Arc::new(Wrapped::new(FakeDriver::new()));
    *driver.gate.lock().unwrap() = Some(gate);
    let mut rt = VmRuntime::new(driver.clone());
    let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
    // The clone is held, so the grace passes and the VM (not yet booted) is forced off.
    rt.stop(&id, Duration::ZERO).unwrap();
    open.send(()).unwrap();
    driver.inner.wait_for(&vm("run-1"), Step::Destroy);
    assert_eq!(rt.poll(&id).unwrap(), exited(128 + 9));
    rt.remove(&id).unwrap();
    assert_eq!(
        driver.inner.steps_of(&vm("run-1")),
        [Step::Stop, Step::Clone, Step::Stop, Step::Destroy]
    );
    assert_eq!(rt.slots().free(), VM_SLOTS);
}

/// Asserts that a guest that ignores the kill has its VM forced off once the grace has
/// passed, which ends the command (the run is gone) and lets the VM be destroyed; and that
/// a VM that cannot be forced off makes the stop fail with the driver's reason. Catches a
/// stop that waits for a hung guest for ever, holding the slot and the worker.
#[test]
fn a_guest_that_ignores_the_kill_has_its_vm_forced_off() {
    let driver = Arc::new(FakeDriver::new().deaf());
    let mut rt = runtime(&driver);
    let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
    driver.wait_for(&vm("run-1"), Step::Exec);
    rt.stop(&id, Duration::ZERO).unwrap();
    driver.wait_for(&vm("run-1"), Step::Destroy);
    assert_eq!(rt.poll(&id).unwrap(), Status::Gone);
    rt.remove(&id).unwrap();
    assert_eq!(
        driver.steps_of(&vm("run-1")),
        [
            Step::Clone,
            Step::Boot,
            Step::Ready,
            Step::Exec,
            Step::Stop,
            Step::Stop,
            Step::Destroy
        ]
    );

    let driver = Arc::new(FakeDriver::new().deaf().failing(Step::Stop));
    let mut rt = runtime(&driver);
    let id = rt.create_and_start(&spec("run-2", &["x"])).unwrap();
    driver.wait_for(&vm("run-2"), Step::Exec);
    assert_eq!(
        rt.stop(&id, Duration::ZERO).unwrap_err().to_string(),
        "VM run-2: stop failed: the fake was told to fail"
    );
    driver.finish(&vm("run-2"));
    rt.remove(&id).unwrap();
    assert_eq!(rt.slots().free(), VM_SLOTS);
}

/// Asserts that a driver that panics in a step fails the run (gone) and that its VM is
/// still stopped and destroyed and its slot given back. Catches a panic that leaves a VM
/// running on the host.
#[test]
fn a_driver_that_panics_still_has_its_vm_destroyed() {
    let driver = Arc::new(Wrapped {
        panics_in: Some(Step::Exec),
        ..Wrapped::new(FakeDriver::new())
    });
    let mut rt = VmRuntime::new(driver.clone());
    let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
    driver.inner.wait_for(&vm("run-1"), Step::Destroy);
    assert_eq!(rt.poll(&id).unwrap(), Status::Gone);
    rt.remove(&id).unwrap();
    assert_eq!(
        driver.inner.steps_of(&vm("run-1")),
        [
            Step::Clone,
            Step::Boot,
            Step::Ready,
            Step::Stop,
            Step::Destroy
        ]
    );
    assert_eq!(rt.slots().free(), VM_SLOTS);
}

/// Asserts that a VM whose thread dies in the destroy fails its removal, naming the VM, and
/// gives its slot back. Catches a VM that may be left on the host reported as destroyed.
#[test]
fn a_vm_whose_thread_dies_in_the_destroy_is_not_removed_cleanly() {
    let driver = Arc::new(Wrapped {
        panics_in: Some(Step::Destroy),
        ..Wrapped::new(FakeDriver::new())
    });
    let mut rt = VmRuntime::new(driver.clone());
    let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
    assert_eq!(
        rt.remove(&id).unwrap_err().to_string(),
        "VM run-1: destroy failed: the VM's thread ended before it destroyed the VM"
    );
    assert_eq!(rt.slots().free(), VM_SLOTS);
}

/// Asserts what is refused before a VM is made: a native body, a run id that is no VM name,
/// and a second start of a run that has a VM; each costs no slot and no call. Catches a
/// native body run as a command, and one run with two VMs.
#[test]
fn what_is_refused_before_a_vm_is_made() {
    let driver = Arc::new(FakeDriver::new().holding());
    let mut rt = runtime(&driver);
    let mut native = spec("run-n", &[]);
    native.body = Body::Native(super::super::native::native("/bin/true", &[]));
    assert_eq!(
        rt.create_and_start(&native).unwrap_err().to_string(),
        "a VM runs a command, not a native body"
    );
    let bad = spec("-bad", &["x"]);
    assert!(
        rt.create_and_start(&bad)
            .unwrap_err()
            .to_string()
            .starts_with("\"-bad\" is not a VM name")
    );
    let id = rt.create_and_start(&spec("run-1", &["x"])).unwrap();
    assert_eq!(
        rt.create_and_start(&spec("run-1", &["x"]))
            .unwrap_err()
            .to_string(),
        "run run-1 already has a VM"
    );
    assert_eq!(rt.slots().free(), VM_SLOTS - 1);
    driver.finish(&vm("run-1"));
    rt.remove(&id).unwrap();
    assert_eq!(driver.steps_of(&vm("run-n")), []);
}

/// Asserts what the runtime says of a run it does not hold: gone, no tail, and a stop and a
/// removal that succeed and do nothing; that it lists no run to adopt; and that a VM has no
/// GPU. Catches a restarted agent that adopts a VM (a VM is never reused) and a GPU start
/// sent into a VM.
#[test]
fn a_run_it_does_not_hold_is_gone() {
    let driver = Arc::new(FakeDriver::new());
    let mut rt = runtime(&driver);
    let other = RunHandle("other".to_owned());
    assert_eq!(rt.poll(&other).unwrap(), Status::Gone);
    assert_eq!(rt.log_tail(&other).unwrap(), None);
    rt.stop(&other, GRACE).unwrap();
    rt.remove(&other).unwrap();
    assert!(rt.list_labelled().unwrap().is_empty());
    assert!(!rt.supports_gpus());
    assert!(driver.calls().is_empty());
}

/// Asserts that the VMs an earlier process left are stopped and destroyed when the
/// runtime reclaims, one line each, and that a store that cannot be listed is one line
/// saying so. Catches a restarted agent that keeps a dead run's VM (and its slot) for ever.
#[test]
fn leftover_vms_are_destroyed_when_reclaimed() {
    let driver = Arc::new(
        FakeDriver::new()
            .leaving(&[vm("old-1"), vm("old-2")])
            .failing(Step::Destroy),
    );
    let mut rt = runtime(&driver);
    assert_eq!(
        rt.reclaim(),
        [
            "vm: VM old-1: destroy failed: the fake was told to fail",
            "vm: destroyed the VM old-2 an earlier process left",
        ]
    );
    assert_eq!(driver.steps_of(&vm("old-2")), [Step::Stop, Step::Destroy]);
    assert_eq!(driver.clones(), [vm("old-1")]);

    let broken = Arc::new(FakeDriver::new().unlistable());
    assert_eq!(
        runtime(&broken).reclaim(),
        ["vm: the VM store cannot be listed: the fake was told to fail"]
    );
}
