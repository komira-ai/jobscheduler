//! [`WithVms`]: VM runs go to the VM runtime, everything else to the runtime the agent had.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_agent::lifecycle::{
    Created, FsPrepared, JobFs, Prepared, Removal, RunHandle, Runtime, Status, Wake, Whereabouts,
    label,
};
use js_agent::vm::{VmRuntime, WithVms};
use js_agent_wire::{JobUsage, LogReport, LogTail, NativeSpec, NetworkLevel, PortRange, refusal};
use js_vm_lease::fake::FakeDriver;
use js_vm_lease::{ExecEnd, Step};

use super::super::lifecycle::deferring::{Deferring, lease};
use super::super::lifecycle::fake::{FakeRuntime, Log, ManualClock};
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
/// group, where its container is, stop, removal) goes to the runtime that made it: a running
/// VM run of a router whose other runtime places its containers in groups has no container
/// process to be blind to, and a container's first process outside its group is the other
/// runtime's answer. Catches a VM start run as a native job on the host, a container's calls
/// sent to the VM runtime, and a VM run that would taint the host for lacking a group.
#[test]
fn each_run_goes_to_the_runtime_of_its_kind() {
    let driver = Arc::new(FakeDriver::new().answering(ExecEnd::Exited(2), "vm out", ""));
    let (mut with, fake) = split(&driver);
    let in_vm = with.create_and_start(&spec("run-vm", &["make"])).unwrap();
    let mut plain = spec("run-plain", &["make"]);
    plain.labels.remove(label::KIND);
    let on_host = with.create_and_start(&plain).unwrap();
    assert_eq!(in_vm, RunHandle("run-vm".to_owned()));
    fake.promise_cgroups(true);
    assert!(with.places_in_cgroup());
    assert_eq!(with.cgroup(&in_vm), None);
    assert_eq!(with.whereabouts(&in_vm), Whereabouts::NoProcess);
    fake.run_elsewhere("run-plain", "/actions/other");
    assert_eq!(
        with.whereabouts(&on_host),
        Whereabouts::Elsewhere("/actions/other".to_owned())
    );
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
    let counted = JobUsage {
        peak_memory_bytes: 7,
        cpu_usage_usec: 9,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak: None,
        peak_swap_bytes: None,
    };
    fake.report_usage("run-plain", counted);
    fake.set_cgroup("run-plain", "/sys/fs/cgroup/run-plain");
    fake.exit("run-plain", 0);
    assert!(matches!(with.poll(&on_host).unwrap(), Status::Exited(_)));
    assert_eq!(with.log_tail(&on_host).unwrap().unwrap().stdout, "host out");
    assert_eq!(with.logs_report(&on_host), Some(report));
    assert_eq!(with.usage(&on_host), Some(counted));
    assert_eq!(with.usage(&in_vm), None);
    assert!(with.cgroup(&on_host).is_some());
    with.stop(&on_host, GRACE).unwrap();
    assert_eq!(fake.stops().len(), 1);
    with.remove(&on_host).unwrap();
    assert!(fake.gone("run-plain"));
}

/// Asserts that a removal is asked of the runtime that holds the run: a container's of the
/// other runtime, whose answer the router passes on (pending while that runtime's gate holds
/// it, then done, the container gone), and a VM run's of the VM runtime, which destroys the
/// VM at once. Catches a router that keeps the trait's default and removes a container on
/// the caller's thread, and a VM run's removal sent to the other runtime.
#[test]
fn a_removal_is_asked_of_the_runtime_that_holds_the_run() {
    let driver = Arc::new(FakeDriver::new().answering(ExecEnd::Exited(2), "vm out", ""));
    let fake = FakeRuntime::new(&Log::default(), &ManualClock::default());
    let (other, gate) = Deferring::around(fake.clone());
    let mut with = WithVms::new(VmRuntime::new(driver.clone()), Box::new(other));
    let in_vm = with.create_and_start(&spec("run-vm", &["make"])).unwrap();
    let mut plain = spec("run-plain", &["make"]);
    plain.labels.remove(label::KIND);
    let on_host = with.create_and_start(&plain).unwrap();

    gate.hold_removal(&on_host.0);
    assert_eq!(with.removing(&on_host), Removal::Pending);
    assert_eq!(gate.removal_asks(&on_host.0), 1);
    assert!(!fake.gone("run-plain"));
    gate.release_removal(&on_host.0);
    assert_eq!(with.removing(&on_host), Removal::Done(Ok(())));
    assert!(fake.gone("run-plain"));

    assert_eq!(ended(&mut with, &driver, &in_vm), super::runtime_exit(2));
    assert_eq!(with.removing(&in_vm), Removal::Done(Ok(())));
    assert_eq!(driver.steps_of(&vm("run-vm")).last(), Some(&Step::Destroy));
    assert_eq!(gate.removal_asks(&in_vm.0), 0);
}

/// Asserts that the create of a VM run is begun by the VM runtime, which answers at once
/// with its handle, and the create of a run of another kind by the other runtime, so that a
/// create the other runtime holds pending is answered pending by the router's `poll_create`
/// until it is released, and then with the container's handle; a run the other runtime holds
/// no create of is answered a failure naming it. `creating` names the held create's lease by
/// the other runtime's answer until it is answered, and no VM run, while the wrapped fake,
/// which keeps the trait's default, names no run. Catches a VM start begun on the host
/// runtime, a container's create begun on the VM runtime, a poll or a `creating` that is not
/// forwarded to the runtime that holds the create, and a default that names a run nothing is
/// making.
#[test]
fn a_create_is_begun_by_the_runtime_of_its_kind_and_polled_on_the_other() {
    let driver = Arc::new(FakeDriver::new());
    let fake = FakeRuntime::new(&Log::default(), &ManualClock::default());
    let (deferring, gate) = Deferring::around(fake.clone());
    let mut with = WithVms::new(VmRuntime::new(driver), Box::new(deferring));
    gate.hold("run-plain");
    let begun = with.begin_create(&spec("run-vm", &["make"]));
    assert_eq!(begun, Created::Done(Ok(RunHandle("run-vm".to_owned()))));
    assert!(gate.begun().is_empty());
    let mut plain = spec("run-plain", &["make"]);
    plain.labels.remove(label::KIND);
    assert_eq!(with.begin_create(&plain), Created::Pending);
    assert_eq!(gate.begun(), ["run-plain"]);
    assert_eq!(with.poll_create("run-plain"), Created::Pending);
    assert_eq!(
        with.creating("run-plain"),
        Some(RunHandle(lease("run-plain"))),
        "the other runtime names the create it holds"
    );
    assert_eq!(with.creating("run-vm"), None);
    assert_eq!(
        fake.creating("run-plain"),
        None,
        "a runtime with the trait's default, whose creates answer at once, names none"
    );
    assert!(fake.created().is_empty());
    gate.release("run-plain");
    assert!(matches!(
        with.poll_create("run-plain"),
        Created::Done(Ok(_))
    ));
    assert_eq!(with.creating("run-plain"), None, "answered");
    assert_eq!(fake.created().len(), 1);
    let Created::Done(Err(why)) = with.poll_create("run-plain") else {
        panic!("the create was answered");
    };
    assert_eq!(why.to_string(), "no create of run-plain is under way");
}

/// Asserts that every question about the host is the other runtime's answer (the image
/// fetch, the listing, the checks of a body, the network ceiling and egress, the host ports
/// allowed and lost, the lease
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
        [RunHandle(leaked.clone())],
        "the other runtime's containers, and no VM"
    );
    let body = native("/bin/true", &[]);
    assert_eq!(with.check_native(&body), alone.check_native(&body));
    assert_eq!(with.check_env(&[]), alone.check_env(&[]));
    assert_eq!(with.check_xcode("16C5032a"), alone.check_xcode("16C5032a"));
    assert_eq!(with.check_image(), alone.check_image());
    assert_eq!(with.runs_image_runs(), alone.runs_image_runs());
    assert_eq!(with.supports_gpus(), alone.supports_gpus());
    assert_eq!(with.check_egress(), alone.check_egress());
    assert_eq!(with.network_ceiling(), alone.network_ceiling());
    // The wake is the other runtime's to ring, and a start's steps are its runtime's.
    let rings = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = Arc::clone(&rings);
    with.set_wake(Wake::new(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    fake.ring_wake();
    assert_eq!(rings.load(std::sync::atomic::Ordering::SeqCst), 1);
    fake.steps_of("left", &[("begin", 7)]);
    let steps = with.start_steps(&RunHandle(leaked.clone()));
    assert_eq!(steps, [("begin", 7)]);
    fake.set_host_ports(&[PortRange::new(5432, 5433).unwrap()]);
    assert_eq!(with.host_ports(), [PortRange::new(5432, 5433).unwrap()]);
    fake.lose_host_ports("left", "taken");
    let lost = with.host_port_lost(&RunHandle(leaked.clone()));
    assert_eq!(lost.as_deref(), Some("taken"));
    let mut open = native("/bin/true", &[]);
    open.network = NetworkLevel::Open;
    assert_eq!(
        with.check_native_egress(&open),
        alone.check_native_egress(&open)
    );
    assert_eq!(with.lease_dir("run-1"), alone.lease_dir("run-1"));
    fake.keep_tmp_dirs_in(Path::new("/scratch"));
    assert_eq!(
        with.tmp_dir("run-1"),
        Some(PathBuf::from("/scratch/.tmp-fake-run-1"))
    );
    // A job's layers and mounts are the other runtime's to give and to prepare.
    fake.give_layers();
    fake.mount_in(Path::new("/mounts"));
    fake.refuse_layer_image("registry.example/l@sha256:0");
    assert!(with.layered_rootfs());
    assert_eq!(
        with.check_layer_image("registry.example/l@sha256:0"),
        alone.check_layer_image("registry.example/l@sha256:0")
    );
    assert!(!with.binds_named_mounts());
    let fs = JobFs::default();
    fake.resolve_answers("run-1", FsPrepared::Pending);
    assert_eq!(with.resolve_fs("run-1", "", &fs, 0), FsPrepared::Pending);
    assert_eq!(with.assemble_fs("run-1", 0), FsPrepared::Ready);
    assert_eq!(with.discard_fs("run-1"), Ok(()));
    assert_eq!(
        with.mounts_of("run-1"),
        Some(PathBuf::from("/mounts/run-1"))
    );
    assert_eq!(with.input_failure(&RunHandle(leaked.clone())), None);
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

/// A native body that runs `program` with `args`, with no network and nothing else.
fn native(program: &str, args: &[&str]) -> NativeSpec {
    NativeSpec {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: std::collections::BTreeMap::new(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    }
}
