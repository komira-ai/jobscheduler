//! [`WithVms`]: VM runs go to the VM runtime, everything else to the runtime the agent had.

use std::sync::Arc;
use std::time::Duration;

use js_agent::lifecycle::{Prepared, RunHandle, Runtime, Status, label};
use js_agent::vm::{VmRuntime, WithVms};
use js_agent_wire::{LogReport, LogTail, NetworkLevel, refusal};
use js_vmm::lease::{ExecEnd, Step};
use js_vmm::lease_fake::FakeDriver;

use super::super::lifecycle::fake::{FakeRuntime, Log, ManualClock};
use super::super::native::native;
use super::{ended, spec, vm};

/// A grace far above what a fake guest takes to end its command once killed.
const GRACE: Duration = Duration::from_secs(120);

/// The router over a fresh fake runtime and `driver`, and the fake (a handle on the same
/// state) to look at.
fn split(driver: &Arc<FakeDriver>) -> (WithVms, FakeRuntime) {
    let fake = FakeRuntime::new(&Log::default(), &ManualClock::default());
    let with = WithVms::new(VmRuntime::new(driver.clone()), Box::new(fake.clone()));
    (with, fake)
}

/// Asserts that a run with the VM kind is made by the VM runtime and one without it by the
/// other runtime, and that each later call about a run (poll, tail, log report, control
/// group, stop, removal) goes to the runtime that made it. Catches a VM start run as a
/// native job on the host, and a container's calls sent to the VM runtime.
#[test]
fn each_run_goes_to_the_runtime_of_its_kind() {
    let driver = Arc::new(FakeDriver::new().answering(ExecEnd::Exited(2), "vm out", ""));
    let (mut with, fake) = split(&driver);
    let in_vm = with.create_and_start(&spec("run-vm", &["make"])).unwrap();
    let mut plain = spec("run-plain", &["make"]);
    plain.labels.remove(label::KIND);
    let on_host = with.create_and_start(&plain).unwrap();
    assert_eq!(in_vm, RunHandle("run-vm".to_owned()));
    assert_eq!(
        fake.created()
            .iter()
            .map(|run| &run.run_id)
            .collect::<Vec<_>>(),
        ["run-plain"]
    );
    assert_eq!(driver.steps_of(&vm("run-plain")), []);

    assert_eq!(ended(&mut with, &driver, &in_vm), super::runtime_exit(2));
    let tail = with.log_tail(&in_vm).unwrap().unwrap();
    assert_eq!(tail.stdout, "vm out");
    assert_eq!(with.logs_report(&in_vm), None);
    assert_eq!(with.cgroup(&in_vm), None);
    with.stop(&in_vm, GRACE).unwrap();
    with.remove(&in_vm).unwrap();
    assert_eq!(driver.steps_of(&vm("run-vm")).last(), Some(&Step::Destroy));

    fake.print(
        "run-plain",
        LogTail {
            stdout: "host out".to_owned(),
            ..LogTail::default()
        },
    );
    let report = LogReport {
        stdout_bytes: 8,
        stderr_bytes: 0,
        stdout_dropped: 0,
        stderr_dropped: 0,
    };
    fake.report_logs("run-plain", report);
    fake.set_cgroup("run-plain", "/sys/fs/cgroup/run-plain");
    fake.exit("run-plain", 0);
    assert!(matches!(with.poll(&on_host).unwrap(), Status::Exited(_)));
    assert_eq!(with.log_tail(&on_host).unwrap().unwrap().stdout, "host out");
    assert_eq!(with.logs_report(&on_host), Some(report));
    assert!(with.cgroup(&on_host).is_some());
    with.stop(&on_host, GRACE).unwrap();
    assert_eq!(fake.stops().len(), 1);
    with.remove(&on_host).unwrap();
    assert!(fake.gone("run-plain"));
}

/// Asserts that every question about the host is the other runtime's answer (the image
/// fetch, the listing, the checks of a body, the network ceiling and egress, the lease
/// directory, the control groups, the GPUs), that the VM check is the VM runtime's, and that
/// a reclaim does the other runtime's and then the VMs'. Catches a router that answers a
/// host question with the VM runtime's default and so changes what the agent runs.
#[test]
fn every_host_question_is_the_other_runtimes() {
    let driver = Arc::new(FakeDriver::new().leaving(&[vm("old")]));
    let (mut with, fake) = split(&driver);
    let mut alone = fake.clone();
    assert_eq!(with.prepare("img", None), alone.prepare("img", None));
    assert_eq!(with.prepare("img", None), Prepared::Ready);
    let leaked = fake.leak(&[(label::RUN, "left")], 5);
    assert_eq!(
        with.list_labelled().unwrap(),
        alone.list_labelled().unwrap()
    );
    assert_eq!(
        with.list_ids().unwrap(),
        [RunHandle(leaked)],
        "the other runtime's containers, and no VM"
    );
    let body = native("/bin/true", &[]);
    assert_eq!(with.check_native(&body), alone.check_native(&body));
    assert_eq!(with.check_env(&[]), alone.check_env(&[]));
    assert_eq!(with.check_image(), alone.check_image());
    assert_eq!(with.runs_image_runs(), alone.runs_image_runs());
    assert_eq!(with.supports_gpus(), alone.supports_gpus());
    assert_eq!(with.check_egress(), alone.check_egress());
    assert_eq!(with.network_ceiling(), alone.network_ceiling());
    let mut open = native("/bin/true", &[]);
    open.network = NetworkLevel::Open;
    assert_eq!(
        with.check_native_egress(&open),
        alone.check_native_egress(&open)
    );
    assert_eq!(with.lease_dir("run-1"), alone.lease_dir("run-1"));
    assert_eq!(with.places_in_cgroup(), alone.places_in_cgroup());
    assert_eq!(with.check_vm(), Ok(()));
    assert_eq!(
        alone.check_vm().unwrap_err().code,
        refusal::VM_UNAVAILABLE,
        "a runtime without VMs runs none"
    );
    assert_eq!(
        with.reclaim(),
        ["vm: destroyed the VM old an earlier process left"]
    );
}
