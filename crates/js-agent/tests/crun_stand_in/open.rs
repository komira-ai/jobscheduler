//! Opening the runtime, and getting an image ready.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use js_agent::crun::{CrunError, CrunRuntime, config_dir};
use js_agent::lifecycle::{Prepared, Runtime};

use crate::node::{Endless, LONG, NoImages, Node, given};

fn why(result: Result<CrunRuntime, CrunError>) -> String {
    result.expect_err("refused").to_string()
}

/// Proves a runtime opens on a node with the system's own hooks, and that opening gives the
/// job user what `crun`, run as that user, needs of the agent's cgroup: the `cgroup.procs`
/// of the agent's delegated cgroup, and the directory, `cgroup.procs`, `cgroup.threads` and
/// `cgroup.subtree_control` of the cgroup the leases are made in, each with the job user's
/// ids. Catches a `crun` that cannot move its first process into a lease's cgroup.
#[test]
fn opening_gives_the_job_user_the_cgroups_crun_moves_processes_in() {
    let node = Node::new("open-gives");
    let opened = CrunRuntime::open(node.settings("/root"), Arc::new(NoImages)).expect("opens");
    assert!(format!("{opened:?}").contains("node-1"));
    drop(opened);
    node.open();
    let (uid, gid) = (node.uid, node.gid);
    let actions = node.cgroups.join("actions");
    assert_eq!(
        given(&node.dir),
        [
            (node.cgroups.join("root/cgroup.procs"), uid, gid),
            (actions.join(""), uid, gid),
            (actions.join("cgroup.procs"), uid, gid),
            (actions.join("cgroup.threads"), uid, gid),
            (actions.join("cgroup.subtree_control"), uid, gid),
        ]
    );
}

/// Proves each way a node can be unfit is refused at opening, with the reason: no job user
/// (the agent is root, and a container runs as another), no `crun`, no thread to pull
/// with, a store whose config directory cannot be made, and a cgroup that cannot be given
/// away. Catches a runtime that opens and fails on the first job.
#[test]
fn a_node_that_cannot_run_containers_is_refused_when_the_runtime_opens() {
    let node = Node::new("open-unfit");
    let open =
        |settings| CrunRuntime::open_with(settings, Arc::new(NoImages), Node::hooks(LONG, LONG));

    let mut settings = node.settings("/root");
    settings.oci.job_user = None;
    assert!(why(open(settings)).contains("needs the job user"));

    let mut settings = node.settings("/root");
    settings.oci.runtime = node.dir.join("no-crun");
    assert!(why(open(settings)).contains("not a file"));

    let mut settings = node.settings("/root");
    settings.pull_threads = 0;
    assert!(why(open(settings)).contains("at least one worker thread"));

    let mut settings = node.settings("/root");
    let scratch: PathBuf = node.dir.join("other-store");
    std::fs::create_dir_all(&scratch).expect("store");
    std::fs::write(config_dir(&scratch), b"a file").expect("a file");
    settings.scratch = scratch;
    assert!(why(open(settings)).contains("directory of the image configs"));

    let settings = node.settings("/refuse-root");
    assert!(why(open(settings)).contains("to the job user"));
}

/// Proves `prepare` is the pull and unpack in the background: an image the store has is
/// ready, an unpinned reference fails saying so, an image the source cannot fetch fails with
/// the source's words, and a pull that outlasts its limit fails saying it took longer. The
/// limit is given as zero, so the pull of a source that never answers is over at once.
/// Catches a runtime that waits for a pull in a call, or reports a failed pull as ready.
#[test]
fn prepare_pulls_in_the_background_and_reports_how_it_ended() {
    let node = Node::new("prepare");
    let mut runtime = node.open();
    let _ = runtime.prepare(&node.image(), None);
    assert_eq!(runtime.wait_prepared(&node.image()), Prepared::Ready);

    let failed = |runtime: &CrunRuntime, image: &str| match runtime.wait_prepared(image) {
        Prepared::Failed(why) => why.to_string(),
        other => panic!("{image}: {other:?}"),
    };
    assert!(failed(&runtime, "registry.example/job:1").contains("not pinned"));
    let missing = format!("registry.example/job@sha256:{}", "cd".repeat(32));
    assert!(failed(&runtime, &missing).contains("no registry in this test"));

    let hooks = Node::hooks(LONG, Duration::ZERO);
    let endless = node.open_with(hooks, Arc::new(Endless));
    assert!(failed(&endless, &missing).contains("took longer"));
}
