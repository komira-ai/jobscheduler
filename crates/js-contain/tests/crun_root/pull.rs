//! The image store's unpack, through the real helper container under a root daemon: who
//! owns the directory an unpack publishes, and what an unrelated local account can do to
//! the store.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage, ROOTFS};

use crate::cell::Cell;
use crate::support::{Item, tar_of};

/// An account that is neither root nor the job user nor in the job user's range.
const STRANGER: u32 = 65_534;

/// A source that serves one layer: a directory and a file in it.
#[derive(Debug)]
pub(crate) struct OneLayer;

#[async_trait]
impl ImageSource for OneLayer {
    async fn fetch(&self, _: &PinnedImage, blobs: &Path) -> Result<Vec<String>, FetchError> {
        let layer = tar_of(
            &[
                Item::Dir {
                    path: "bin",
                    mode: 0o755,
                },
                Item::File {
                    path: "bin/hello",
                    data: b"hello",
                    mode: 0o755,
                },
            ],
            (0, 0),
        );
        fs::write(blobs.join("0.tar"), layer).map_err(|e| FetchError::Failed(e.to_string()))?;
        Ok(vec!["0.tar".to_owned()])
    }
}

/// Runs `argv` as the stranger account and returns whether it succeeded.
fn as_stranger(argv: &[&str]) -> bool {
    Command::new(argv[0])
        .args(&argv[1..])
        .uid(STRANGER)
        .gid(STRANGER)
        .status()
        .unwrap_or_else(|e| panic!("run {argv:?}: {e}"))
        .success()
}

/// Catches an unpack stage that the helper's user does not own, or that someone else can
/// write: the stage is what is renamed into the store, so the published directory shows
/// what the stage was. After a pull through the real helper the image is published (its
/// layer is there), the directory belongs to the helper's id (the first id of the job
/// user's subordinate range, which is what the helper writes as), it is not writable by
/// group or others, the store directory above it can be searched but not listed or written
/// by a stranger, and the stranger can neither move the root filesystem away nor add to
/// the directory.
#[tokio::test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
async fn a_pulled_image_is_published_owned_by_the_helpers_id_and_closed_to_strangers() {
    let cell = Cell::pulling("pull-owner", Arc::new(OneLayer));
    cell.oci
        .check_pinned(&cell.image)
        .await
        .unwrap_or_else(|e| panic!("the pull publishes: {e}"));

    let store = cell.store();
    let hex = cell.image.digest.trim_start_matches("sha256:");
    let image = store.join(hex);
    let meta = fs::metadata(&image).expect("the published directory");
    assert_eq!(
        meta.uid(),
        cell.ids.sub_uid,
        "owner of the published directory"
    );
    assert_eq!(meta.gid(), cell.ids.sub_gid);
    assert_eq!(meta.permissions().mode() & 0o777, 0o755);
    let hello = image.join(ROOTFS).join("bin/hello");
    assert_eq!(fs::read(&hello).expect("the layer's file"), b"hello");
    assert_eq!(fs::metadata(&hello).expect("hello").uid(), cell.ids.sub_uid);
    let left: Vec<_> = fs::read_dir(&store)
        .expect("store")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left, [hex], "a stage or blobs directory was left");

    // The store is searched, never listed or written, by others.
    assert_eq!(fs::metadata(&store).expect("store").mode() & 0o777, 0o711);
    let text = |path: &Path| path.to_str().expect("utf-8").to_owned();
    let (root, moved, extra) = (
        text(&image.join(ROOTFS)),
        text(&image.join("stolen")),
        text(&image.join("extra")),
    );
    assert!(!as_stranger(&["/bin/ls", &text(&store)]));
    assert!(!as_stranger(&["/bin/mv", &root, &moved]));
    assert!(!as_stranger(&["/bin/mkdir", &extra]));
    assert!(image.join(ROOTFS).is_dir() && !Path::new(&extra).exists());
}
