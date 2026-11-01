//! The start check and the delegate step of the agent, in a real systemd unit started as the
//! packaged unit is (`Delegate=yes`, `DelegateSubgroup=supervisor`). Its own test binary: any
//! other test of the crun runtime runs the delegate step in its process, which moves the process
//! into the leaf and enables the controllers, and this test needs a process nothing has done that to.
//! The test is `#[ignore]`d; the crun-runtime workflow runs it in its own step.
#![cfg(target_os = "linux")]

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

    let user = std::env::var("JS_JOB_USER").expect("JS_JOB_USER");
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
