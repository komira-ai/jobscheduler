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

/// Proves, in a real systemd unit started as the packaged unit is (`Delegate=yes` and
/// `DelegateSubgroup=supervisor`, so the agent starts in `<unit>/supervisor` and the unit's
/// cgroup holds no process), that the agent's start check passes before anything has enabled
/// a controller for the unit's children, and that the delegate step the crun wiring then runs
/// enables `cpu`, `memory` and `pids` in the unit's own `cgroup.subtree_control` and in
/// `actions`. Catches a start check that refuses a stock systemd host (systemd writes no
/// `subtree_control` for a delegated unit), and a delegate step that leaves the unit without
/// the controllers. Run by its own step of the workflow (`DelegateSubgroup=` needs systemd 254).
#[test]
#[ignore = "real systemd unit with DelegateSubgroup=supervisor: run by the crun-runtime workflow"]
fn the_start_check_passes_in_a_unit_with_a_supervisor_leaf_and_delegate_enables_the_controllers() {
    use js_agent::SystemCommands;
    use js_agent::crun_host::{CrunHost, Piece, Request, check};
    use js_contain::delegate::{self, PROC_SELF_CGROUP};

    let own = std::fs::read_to_string(PROC_SELF_CGROUP).expect("read /proc/self/cgroup");
    let line = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .expect("cgroup v2");
    assert!(
        line.ends_with("/supervisor"),
        "not started with DelegateSubgroup=supervisor: in {line}"
    );
    let unit = std::path::Path::new("/sys/fs/cgroup")
        .join(line.trim_start_matches('/'))
        .parent()
        .unwrap()
        .to_owned();
    // The premise: systemd enabled nothing in the unit for its children.
    let enabled = std::fs::read_to_string(unit.join("cgroup.subtree_control")).expect("read");
    assert!(
        enabled.trim().is_empty(),
        "the unit already enables {enabled}"
    );

    let user = subject::var("JS_JOB_USER");
    let dir = std::env::temp_dir();
    let report = check(
        &Request {
            crun: std::path::Path::new("/usr/bin/crun"),
            job_user: &user,
            state_root: &dir,
            scratch_root: &dir,
        },
        &CrunHost::system(),
        &SystemCommands::default(),
    );
    let delegation: Vec<_> = report
        .missing
        .iter()
        .filter(|m| m.piece == Piece::CgroupDelegation)
        .collect();
    assert!(delegation.is_empty(), "{delegation:?}");

    let done = delegate::delegate(std::path::Path::new("/sys/fs/cgroup"), &own, 0, None)
        .expect("delegate");
    for cgroup in [
        unit,
        std::path::Path::new("/sys/fs/cgroup").join(done.actions.trim_start_matches('/')),
    ] {
        let text = std::fs::read_to_string(cgroup.join("cgroup.subtree_control")).expect("read");
        for controller in ["cpu", "memory", "pids"] {
            assert!(
                text.split_whitespace().any(|c| c == controller),
                "{}: {text}",
                cgroup.display()
            );
        }
    }
}
