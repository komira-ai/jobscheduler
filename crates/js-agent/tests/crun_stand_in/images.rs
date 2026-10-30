//! Collecting the image store beside the runs this runtime holds.

use js_agent::lifecycle::{Prepared, Runtime};

use crate::node::Node;

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
    assert_eq!(stored(&node), ["c".repeat(64), node.digest[7..].to_owned()]);
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
