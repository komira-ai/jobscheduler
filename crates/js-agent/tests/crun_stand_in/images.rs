//! Collecting the image store beside the runs this runtime, and another agent's runtime on
//! the same store, hold.

use std::sync::Arc;

use js_agent::crun::{CrunRuntime, config_dir};
use js_agent::lifecycle::{Prepared, Runtime};

use crate::node::{LONG, NoImages, Node};

/// The digest `c` repeated, as a stored image of the node.
fn store(node: &Node, c: char) -> String {
    let hex = c.to_string().repeat(64);
    std::fs::create_dir_all(node.scratch.join("oci-images").join(&hex).join("rootfs"))
        .expect("image");
    format!("sha256:{hex}")
}

fn stored(node: &Node) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(node.scratch.join("oci-images"))
        .expect("store")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Proves the referenced set is the configured digests plus the images of the runs the
/// runtime holds: an unnamed image goes, the configured one stays, and the image of a
/// run stays until the run is removed and goes after. Catches a lease losing its root
/// filesystem, and a held run's image being treated as unreferenced.
#[test]
fn configured_and_running_images_stay_and_the_rest_go() {
    let node = Node::new("gc-runs");
    let (configured, other) = (store(&node, 'c'), store(&node, 'd'));
    let mut runtime = node.open();
    let id = runtime
        .create_and_start(&node.spec("r1-exit"))
        .expect("started");
    let report = runtime
        .collect_images(std::slice::from_ref(&configured))
        .expect("collected");
    assert_eq!(report.removed, [other]);
    assert_eq!(stored(&node), [node.digest[7..].to_owned(), "c".repeat(64)]);
    runtime.remove(&id).expect("removed");
    let report = runtime.collect_images(&[configured]).expect("collected");
    assert_eq!(report.removed, std::slice::from_ref(&node.digest));
    assert_eq!(stored(&node), ["c".repeat(64)]);
}

/// Catches an image the store removed still being answered `Ready` by the prefetcher:
/// after the collection a prepare of it pulls again (and this node has no registry, so
/// it fails).
#[test]
fn a_collected_image_is_not_ready_afterwards() {
    let node = Node::new("gc-forget");
    let runtime = node.open();
    let image = node.image();
    assert_eq!(runtime.wait_prepared(&image), Prepared::Ready);
    let report = runtime.collect_images(&[]).expect("collected");
    assert_eq!(report.removed, std::slice::from_ref(&node.digest));
    assert!(
        matches!(runtime.wait_prepared(&image), Prepared::Failed(_)),
        "answered from before the removal"
    );
}

/// The issue's case: two agents on one store, the second holding a run on the node's image,
/// which the first agent knows nothing of. Catches the first agent's collection removing an
/// image another agent's run uses as its root: the image stays while that run is held, and
/// goes at the first agent's next collection once the run is removed.
#[test]
fn an_image_another_agents_run_holds_stays_until_that_run_is_removed() {
    let node = Node::new("gc-two-agents");
    let mut first = node.open();
    let mut settings = node.settings("/root");
    settings.host_id = "node-2".to_owned();
    let hooks = Node::hooks(LONG, LONG);
    let mut second = CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens");
    let id = second
        .create_and_start(&node.spec("r1-exit"))
        .expect("started");
    let report = first.collect_images(&[]).expect("collected");
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(stored(&node), [node.digest[7..].to_owned()]);
    second.remove(&id).expect("removed");
    let report = first.collect_images(&[]).expect("collected");
    assert_eq!(report.removed, std::slice::from_ref(&node.digest));
    assert!(stored(&node).is_empty());
    assert!(first.list_labelled().expect("lists").is_empty());
}

/// Catches a run made without its image pinned: when the pin cannot be taken (here the pin
/// directory's name is a file), the run is refused, says why, and leaves no container.
#[test]
fn a_run_whose_image_cannot_be_pinned_is_refused() {
    let node = Node::new("gc-no-pin");
    std::fs::write(node.scratch.join("oci-image-pins"), b"").expect("a file");
    let mut runtime = node.open();
    let why = runtime
        .create_and_start(&node.spec("r1-exit"))
        .expect_err("no pin")
        .to_string();
    assert!(why.starts_with("pin the image: "), "{why}");
    assert!(why.contains("oci-image-pins"), "{why}");
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// Catches a run of one agent whose image has no kept config removing that image (to fetch
/// it again with its config) from under another agent's run: the removal is refused, the
/// run with it, and the other run's image stays.
#[test]
fn an_image_another_agents_run_holds_is_not_removed_to_fetch_its_config() {
    let node = Node::new("gc-no-config");
    let mut settings = node.settings("/root");
    settings.host_id = "node-2".to_owned();
    let hooks = Node::hooks(LONG, LONG);
    let mut second = CrunRuntime::open_with(settings, Arc::new(NoImages), hooks).expect("opens");
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
    assert!(why.contains("is pinned by a lease on this store"), "{why}");
    assert!(first.list_labelled().expect("lists").is_empty());
    assert_eq!(stored(&node), [hex.to_owned()]);
    second.remove(&held).expect("removed");
}
