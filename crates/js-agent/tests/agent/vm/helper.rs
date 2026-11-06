//! [`HelperDriver`]: the agent's real driver, which drives one `js-vmm lease` process for each
//! VM over the line protocol. Here the helper is the protocol's own service over the recording
//! driver on a thread (or a scripted one, or a program that is no helper), on an anonymous
//! pair of Unix sockets.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_agent::vm::{HelperDriver, HelperLaunch, Launch, Launched};
use js_vm_lease::fake::FakeDriver;
use js_vm_lease::protocol::{Reply, Session, serve};
use js_vm_lease::{Collected, ExecEnd, ExecRequest, Step, VmDriver, VmError, VmSize};

use super::vm;
use crate::support::Scratch;

const SIZE: VmSize = VmSize {
    cpus: 2,
    memory_mib: 4096,
};

fn request() -> ExecRequest {
    ExecRequest {
        argv: vec!["make".to_owned()],
        env: Vec::new(),
        timeout_ms: 1000,
    }
}

/// A session to a helper that `answer` plays on a thread, on a new socket pair.
fn session_to(answer: impl FnOnce(UnixStream) + Send + 'static) -> Session {
    let (agent, helper) = UnixStream::pair().unwrap();
    std::thread::spawn(move || answer(helper));
    let requests = Box::new(agent.try_clone().unwrap());
    Session::new(requests, Box::new(BufReader::new(agent)))
}

/// Helpers that are the protocol's service over one recording driver, counted.
struct Served {
    driver: Arc<FakeDriver>,
    launches: Arc<AtomicUsize>,
}

impl Launch for Served {
    fn launch(&self) -> io::Result<Launched> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        let driver = Arc::clone(&self.driver);
        let session = session_to(move |helper| {
            let mut input = BufReader::new(helper.try_clone().unwrap());
            let mut output = helper;
            let _ = serve(&*driver, &mut input, &mut output);
        });
        Ok(Launched {
            session,
            child: None,
        })
    }
}

/// A driver whose helpers serve `driver`, and the count of the helpers started.
fn served(driver: &Arc<FakeDriver>) -> (HelperDriver, Arc<AtomicUsize>) {
    let launches = Arc::new(AtomicUsize::new(0));
    let launch = Served {
        driver: Arc::clone(driver),
        launches: Arc::clone(&launches),
    };
    (HelperDriver::new(Box::new(launch)), launches)
}

/// Asserts that each call of a VM's life reaches the driver behind the helper and comes back
/// with what it said (the command's end, the output, the driver's own error), that one helper
/// serves one VM from its clone to its destroy, and that a VM made again after its destroy
/// gets a new helper. Catches a call lost on the way to the helper, an error of the driver
/// replaced by another, and a helper left running after its VM.
#[test]
fn a_vms_life_goes_through_one_helper_of_its_own() {
    let driver = Arc::new(
        FakeDriver::new()
            .answering(ExecEnd::Exited(3), "out", "err")
            .failing(Step::Ready),
    );
    let (helpers, launches) = served(&driver);
    let one = vm("run-1");
    helpers.clone_base(&one).unwrap();
    helpers.boot(&one, SIZE).unwrap();
    assert_eq!(
        helpers.wait_ready(&one),
        Err(VmError::step(
            &one,
            Step::Ready,
            "the fake was told to fail"
        ))
    );
    helpers.wait_ready(&one).unwrap();
    assert_eq!(
        helpers.run_command(&one, &request()),
        Ok(ExecEnd::Exited(3))
    );
    assert_eq!(
        helpers.collect(&one, 16),
        Ok(Collected {
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
            truncated: false,
        })
    );
    helpers.stop(&one).unwrap();
    helpers.destroy(&one).unwrap();
    assert_eq!(
        driver.steps_of(&one),
        [
            Step::Clone,
            Step::Boot,
            Step::Ready,
            Step::Ready,
            Step::Exec,
            Step::Collect,
            Step::Stop,
            Step::Destroy
        ]
    );
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    helpers.clone_base(&one).unwrap();
    assert_eq!(launches.load(Ordering::SeqCst), 2);
}

/// Asserts that the driver's error of a command and of a collect come back as they were.
/// Catches an error of the command read as an end.
#[test]
fn the_drivers_errors_of_a_command_and_a_collect_come_back() {
    for step in [Step::Exec, Step::Collect] {
        let driver = Arc::new(FakeDriver::new().failing(step));
        let (helpers, _) = served(&driver);
        let one = vm("run-1");
        let failed = VmError::step(&one, step, "the fake was told to fail");
        let got = match step {
            Step::Exec => helpers.run_command(&one, &request()).map(|_| ()),
            _ => helpers.collect(&one, 1).map(|_| ()),
        };
        assert_eq!(got, Err(failed));
    }
}

/// Asserts that a kill reaches a command the helper is running and ends it, and that a kill
/// of a VM that has no helper yet starts none. Catches a stop that waits for the command it
/// is meant to end, and a helper started only to be killed.
#[test]
fn a_kill_reaches_the_running_command() {
    let driver = Arc::new(FakeDriver::new().holding());
    let (helpers, launches) = served(&driver);
    let helpers = Arc::new(helpers);
    let one = vm("run-1");
    helpers.kill(&one);
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    helpers.clone_base(&one).unwrap();
    let running = {
        let (helpers, one) = (Arc::clone(&helpers), one.clone());
        std::thread::spawn(move || helpers.run_command(&one, &request()))
    };
    driver.wait_for(&one, Step::Exec);
    helpers.kill(&one);
    assert_eq!(running.join().unwrap(), Ok(ExecEnd::Killed));
    helpers.destroy(&one).unwrap();
}

/// Asserts that the leftovers are asked of a helper of their own (the names, the driver's
/// error), and that a VM's helper outlives a destroy that failed only until it is ended.
/// Catches leftovers that a restarted agent never finds.
#[test]
fn leftovers_are_asked_of_a_helper_of_their_own() {
    let driver = Arc::new(FakeDriver::new().leaving(&[vm("old-1"), vm("old-2")]));
    let (helpers, launches) = served(&driver);
    assert_eq!(helpers.leftovers(), Ok(vec![vm("old-1"), vm("old-2")]));
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    let broken = Arc::new(FakeDriver::new().unlistable());
    let (helpers, _) = served(&broken);
    assert_eq!(
        helpers.leftovers(),
        Err(VmError::List("the fake was told to fail".to_owned()))
    );
    let failing = Arc::new(FakeDriver::new().failing(Step::Destroy));
    let (helpers, launches) = served(&failing);
    assert_eq!(
        helpers.destroy(&vm("run-1")),
        Err(VmError::step(
            &vm("run-1"),
            Step::Destroy,
            "the fake was told to fail"
        ))
    );
    helpers.destroy(&vm("run-1")).unwrap();
    assert_eq!(
        launches.load(Ordering::SeqCst),
        2,
        "a destroy ends its helper"
    );
}

/// Helpers that answer every request with one reply.
struct Answering(Reply);

impl Launch for Answering {
    fn launch(&self) -> io::Result<Launched> {
        let reply = serde_json::to_string(&self.0).unwrap();
        let session = session_to(move |helper| {
            let mut output = helper.try_clone().unwrap();
            for _ in BufReader::new(helper).lines() {
                writeln!(output, "{reply}").unwrap();
            }
        });
        Ok(Launched {
            session,
            child: None,
        })
    }
}

/// Asserts that a reply that is not the one a call expects is an error of that call that
/// names the reply, for every call. Catches a helper of another version whose answer is read
/// as success.
#[test]
fn a_reply_out_of_turn_is_an_error_of_the_call() {
    let helpers = HelperDriver::new(Box::new(Answering(Reply::Names { names: Vec::new() })));
    let one = vm("run-1");
    let said = |step: Step| VmError::step(&one, step, "the VM helper answered Names { names: [] }");
    assert_eq!(helpers.clone_base(&one), Err(said(Step::Clone)));
    assert_eq!(helpers.boot(&one, SIZE), Err(said(Step::Boot)));
    assert_eq!(helpers.wait_ready(&one), Err(said(Step::Ready)));
    assert_eq!(helpers.run_command(&one, &request()), Err(said(Step::Exec)));
    assert_eq!(helpers.collect(&one, 1), Err(said(Step::Collect)));
    assert_eq!(helpers.stop(&one), Err(said(Step::Stop)));
    assert_eq!(helpers.destroy(&one), Err(said(Step::Destroy)));
    let helpers = HelperDriver::new(Box::new(Answering(Reply::Done)));
    assert_eq!(
        helpers.leftovers(),
        Err(VmError::List("the VM helper answered Done".to_owned()))
    );
}

/// Asserts that the real launcher starts `<driver> lease --store <store>` through the spawner:
/// a driver that is not there is an error of the first call (the helper did not start), and
/// one that ends at once (no helper at all) is an error of the call that names the helper,
/// for the VM and for the leftovers alike; the ended program is reaped. Catches an agent that
/// reads a missing or broken `js-vmm` as a VM that answered.
#[test]
fn the_real_launcher_starts_the_driver_and_reports_one_that_is_no_helper() {
    let dir = Scratch::new("vm-helper-launch");
    let store = dir.path("store");
    let missing = HelperDriver::new(Box::new(HelperLaunch::new(
        dir.path("no-js-vmm"),
        store.clone(),
    )));
    let one = vm("run-1");
    let error = missing.clone_base(&one).unwrap_err().to_string();
    assert!(
        error.starts_with("VM run-1: clone failed: the VM helper did not start: "),
        "{error}"
    );
    assert!(missing.destroy(&one).is_err());
    let listed = missing.leftovers().unwrap_err().to_string();
    assert!(
        listed.starts_with("the VM store cannot be listed: the VM helper did not start: "),
        "{listed}"
    );

    let quits = dir.write("quits", "#!/bin/sh\nexit 0\n", 0o755);
    let quitting = HelperDriver::new(Box::new(HelperLaunch::new(quits, store)));
    let error = quitting.clone_base(&one).unwrap_err().to_string();
    assert!(
        error.starts_with("VM run-1: clone failed: the VM helper: "),
        "{error}"
    );
    assert!(quitting.destroy(&one).is_err());
    let listed = quitting.leftovers().unwrap_err().to_string();
    assert!(
        listed.starts_with("the VM store cannot be listed: the VM helper: "),
        "{listed}"
    );
}
