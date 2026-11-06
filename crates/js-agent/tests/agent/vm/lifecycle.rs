//! VM starts through the whole lifecycle: what is refused at admission with the backend off
//! and on, and a VM job's run from its start to its result beside an action job.

use std::sync::Arc;

use js_agent::lifecycle::{Event, Lifecycle, NoChecks, Runtime, detail, label};
use js_agent::vm::{NoVmSlot, VmRuntime, WithVms};
use js_agent_wire::{Command, HeldJob, JobStartSpec, Outcome, refusal};
use js_vm_lease::fake::FakeDriver;
use js_vm_lease::{ExecEnd, Step};
use serde_json::{Value, json};

use super::super::lifecycle::fake::{FakeRuntime, Log, ManualClock};
use super::super::lifecycle::{config_in, names, reply};
use super::vm;
use crate::support::Scratch;

/// A lifecycle over a fake runtime, with the VM runtime of `driver` beside it when there is
/// one (the backend on), and the fake runtime to look at.
fn life(dir: &Scratch, driver: Option<&Arc<FakeDriver>>) -> (Lifecycle, FakeRuntime) {
    let fake = FakeRuntime::new(&Log::default(), &ManualClock::default());
    let runtime: Box<dyn Runtime> = match driver {
        Some(driver) => Box::new(WithVms::new(
            VmRuntime::new(driver.clone()),
            Box::new(fake.clone()),
        )),
        None => Box::new(fake.clone()),
    };
    let clock = ManualClock::default();
    let config = config_in(dir, false);
    let (life, _) = Lifecycle::adopt(config, runtime, Box::new(NoChecks), Box::new(clock)).unwrap();
    (life, fake)
}

/// A job start of `job_id`, as the server's JSON says it, with `extra` over it.
fn start_of(job_id: &str, extra: &Value) -> Command {
    let mut body = json!({
        "job_id": job_id,
        "allocation_id": format!("alloc-{job_id}"),
        "image": "macos:15@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "command": ["xcodebuild"],
        "booked": { "cpu_millis": 1000, "memory_bytes": 1u64 << 30 },
        "wall_ms": 60_000,
        "kind": "vm"
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    let spec: JobStartSpec = serde_json::from_value(body).unwrap();
    Command::JobStart(Box::new(spec))
}

/// A VM job start of `job_id`.
fn vm_start(job_id: &str) -> Command {
    start_of(job_id, &json!({}))
}

/// The listing of `job_id`.
fn listing(life: &Lifecycle, job_id: &str) -> HeldJob {
    let jobs = life.held_jobs().into_iter();
    jobs.into_iter().find(|job| job.job_id == job_id).unwrap()
}

/// Steps the lifecycle until `job_id` has a result, once its VM's thread has reported the
/// end (the driver was asked to stop the VM); the cleanup after it takes a bounded number
/// of steps.
fn result_of(life: &mut Lifecycle, driver: &FakeDriver, job_id: &str) -> js_agent_wire::JobResult {
    driver.wait_for(&vm(job_id), Step::Stop);
    for _ in 0..8 {
        life.step();
        if let Some(result) = listing(life, job_id).result {
            return result;
        }
    }
    panic!("{job_id} has no result");
}

/// The refusal `job_id` was answered with.
fn refused(events: &[Event], job_id: &str) -> (&'static str, String) {
    let found = events.iter().find_map(|event| match event {
        Event::Refused { run_id, code, why } if run_id == job_id => Some((*code, why.clone())),
        _ => None,
    });
    found.unwrap_or_else(|| panic!("{job_id} was not refused: {:?}", names(events)))
}

/// Asserts that an agent without the VM backend answers a VM start `failed_to_start` with
/// `vm-unavailable` and creates nothing, while an action start beside it runs. Catches a VM
/// start run on the host as an action by an agent that runs no VMs.
#[test]
fn without_the_backend_a_vm_start_is_refused_vm_unavailable() {
    let dir = Scratch::new("vm-life-off");
    let (mut life, fake) = life(&dir, None);
    let events = life.apply(&reply(vec![
        vm_start("job-vm"),
        start_of("job-host", &json!({ "kind": "action" })),
    ]));
    assert_eq!(refused(&events, "job-vm").0, refusal::VM_UNAVAILABLE);
    let result = listing(&life, "job-vm").result.unwrap();
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::VM_UNAVAILABLE));
    let created: Vec<String> = fake.created().into_iter().map(|run| run.run_id).collect();
    assert_eq!(created, ["job-host"]);
}

/// Asserts that with the backend on a VM job runs in a VM (cloned, booted, run, destroyed)
/// and ends with the guest's status and output, while an action job beside it is made by
/// the other runtime without the VM label. Catches a VM job sent to the host runtime and an
/// action job sent into a VM.
#[test]
fn a_vm_job_runs_in_a_vm_beside_an_action_job() {
    let dir = Scratch::new("vm-life-on");
    let driver = Arc::new(FakeDriver::new().answering(ExecEnd::Exited(4), "built", "warn"));
    let (mut life, fake) = life(&dir, Some(&driver));
    let events = life.apply(&reply(vec![
        vm_start("job-vm"),
        start_of("job-host", &json!({ "kind": "action" })),
    ]));
    assert!(
        names(&events).contains(&"started job-vm".to_owned()),
        "{:?}",
        names(&events)
    );
    let created = fake.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].run_id, "job-host");
    assert_eq!(created[0].labels.get(label::KIND), None);

    let result = result_of(&mut life, &driver, "job-vm");
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.exit_code, Some(4));
    let tail = result.log_tail.unwrap();
    assert_eq!(
        (tail.stdout.as_str(), tail.stderr.as_str()),
        ("built", "warn")
    );
    assert_eq!(
        driver.steps_of(&vm("job-vm")),
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
    assert_eq!(driver.live(), 0);
}

/// Asserts that a VM start that asks for what a VM does not give is refused before anything
/// is made, each with its own code: a `run` body, run files, a GPU, network above `none`, a
/// declared port. Catches a VM start run with less than it asked for.
#[test]
fn a_vm_start_that_asks_for_what_a_vm_lacks_is_refused() {
    let cases = [
        (
            json!({ "command": [], "run": { "program": "/bin/true" } }),
            refusal::RUN_UNSUPPORTED,
        ),
        (
            json!({ "files": [{ "name": "grant", "contents": "x" }] }),
            detail::FILES_UNSUPPORTED,
        ),
        (
            json!({ "booked": { "cpu_millis": 1000, "memory_bytes": 1u64 << 30, "gpu_indices": [0] } }),
            detail::GPU_UNSUPPORTED,
        ),
        (json!({ "network": "open" }), refusal::NETWORK_ABOVE_CEILING),
        (
            json!({ "ports": [{ "host": 8080, "container": 80 }] }),
            refusal::PORTS_NOT_SUPPORTED,
        ),
    ];
    for (n, (extra, code)) in cases.into_iter().enumerate() {
        let dir = Scratch::new(&format!("vm-life-lacks{n}"));
        let driver = Arc::new(FakeDriver::new());
        let (mut life, fake) = life(&dir, Some(&driver));
        let events = life.apply(&reply(vec![start_of("job-vm", &extra)]));
        assert_eq!(refused(&events, "job-vm").0, code, "{extra}");
        assert!(driver.calls().is_empty(), "{extra}");
        assert!(fake.created().is_empty(), "{extra}");
    }
}

/// Asserts that with two VM jobs running a third VM start is refused at admission
/// `no-capacity`, saying why, and creates nothing, though the host's CPUs and memory would
/// fit it; and that once one of the two has ended a new VM start runs. Catches a third VM
/// queued or made, and a slot that stays held after its job ends.
#[test]
fn a_third_vm_start_is_refused_no_capacity() {
    let dir = Scratch::new("vm-life-third");
    let driver = Arc::new(FakeDriver::new().holding());
    let (mut life, _fake) = life(&dir, Some(&driver));
    let events = life.apply(&reply(vec![
        vm_start("job-a"),
        vm_start("job-b"),
        vm_start("job-c"),
    ]));
    assert_eq!(
        refused(&events, "job-c"),
        (refusal::NO_CAPACITY, NoVmSlot.to_string())
    );
    driver.wait_for(&vm("job-a"), Step::Exec);
    driver.wait_for(&vm("job-b"), Step::Exec);
    assert!(driver.steps_of(&vm("job-c")).is_empty());

    driver.finish(&vm("job-a"));
    assert_eq!(
        result_of(&mut life, &driver, "job-a").outcome,
        Outcome::Succeeded
    );
    let events = life.apply(&reply(vec![vm_start("job-d")]));
    assert!(
        names(&events).contains(&"started job-d".to_owned()),
        "{:?}",
        names(&events)
    );
    for job in ["job-b", "job-d"] {
        driver.finish(&vm(job));
        assert_eq!(
            result_of(&mut life, &driver, job).outcome,
            Outcome::Succeeded
        );
    }
    assert_eq!(driver.peak(), 2);
    assert_eq!(driver.live(), 0);
}

/// Asserts that a job stop of a running VM job kills its command, ends the job `killed`,
/// and destroys its VM. Catches a cancel that leaves the VM running until its wall limit.
#[test]
fn a_stopped_vm_job_ends_killed_and_its_vm_is_destroyed() {
    let dir = Scratch::new("vm-life-stop");
    let driver = Arc::new(FakeDriver::new().holding());
    let (mut life, _fake) = life(&dir, Some(&driver));
    life.apply(&reply(vec![vm_start("job-vm")]));
    driver.wait_for(&vm("job-vm"), Step::Exec);
    life.apply(&reply(vec![Command::JobStop {
        job_id: "job-vm".to_owned(),
    }]));
    life.step();
    let result = result_of(&mut life, &driver, "job-vm");
    assert_eq!(result.outcome, Outcome::Killed);
    assert_eq!(driver.live(), 0);
    assert_eq!(driver.steps_of(&vm("job-vm")).last(), Some(&Step::Destroy));
}
