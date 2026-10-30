//! Stopping, killing and removing a container that runs, and removals that fail.

use std::os::unix::fs::PermissionsExt as _;

use js_agent::lifecycle::{Exit, RunHandle, Runtime, Status};

use crate::node::{LONG, Node};

/// Stages the run `run`, starts it once it can say it is up, and waits for that.
fn started_and_up(node: &Node, runtime: &mut js_agent::crun::CrunRuntime, run: &str) -> RunHandle {
    let id = runtime.stage(&node.spec(run)).expect("staged");
    node.pipe(&id.0);
    runtime.start_staged(&id).expect("started");
    node.up(&id);
    id
}

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// Proves a stop sends SIGTERM first: a job that handles it and exits when it comes ends
/// with its own status (143 here, the status of the stand-in's handler) within a grace no
/// test waits out, a second stop of what has ended is nothing, a running container names
/// its container cgroup once `crun` has made it and none once it ended. Catches a stop
/// that kills at once, or waits the grace out.
#[test]
fn a_stop_gives_a_job_that_handles_sigterm_the_chance_to_end() {
    let node = Node::new("stop-term");
    let mut runtime = node.open();
    let id = started_and_up(&node, &mut runtime, "r1-hold");
    let ctr = node.cgroup_of(&id.0).join("ctr");
    assert_eq!(runtime.cgroup(&id), None);
    std::fs::create_dir(&ctr).expect("the container's cgroup");
    assert_eq!(runtime.cgroup(&id), Some(ctr));
    assert_eq!(runtime.list_labelled().expect("lists")[0].running_ms, 0);
    assert_eq!(runtime.stop(&id, LONG), Ok(()));
    assert_eq!(runtime.stop(&id, LONG), Ok(()));
    assert_eq!(runtime.poll(&id), Ok(exited(143)));
    assert_eq!(runtime.cgroup(&id), None);
    assert_eq!(runtime.stop(&id, LONG), Ok(()));
    assert_eq!(runtime.remove(&id), Ok(()));
    assert_eq!(runtime.stop(&id, LONG), Ok(()));
}

/// Proves the kill when the grace is spent: a job that ignores SIGTERM and holds until the
/// lease's `cgroup.kill` is written is stopped with a grace of zero, the runtime writes
/// that file, and the container ends with 137. Catches a stop that returns with the job
/// still running.
#[test]
fn a_job_that_ignores_sigterm_is_ended_through_the_lease_cgroup() {
    let node = Node::new("stop-kill");
    let mut runtime = node.open();
    let id = started_and_up(&node, &mut runtime, "r2-stubborn");
    assert_eq!(runtime.stop(&id, std::time::Duration::ZERO), Ok(()));
    assert_eq!(runtime.poll(&id), Ok(exited(137)));
    assert_eq!(runtime.remove(&id), Ok(()));
}

/// Proves a stop that cannot end the job is an error, not a success: a job that is not
/// ended by the kill within the wait (given as zero) fails the stop saying it still runs;
/// removing it then ends it by force, and the lease is gone. Catches a stop that reports an
/// ended job that runs, and a removal that waits for ever.
#[test]
fn a_job_the_kill_does_not_end_fails_the_stop_and_is_ended_by_force_on_removal() {
    let node = Node::new("stop-stuck");
    let mut runtime = node.open_impatient();
    let id = started_and_up(&node, &mut runtime, "r3-stuck");
    let why = runtime
        .stop(&id, std::time::Duration::ZERO)
        .expect_err("still runs");
    assert!(why.to_string().contains("still runs"), "{why}");
    assert_eq!(runtime.remove(&id), Ok(()));
    assert_eq!(runtime.poll(&id), Ok(Status::Gone));
    assert!(!node.lease_of(&id.0).exists());
}

/// Proves a stop whose kill cannot be written (the lease's cgroup is gone) is an error
/// naming the cgroup, and that removing the run still works. Catches a stop that says it
/// stopped a job it could not signal.
#[test]
fn a_kill_that_cannot_be_written_fails_the_stop() {
    let node = Node::new("stop-no-cgroup");
    let mut runtime = node.open_impatient();
    let id = started_and_up(&node, &mut runtime, "r4-stuck");
    std::fs::remove_dir_all(node.cgroup_of(&id.0)).expect("remove the cgroup");
    let why = runtime
        .stop(&id, std::time::Duration::ZERO)
        .expect_err("not killed");
    assert!(why.to_string().contains("kill the lease's cgroup"), "{why}");
    assert_eq!(runtime.remove(&id), Ok(()));
}

/// Proves a removal that fails keeps the container, so that it is tried again: `crun` that
/// cannot delete it (the node has a file `delete-fails`), and a lease directory that cannot
/// be removed (its parent is not writable) each fail with their reason, the container is
/// still listed, and the removal succeeds once the cause is gone. Catches a container
/// forgotten while its files are still on the disk.
#[test]
fn a_removal_that_fails_leaves_the_container_to_be_removed_again() {
    let node = Node::new("stop-removal");
    let mut runtime = node.open();
    let id = runtime
        .create_and_start(&node.spec("r5-exit"))
        .expect("started");
    runtime.wait_ended(&id);

    let marker = node.dir.join("delete-fails");
    std::fs::write(&marker, b"").expect("marker");
    let why = runtime.remove(&id).expect_err("not deleted");
    assert!(why.to_string().contains("delete the container"), "{why}");
    std::fs::remove_file(&marker).expect("marker");

    let readable = std::fs::Permissions::from_mode(0o555);
    std::fs::set_permissions(&node.scratch, readable).expect("mode");
    let why = runtime.remove(&id).expect_err("not removed");
    assert!(
        why.to_string().contains("remove the lease directory"),
        "{why}"
    );
    assert_eq!(runtime.list_labelled().expect("lists").len(), 1);
    std::fs::set_permissions(&node.scratch, std::fs::Permissions::from_mode(0o755)).expect("mode");
    assert_eq!(runtime.remove(&id), Ok(()));
    assert!(runtime.list_labelled().expect("lists").is_empty());
}
