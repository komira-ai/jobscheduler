//! Collecting the image store under a root daemon with the real helper container: the
//! digest directories an unpack publishes belong to the helper's id (the first id of the
//! job user's subordinate range), so the daemon cannot empty them itself and the
//! collection has to go through the helper.

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;

use js_contain::oci::image::{PinnedImage, ROOTFS};

use crate::cell::Cell;
use crate::pull::OneLayer;

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("store")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches a collection that works on daemon-owned trees only: two images are unpacked
/// through the real helper (each directory belongs to the helper's id, which the root
/// daemon's own removal cannot empty by walking: the contents are owned by that id and the
/// directory is not the daemon's to write below). Collecting with one digest named removes
/// the other, reports it removed and nothing failed, and the named image keeps its files
/// (the layer's file, its owner, no stage or blobs directory left). Collecting with
/// nothing named removes the rest and leaves the store empty.
#[tokio::test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
async fn a_collection_removes_what_the_helper_owns_and_keeps_what_is_named() {
    let cell = Cell::pulling("gc-real", Arc::new(OneLayer));
    let second = PinnedImage {
        reference: format!("local/other@sha256:{}", "e".repeat(64)),
        digest: format!("sha256:{}", "e".repeat(64)),
    };
    for image in [&cell.image, &second] {
        cell.oci
            .check_pinned(image)
            .await
            .unwrap_or_else(|e| panic!("the pull publishes {image}: {e}"));
    }
    let store = cell.store();
    let (hex, other) = (
        cell.image.digest.trim_start_matches("sha256:").to_owned(),
        second.digest.trim_start_matches("sha256:").to_owned(),
    );
    let owner = fs::metadata(store.join(&other)).expect("published").uid();
    assert_eq!(owner, cell.ids.sub_uid, "the directory is the helper id's");

    let named = std::slice::from_ref(&cell.image.digest);
    let kept = cell.oci.collect_images(named, &[]).expect("collected");
    assert_eq!(kept.removed, std::slice::from_ref(&second.digest), "{kept}");
    assert!(kept.failed.is_empty(), "{kept}");
    assert_eq!(
        names(&store),
        std::slice::from_ref(&hex),
        "the other digest is gone, nothing left"
    );
    let hello = store.join(&hex).join(ROOTFS).join("bin/hello");
    assert_eq!(fs::read(&hello).expect("the named image's file"), b"hello");
    assert_eq!(fs::metadata(&hello).expect("hello").uid(), cell.ids.sub_uid);

    let all = cell.oci.collect_images(&[], &[]).expect("collected");
    assert_eq!(all.removed, named, "{all}");
    assert!(all.failed.is_empty(), "{all}");
    assert_eq!(
        names(&store),
        Vec::<String>::new(),
        "the store is not empty"
    );
}
