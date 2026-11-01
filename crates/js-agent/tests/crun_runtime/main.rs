//! The runtime conformance suite (`tests/runtime_suite`) over `CrunRuntime` and a real
//! `crun`, as the agent runs it on a node: a root process that starts every container as a
//! dedicated non-root job user.
//!
//! Every test is `#[ignore]`: they need `crun`, `newuidmap`/`newgidmap`, a job user with a
//! subordinate id range (`JS_JOB_USER`), cgroup v2 with a delegated unit, root, and the
//! network (the image is busybox, pinned by digest in `JS_TEST_IMAGE`). A missing
//! prerequisite fails a test, never skips it. They are run by `cargo xtask crun-tests
//! --job-user <name>`, which starts the binary as root in a `Delegate=yes` unit.
//!
//! No test waits for a clock or asserts on how long anything took: a job that must be up
//! before it is signalled says so by opening a named pipe the test reads, and a job that
//! must have ended is waited for by the runtime.
#![cfg(target_os = "linux")]

mod quota;
#[path = "../runtime_suite/mod.rs"]
mod runtime_suite;
mod subject;
mod sweep;

use runtime_suite::{Program, Subject, begin, clean_up, conformance};

conformance!(subject::crun_subject, all());

/// Proves, over a real `crun`, that a running container's cgroup is `ctr` directly below
/// the lease's cgroup, and that both are real cgroups (the lease's carries the limits the
/// agent set, the container's holds the job). This is the layout the usage sampler and the
/// integrity check accept for a direct runner. Catches a `crun` whose `cgroupsPath` lands
/// the container elsewhere, which usage would then sample as nothing.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn the_cgroup_of_a_running_container_is_ctr_below_its_lease() {
    let mut s = subject::crun_subject("the_cgroup_is_ctr_below_lease");
    let id = begin(&mut s, "r1", Program::Hold);
    let cgroup = s.runtime().borrow_mut().cgroup(&id).expect("a cgroup");
    assert_eq!(cgroup.file_name().and_then(|n| n.to_str()), Some("ctr"));
    let lease = cgroup.parent().expect("the lease's cgroup");
    assert!(lease.join("memory.max").is_file());
    assert!(cgroup.join("cgroup.procs").is_file());
    clean_up(&s);
}
