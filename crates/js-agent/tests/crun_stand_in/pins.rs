//! Two agents on one image store: the first collects, the second holds runs on the node's
//! image, which the first knows nothing of. The image must stay for as long as any run of
//! the second agent may use it (staged, running, ended and not removed, its removal failed,
//! its agent gone without removing it) and go once the last is removed. Each state is
//! reached by the stand-in's own signals (a pipe it opens when up, the end of `crun run`),
//! never by a wait.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use js_agent::crun::{CrunRuntime, config_dir};
use js_agent::lifecycle::{Runtime, Status};

use crate::node::{Endless, LONG, NoImages, Node};

/// The second agent's runtime on `node`.
fn second(node: &Node) -> CrunRuntime {
    let mut settings = node.settings("/root");
    settings.host_id = "node-2".to_owned();
    let hooks = Node::hooks(LONG, LONG);
    CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens")
}

/// What the store holds.
fn stored(node: &Node) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(node.scratch.join("oci-images"))
        .expect("store")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The leases that pin an image.
fn pins(node: &Node) -> Vec<String> {
    let leases = node.scratch.join("oci-image-pins/leases");
    let mut names: Vec<String> = std::fs::read_dir(leases)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The first agent's collection keeps the node's image.
fn keeps(first: &CrunRuntime, node: &Node, when: &str) {
    let report = first.collect_images(&[]).expect("collected");
    assert!(report.removed.is_empty(), "{when}: {report:?}");
    assert_eq!(stored(node), [node.digest[7..].to_owned()], "{when}");
}

/// The first agent's collection removes the node's image, and no pin is left.
fn removes(first: &CrunRuntime, node: &Node) {
    let report = first.collect_images(&[]).expect("collected");
    assert_eq!(report.removed, std::slice::from_ref(&node.digest));
    assert!(stored(node).is_empty());
    assert!(pins(node).is_empty(), "{:?}", pins(node));
}

/// The issue's case. Catches the first agent's collection removing the image another
/// agent's run uses as its root: it stays while that run is held, and goes at the first
/// agent's next collection once the run is removed.
#[test]
fn an_image_another_agents_run_holds_stays_until_that_run_is_removed() {
    let node = Node::new("pin-two-agents");
    let first = node.open();
    let mut second = second(&node);
    let id = second
        .create_and_start(&node.spec("r1-exit"))
        .expect("started");
    keeps(&first, &node, "held");
    second.remove(&id).expect("removed");
    removes(&first, &node);
}

/// Catches a pin taken late or let go early in a run's life: the image stays while the
/// second agent's run is staged, running (its script says it is up), ended (`crun run`
/// over and its end read) and while a second run on it is held after the first is
/// removed; it goes once the last is removed.
#[test]
fn the_image_stays_through_every_state_of_the_runs_on_it_until_the_last_is_removed() {
    let node = Node::new("pin-states");
    let first = node.open();
    let mut second = second(&node);
    let held = second.stage(&node.spec("r1-hold")).expect("staged");
    keeps(&first, &node, "staged");
    node.pipe(&held.0);
    second.start_staged(&held).expect("started");
    node.up(&held);
    assert_eq!(second.poll(&held), Ok(Status::Running));
    keeps(&first, &node, "running");
    second.stop(&held, LONG).expect("stopped");
    second.wait_ended(&held);
    assert!(matches!(second.poll(&held), Ok(Status::Exited(_))));
    keeps(&first, &node, "ended");
    let other = second
        .create_and_start(&node.spec("r2-exit"))
        .expect("started");
    second.remove(&held).expect("removed");
    keeps(&first, &node, "one run of two removed");
    second.wait_ended(&other);
    second.remove(&other).expect("removed");
    removes(&first, &node);
}

/// Catches a pin that ends with its agent while its run's container may still run: the
/// second agent goes away without removing its run; the image stays until that agent's
/// next start sweeps the lease (removing the container first), and goes after.
#[test]
fn a_run_its_agent_left_keeps_the_image_until_that_agent_sweeps_it() {
    let node = Node::new("pin-left");
    let first = node.open();
    let mut gone = second(&node);
    gone.create_and_start(&node.spec("r1-exit"))
        .expect("started");
    drop(gone);
    keeps(&first, &node, "left by its agent");
    let restarted = second(&node);
    assert_eq!(restarted.swept().removed.len(), 1);
    removes(&first, &node);
}

/// Catches a run that fails to be made keeping its pin (the image would never go): a run
/// whose image cannot be fetched, a run whose scratch directory cannot be given to the job
/// and a run whose `crun` cannot be started each fail and leave no pin, and the image goes
/// at the next collection.
#[test]
fn a_run_that_fails_to_be_made_leaves_no_pin() {
    let node = Node::new("pin-fails-start-fails");
    let first = node.open();
    let mut second = second(&node);
    let mut absent = node.spec("r1-exit");
    let missing = format!("registry.example/job:1@sha256:{}", "cd".repeat(32));
    absent.body = js_agent::lifecycle::Body::Container {
        image: missing,
        args: Vec::new(),
    };
    second.create_and_start(&absent).expect_err("no registry");
    assert!(pins(&node).is_empty(), "{:?}", pins(&node));
    let mut refused = node.spec("r2-exit");
    refused.scratch = node.dir.join("refuse-scratch");
    std::fs::create_dir_all(&refused.scratch).expect("scratch");
    second.create_and_start(&refused).expect_err("refused");
    assert!(pins(&node).is_empty(), "{:?}", pins(&node));
    let why = second
        .create_and_start(&node.spec("r3-exit"))
        .expect_err("cannot start")
        .to_string();
    assert!(why.contains("start"), "{why}");
    assert!(second.list_labelled().expect("lists").is_empty());
    removes(&first, &node);
}

/// Catches a removal that fails letting the image go: while `crun delete` fails, and while
/// the pin itself cannot be removed (the pins' directory is open to others), the run is
/// kept and so is its image; once the removal succeeds, the image goes. Assumes a non-root
/// user.
#[test]
fn a_run_whose_removal_fails_keeps_its_image_until_it_is_removed() {
    let node = Node::new("pin-removal-fails");
    let first = node.open();
    let mut second = second(&node);
    let id = second
        .create_and_start(&node.spec("r1-exit"))
        .expect("started");
    second.wait_ended(&id);
    let marker = node.dir.join("delete-fails");
    std::fs::write(&marker, "").expect("switch");
    second.remove(&id).expect_err("delete fails");
    keeps(&first, &node, "delete failed");
    std::fs::remove_file(&marker).expect("switch off");
    let pins_dir = node.scratch.join("oci-image-pins");
    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&pins_dir, mode(0o755)).expect("open");
    let why = second.remove(&id).expect_err("unpin fails").to_string();
    assert!(why.starts_with("remove the image's pin: "), "{why}");
    assert_eq!(second.list_labelled().expect("lists").len(), 1);
    std::fs::set_permissions(&pins_dir, mode(0o700)).expect("closed");
    keeps(&first, &node, "unpin failed");
    second.remove(&id).expect("removed");
    removes(&first, &node);
}

/// Catches a run made without its image pinned: when the pin cannot be taken (the pins'
/// directory's name is a file), the run is refused, says why, and leaves no container. Its
/// sidecar stays, since the absence of its pin cannot be confirmed; once the pins'
/// directory is put right, the leftover is removed with it.
#[test]
fn a_run_whose_image_cannot_be_pinned_is_refused() {
    let node = Node::new("pin-no-pin");
    let pins_dir = node.scratch.join("oci-image-pins");
    std::fs::write(&pins_dir, b"").expect("a file");
    let mut runtime = node.open();
    let why = runtime
        .create_and_start(&node.spec("r1-exit"))
        .expect_err("no pin")
        .to_string();
    assert!(why.starts_with("the image's pin: "), "{why}");
    assert!(why.contains("oci-image-pins"), "{why}");
    assert!(runtime.list_labelled().expect("lists").is_empty());
    let ledger = node.scratch.join("oci-labels");
    assert_eq!(std::fs::read_dir(&ledger).expect("ledger").count(), 1);
    std::fs::remove_file(&pins_dir).expect("put right");
    assert_eq!(runtime.remove_orphan("r1-exit"), Ok(true));
    assert_eq!(std::fs::read_dir(&ledger).expect("ledger").count(), 0);
}

/// Catches a run of one agent whose image has no kept config removing that image (to fetch
/// it again with its config) from under another agent's run: the removal is refused, the
/// run with it, and the other run's image stays.
#[test]
fn an_image_another_agents_run_holds_is_not_removed_to_fetch_its_config() {
    let node = Node::new("pin-no-config");
    let mut second = second(&node);
    let held = second
        .create_and_start(&node.spec("r1-exit"))
        .expect("started");
    let hex = &node.digest[7..];
    std::fs::remove_file(config_dir(&node.scratch).join(format!("{hex}.json"))).expect("config");
    let mut first = node.open();
    let why = first
        .create_and_start(&node.spec("r2-exit"))
        .expect_err("pinned by the other run")
        .to_string();
    assert!(
        why.contains("is pinned by a lease, or being removed, on this store"),
        "{why}"
    );
    assert!(first.list_labelled().expect("lists").is_empty());
    assert_eq!(stored(&node), [hex.to_owned()]);
    assert_eq!(pins(&node).len(), 1, "only the other run's pin");
    second.remove(&held).expect("removed");
}

/// Catches a creation abandoned in the middle of its pull (the pull limit spent, the fetch
/// dropped) keeping its pin: the run fails, no pin is left, and a later collection removes
/// the node's image, which nothing else pins.
#[test]
fn a_run_whose_pull_is_cut_off_leaves_no_pin() {
    let node = Node::new("pin-pull-cut");
    let first = node.open();
    let hooks = Node::hooks(LONG, std::time::Duration::ZERO);
    let mut cut = node.open_with(hooks, Arc::new(Endless));
    let mut spec = node.spec("r1-exit");
    spec.body = js_agent::lifecycle::Body::Container {
        image: format!("registry.example/job:1@sha256:{}", "cd".repeat(32)),
        args: Vec::new(),
    };
    let why = cut
        .create_and_start(&spec)
        .expect_err("cut off")
        .to_string();
    assert!(why.contains("took longer than"), "{why}");
    assert!(pins(&node).is_empty(), "{:?}", pins(&node));
    // The cut unpack's stage stays for the start sweep; the stored image goes.
    let report = first.collect_images(&[]).expect("collected");
    assert_eq!(report.removed, std::slice::from_ref(&node.digest));
}
