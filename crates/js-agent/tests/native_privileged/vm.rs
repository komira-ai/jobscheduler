//! The VM runtime for real on a Mac: a VM cloned from a real base image, booted through
//! the signed `js-vmm` helper, a command run in it through the guest agent, its output
//! collected, the VM destroyed. Part of the `native_privileged` binary, so it is built only
//! by the manual `js-agent-native-privileged-tests` workflow and run by hand.
//!
//! Its test is `#[ignore]` like the others there. Beside what they need, it needs, under
//! the directory `JS_NATIVE_PRIV_ROOT` names: `js-vmm`, the `js-vmm` binary signed with the
//! virtualization entitlement, and `vm-store/base`, a base image (`js-vmm image base`)
//! whose guest runs the guest agent as the VM driver's module documentation requires
//! (`js-vmm`'s `lease_system`). It boots one VM and needs no network.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status, label};
use js_agent::vm::{HelperDriver, HelperLaunch, VmRuntime};
use js_agent_wire::Isolation;

/// What the runtime says of `id` once it ends, polled with a pause between polls: a real
/// VM takes its own time, and nothing here is timed.
fn settle(rt: &mut VmRuntime, id: &RunHandle) -> Status {
    loop {
        match rt.poll(id).unwrap() {
            Status::Running => std::thread::park_timeout(Duration::from_millis(200)),
            ended => return ended,
        }
    }
}

/// Asserts that a run in a real VM prints what its command printed and exits 0, and that
/// once removed nothing of its VM is left in the store. It catches a VM that cannot be
/// cloned, booted or reached through the bridge, and a clone left on disk.
#[test]
#[ignore = "needs a Mac, a signed js-vmm and a base image; run by hand"]
fn a_command_runs_in_a_fresh_vm_and_the_vm_is_destroyed() {
    let root =
        std::env::var_os(super::ROOT).unwrap_or_else(|| panic!("{} is not set", super::ROOT));
    let root = Path::new(&root);
    let store = root.join("vm-store");
    let launch = HelperLaunch::new(root.join("js-vmm"), store.clone());
    let mut rt = VmRuntime::new(Arc::new(HelperDriver::new(Box::new(launch))));
    let spec = RunSpec {
        run_id: "vm-priv-1".to_owned(),
        body: Body::Container {
            image: String::new(),
            args: vec!["/bin/echo".to_owned(), "from the guest".to_owned()],
        },
        env: Vec::new(),
        cpu_millis: 2000,
        memory_bytes: 4 << 30,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch: root.join("vm-scratch"),
        files: None,
        slot: 0,
        isolation: Isolation::Contained,
        labels: [(label::KIND.to_owned(), label::VM_KIND.to_owned())].into(),
        bundle: None,
        network: Default::default(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        log_sink: None,
    };
    let id = rt.create_and_start(&spec).unwrap();
    assert_eq!(
        settle(&mut rt, &id),
        Status::Exited(Exit {
            code: 0,
            oom_killed: false
        })
    );
    let tail = rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "from the guest\n");
    rt.remove(&id).unwrap();
    let left: Vec<_> = fs::read_dir(store.join("runs")).unwrap().collect();
    assert!(left.is_empty(), "the store keeps {left:?}");
}
