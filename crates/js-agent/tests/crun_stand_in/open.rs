//! Opening the runtime, and getting an image ready.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use js_agent::crun::{CrunError, CrunRuntime, config_dir, hand_over_no_follow};
use js_agent::lifecycle::{Prepared, Runtime};

use crate::node::{Endless, LONG, NoImages, Node, given};

fn why(result: Result<CrunRuntime, CrunError>) -> String {
    result.expect_err("refused").to_string()
}

/// Proves a runtime opens on a node with the system's own hooks, and that opening gives the
/// job user the one file of the agent's cgroup `crun`, run as that user, needs to move its
/// first process into a lease's cgroup (`cgroup.procs` of the delegated cgroup) and the
/// cgroup the helper containers are made in, and nothing of the directory the leases'
/// cgroups are in. Catches a `crun` that cannot start a
/// container, and a job user that can make or remove the cgroups of other leases.
#[test]
fn opening_gives_the_job_user_only_the_cgroup_file_crun_moves_processes_with() {
    let node = Node::new("open-gives");
    // The files a cgroup has and a directory made for a test has not.
    let helpers = node.cgroups.join("actions/helpers");
    std::fs::create_dir(&helpers).expect("a cgroup");
    for file in ["cgroup.procs", "cgroup.threads"] {
        std::fs::write(helpers.join(file), b"").expect("a cgroup file");
    }
    let opened = CrunRuntime::open(node.settings("/root"), Arc::new(NoImages)).expect("opens");
    assert!(format!("{opened:?}").contains("node-1"));
    drop(opened);
    node.open();
    let (uid, gid) = (node.uid, node.gid);
    assert_eq!(
        given(&node.dir),
        [
            (node.cgroups.join("root/cgroup.procs"), uid, gid),
            (helpers.clone(), uid, gid),
            (helpers.join("cgroup.procs"), uid, gid),
            (helpers.join("cgroup.threads"), uid, gid),
            (helpers.join("cgroup.subtree_control"), uid, gid),
        ]
    );
}

/// Proves the system's way of giving a path away goes by the descriptor of what it opens and
/// never through a link: a file and a directory are given (to the owner's own ids here), a
/// link to either is refused, and so is a path that is not there. Catches a hand-over that
/// follows a link the job put where the agent was about to give a path away.
#[test]
fn a_path_is_given_by_descriptor_and_never_through_a_link() {
    let node = Node::new("open-no-follow");
    let (file, dir) = (node.dir.join("a-file"), node.dir.join("a-dir"));
    std::fs::write(&file, b"").expect("file");
    std::fs::create_dir(&dir).expect("dir");
    for path in [&file, &dir] {
        assert!(hand_over_no_follow(path, node.uid, node.gid).is_ok());
        let link = path.with_extension("link");
        std::os::unix::fs::symlink(path, &link).expect("link");
        assert!(hand_over_no_follow(&link, node.uid, node.gid).is_err());
    }
    assert!(hand_over_no_follow(&node.dir.join("none"), node.uid, node.gid).is_err());
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

    // The cgroup of the helpers is there, as a file.
    let other = Node::new("open-unfit-file");
    std::fs::write(other.cgroups.join("actions/helpers"), b"a file").expect("a file");
    let why = why(open(other.settings("/root")));
    assert!(why.contains("cgroup of the helper containers"), "{why}");
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
    // A node of its own: one backend holds a node's scratch directory (its lock) at a time.
    let other = Node::new("prepare-endless");
    let endless = other.open_with(hooks, Arc::new(Endless));
    assert!(failed(&endless, &missing).contains("took longer"));
}

/// Proves a pull never holds a call: while the source never answers, `prepare` answers
/// `Pending` at once, and again with the same ticket (one pull for the image, not one for
/// each call), so a run waiting for its image is not a call that counts against any limit
/// of the lifecycle's worker. Catches a runtime that pulls inside a call.
#[test]
fn prepare_answers_at_once_while_a_pull_is_under_way() {
    let node = Node::new("prepare-pending");
    let mut runtime = node.open_with(Node::hooks(LONG, LONG), Arc::new(Endless));
    let missing = format!("registry.example/job@sha256:{}", "cd".repeat(32));
    let Prepared::Pending(ticket) = runtime.prepare(&missing, None) else {
        panic!("a pull that never ends is pending");
    };
    assert_eq!(
        runtime.prepare(&missing, Some(ticket)),
        Prepared::Pending(ticket)
    );
}

/// Proves an image the store no longer has is pulled again once the runtime is told to
/// forget it: it is answered ready while remembered, even after its directory is gone, and
/// after `forget` the next call pulls (and here the source fails). Catches a store that
/// evicts an image the runtime goes on answering ready for.
#[test]
fn a_forgotten_image_is_pulled_again() {
    let node = Node::new("prepare-forget");
    let mut runtime = node.open();
    let image = node.image();
    assert_eq!(runtime.wait_prepared(&image), Prepared::Ready);
    std::fs::remove_dir_all(node.scratch.join("oci-images").join(&node.digest[7..]))
        .expect("evict the image");
    assert_eq!(runtime.prepare(&image, None), Prepared::Ready);
    runtime.forget(&image);
    let Prepared::Failed(why) = runtime.wait_prepared(&image) else {
        panic!("a forgotten image is pulled again");
    };
    assert!(why.to_string().contains("no registry in this test"));
}

/// Proves a store left with unpacks a killed daemon (or a pull past its limit) did not finish
/// is swept when the runtime opens: a stage directory and a blobs directory of an unpack
/// are removed, and the images are not. Catches a store that fills with half-made unpacks.
#[test]
fn opening_removes_the_unpacks_a_killed_pull_left() {
    let node = Node::new("open-sweeps");
    let store = node.scratch.join("oci-images");
    let hex = "ab".repeat(32);
    let (stage, blobs) = (
        store.join(format!(".stage-{hex}-1-1")),
        store.join(format!(".stage-{hex}-1-1-blobs")),
    );
    std::fs::create_dir(&stage).expect("a stage");
    std::fs::create_dir(&blobs).expect("blobs");
    node.open();
    assert!(!stage.exists() && !blobs.exists());
    assert!(store.join(&hex).is_dir());
}
