//! The recording VM driver (`lease_fake`): what it records, how it answers as scripted, and
//! that its waits end a hung test instead of deciding a passing one.

use std::sync::Arc;
use std::time::Duration;

use js_vmm::lease::{Collected, ExecEnd, ExecRequest, Step, VmDriver, VmError, VmName, VmSize};
use js_vmm::lease_fake::{Call, FakeDriver, HANG_GUARD};

fn vm(name: &str) -> VmName {
    VmName::new(name).unwrap()
}

fn request() -> ExecRequest {
    ExecRequest {
        argv: vec!["make".to_owned()],
        env: Vec::new(),
        timeout_ms: 1000,
    }
}

const SIZE: VmSize = VmSize {
    cpus: 2,
    memory_mib: 4096,
};

/// Runs a whole life of `name` on `driver`.
fn whole_life(driver: &FakeDriver, name: &VmName) -> ExecEnd {
    driver.clone_base(name).unwrap();
    driver.boot(name, SIZE).unwrap();
    driver.wait_ready(name).unwrap();
    let end = driver.exec(name, &request()).unwrap();
    driver.collect(name, 16).unwrap();
    driver.stop(name).unwrap();
    driver.destroy(name).unwrap();
    end
}

/// Asserts that every call is recorded in order with what it was given, that each names its
/// VM and step (a kill and a listing have no step), and that by default the command exits 0
/// having written nothing, and that the clone, the live count and the peak follow the
/// calls. Catches a fake whose record a test cannot trust for the order of a VM's life.
#[test]
fn every_call_is_recorded_in_order() {
    let driver = FakeDriver::new();
    let one = vm("run-1");
    assert_eq!(whole_life(&driver, &one), ExecEnd::Exited(0));
    driver.kill(&one);
    assert_eq!(driver.leftovers().unwrap(), []);
    let calls = driver.calls();
    assert_eq!(
        calls,
        [
            Call::Clone(one.clone()),
            Call::Boot(one.clone(), SIZE),
            Call::WaitReady(one.clone()),
            Call::Exec(one.clone(), request()),
            Call::Collect(one.clone(), 16),
            Call::Stop(one.clone()),
            Call::Destroy(one.clone()),
            Call::Kill(one.clone()),
            Call::Leftovers,
        ]
    );
    let steps: Vec<Option<Step>> = calls.iter().map(Call::step).collect();
    assert_eq!(
        steps,
        [
            Some(Step::Clone),
            Some(Step::Boot),
            Some(Step::Ready),
            Some(Step::Exec),
            Some(Step::Collect),
            Some(Step::Stop),
            Some(Step::Destroy),
            None,
            None
        ]
    );
    let vms: Vec<Option<&VmName>> = calls.iter().map(Call::vm).collect();
    assert_eq!(vms[..8], [Some(&one); 8]);
    assert_eq!(vms[8], None);
    assert_eq!(
        driver.steps_of(&one),
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
    assert_eq!((driver.live(), driver.peak()), (0, 1));
    assert!(driver.clones().is_empty());
}

/// Asserts that the scripted end and output are answered, that `collect` keeps the last
/// bytes asked for and says when it cut, and that a second clone of one name is refused.
/// Catches a fake that answers what it was not told.
#[test]
fn it_answers_as_scripted() {
    let driver = FakeDriver::new().answering(ExecEnd::Signaled(15), "abcdef", "xy");
    let one = vm("run-1");
    driver.clone_base(&one).unwrap();
    assert_eq!(
        driver.clone_base(&one),
        Err(VmError::step(&one, Step::Clone, "the clone exists"))
    );
    assert_eq!(driver.exec(&one, &request()), Ok(ExecEnd::Signaled(15)));
    assert_eq!(
        driver.collect(&one, 3),
        Ok(Collected {
            stdout: b"def".to_vec(),
            stderr: b"xy".to_vec(),
            truncated: true,
        })
    );
    assert_eq!(
        driver.collect(&one, 6),
        Ok(Collected {
            stdout: b"abcdef".to_vec(),
            stderr: b"xy".to_vec(),
            truncated: false,
        })
    );
    assert_eq!(driver.clones(), [one]);
}

/// Asserts that a step told to fail fails once, naming the VM and the step, and works the
/// next time; and that a store told to be unlistable says so. Catches a failure that sticks
/// (every later test would fail for it) or that names another step.
#[test]
fn a_step_told_to_fail_fails_once() {
    for step in [
        Step::Clone,
        Step::Boot,
        Step::Ready,
        Step::Exec,
        Step::Collect,
        Step::Stop,
        Step::Destroy,
    ] {
        let driver = FakeDriver::new().failing(step);
        let one = vm("run-1");
        let call = |driver: &FakeDriver| match step {
            Step::Clone => driver.clone_base(&one),
            Step::Boot => driver.boot(&one, SIZE),
            Step::Ready => driver.wait_ready(&one),
            Step::Exec => driver.exec(&one, &request()).map(|_| ()),
            Step::Collect => driver.collect(&one, 1).map(|_| ()),
            Step::Stop => driver.stop(&one),
            Step::Destroy => driver.destroy(&one),
        };
        assert_eq!(
            call(&driver),
            Err(VmError::step(&one, step, "the fake was told to fail"))
        );
        assert_eq!(call(&driver), Ok(()), "{step}");
    }
    assert_eq!(
        FakeDriver::new().unlistable().leftovers(),
        Err(VmError::List("the fake was told to fail".to_owned()))
    );
}

/// Asserts that a held exec waits until it is let end (and then ends as scripted) or the
/// VM is killed (and then ends killed), that a VM killed before its exec ends killed at
/// once, and that the clones an earlier process left are listed. Catches a held exec that
/// ends by itself, which would let a test of two VMs in flight pass with none.
#[test]
fn a_held_exec_ends_when_let_or_killed() {
    let driver = Arc::new(
        FakeDriver::new()
            .holding()
            .answering(ExecEnd::Exited(2), "", "")
            .leaving(&[vm("old")]),
    );
    assert_eq!(driver.leftovers().unwrap(), [vm("old")]);
    for (name, end) in [("let", ExecEnd::Exited(2)), ("killed", ExecEnd::Killed)] {
        let one = vm(name);
        let running = {
            let (driver, one) = (driver.clone(), one.clone());
            std::thread::spawn(move || driver.exec(&one, &request()))
        };
        driver.wait_for(&one, Step::Exec);
        match end {
            ExecEnd::Killed => driver.kill(&one),
            _ => driver.finish(&one),
        }
        assert_eq!(running.join().unwrap(), Ok(end));
    }
    let early = vm("early");
    driver.kill(&early);
    assert_eq!(driver.exec(&early, &request()), Ok(ExecEnd::Killed));
}

/// Asserts that a held exec nobody lets end gives up at the guard with an error, and that the
/// guard is two minutes unless a test sets it. Catches a fake that hangs a test for ever.
#[test]
fn a_held_exec_gives_up_at_the_guard() {
    let driver = FakeDriver::new().holding().guarded(Duration::ZERO);
    let one = vm("run-1");
    assert_eq!(
        driver.exec(&one, &request()),
        Err(VmError::step(
            &one,
            Step::Exec,
            "the held exec was never let end"
        ))
    );
    assert_eq!(HANG_GUARD, Duration::from_secs(120));
}

/// Asserts that a wait for a call that never comes panics at the guard, naming the VM and
/// the step. Catches a wait that returns as if the call had come.
#[test]
#[should_panic(expected = "VM run-1: no exec call came")]
fn a_wait_for_a_call_that_never_comes_panics() {
    let driver = FakeDriver::new().guarded(Duration::ZERO);
    driver.wait_for(&vm("run-1"), Step::Exec);
}

/// Asserts that a deaf guest's held exec ignores a kill and ends, failed, when its VM is
/// stopped, and that a destroy forgets the stop so the name is new again. Catches a fake
/// that cannot stand for a guest that does not answer.
#[test]
fn a_deaf_exec_ignores_the_kill_and_ends_when_the_vm_stops() {
    let driver = Arc::new(FakeDriver::new().deaf());
    let one = vm("run-1");
    let running = {
        let (driver, one) = (driver.clone(), one.clone());
        std::thread::spawn(move || driver.exec(&one, &request()))
    };
    driver.wait_for(&one, Step::Exec);
    driver.kill(&one);
    driver.stop(&one).unwrap();
    assert_eq!(
        running.join().unwrap(),
        Err(VmError::step(&one, Step::Exec, "the VM was stopped"))
    );
    driver.destroy(&one).unwrap();
    driver.finish(&one);
    assert_eq!(driver.exec(&one, &request()), Ok(ExecEnd::Exited(0)));
}
