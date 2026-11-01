//! The image store with the real helper: what a layer makes of the root once crun's user
//! namespace is under it, and that a layer which fails publishes nothing.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::Arc;

use js_contain::oci::image::{ImageCheckError, ROOTFS};
use tar::EntryType;

use crate::cell::{Cell, ids};
use crate::source::{Layers, pinned};
use crate::support::{Item, gzip, tar_of};

/// A layer named `name` of `items`, owned by root of the image.
fn layer(name: &str, items: &[Item<'_>]) -> (String, Vec<u8>) {
    (name.to_owned(), tar_of(items, (0, 0)))
}

fn file<'a>(path: &'a str, data: &'a [u8], mode: u32) -> Item<'a> {
    Item::File { path, data, mode }
}

/// A tar with one header that claims a 40 GiB file and nothing after it, gzipped.
fn claiming_40_gib() -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_path("big").expect("path");
    header.set_size(40 << 30);
    header.set_mode(0o644);
    header.set_cksum();
    gzip(header.as_bytes())
}

/// Catches an unpack whose ownership does not land in the job's user namespace: the
/// files of a layer are owned, on the host, by the first subordinate id plus the id the
/// tar names (0 and 1000 here), a setuid bit is cleared and the other mode bits stay, a device node
/// is skipped, and the published directory is the helper's id's (the first subordinate
/// id: the unpack stage was given to it), closed to writes by anyone else, with a root
/// that no other id can write.
#[tokio::test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
async fn the_unpacked_image_is_owned_by_the_containers_ids() {
    let layers = Arc::new(Layers::of(vec![layer(
        "0.tar",
        &[
            Item::Dir {
                path: "bin",
                mode: 0o755,
            },
            file("bin/su", b"su", 0o4755),
            file("etc/passwd", b"root:x:0:0\n", 0o644),
            Item::Special {
                path: "dev-null",
                kind: EntryType::Char,
            },
        ],
    )]));
    let cell = Cell::new("owned", layers);
    let image = pinned();
    cell.oci.check_pinned(&image).await.expect("unpacked");

    let sub = ids().sub_uid;
    let dir = cell.image_dir(&image.digest);
    let root = dir.join(ROOTFS);
    let owner = |rel: &str| std::fs::symlink_metadata(root.join(rel)).expect(rel).uid();
    assert_eq!(std::fs::metadata(&root).expect("root").uid(), sub);
    assert_eq!(owner("bin"), sub);
    assert_eq!(owner("bin/su"), sub);
    assert_eq!(owner("etc/passwd"), sub);
    let mode = std::fs::metadata(root.join("bin/su")).expect("su").mode();
    assert_eq!(mode & 0o7777, 0o755, "the setuid bit was kept");
    assert!(std::fs::symlink_metadata(root.join("dev-null")).is_err());

    // Published as the helper's id wrote it, and writable by nobody else.
    let published = std::fs::metadata(&dir).expect("image directory");
    assert_eq!(published.uid(), sub);
    assert_eq!(published.permissions().mode() & 0o022, 0);
    assert_eq!(std::fs::metadata(&root).expect("root").mode() & 0o022, 0);
}

/// Catches a layer reaching outside the root through the real helper: a symlink to an
/// absolute host path is followed from the image's root, so what is written below it
/// lands inside the root and a directory of the host with that path stays empty, and
/// the stage leaves nothing but the published image.
#[tokio::test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
async fn an_absolute_symlink_leads_inside_the_root_and_not_to_the_host() {
    let outside = crate::support::scratch("crun-outside");
    std::fs::set_permissions(&*outside, std::fs::Permissions::from_mode(0o777)).expect("open");
    let target = outside.to_str().expect("utf-8").to_owned();
    let layers = Arc::new(Layers::of(vec![layer(
        "0.tar",
        &[
            Item::Symlink {
                path: "escape",
                target: &target,
            },
            file("escape/pwned", b"x", 0o644),
        ],
    )]));
    let cell = Cell::new("symlink", layers);
    let image = pinned();
    cell.oci.check_pinned(&image).await.expect("unpacked");

    let root = cell.image_dir(&image.digest).join(ROOTFS);
    let inside = root.join(target.trim_start_matches('/')).join("pwned");
    assert!(inside.is_file(), "the file did not land inside the root");
    let left: Vec<_> = std::fs::read_dir(&*outside).expect("outside").collect();
    assert!(left.is_empty(), "the host directory was written: {left:?}");
    assert_eq!(cell.store(), [image.digest.trim_start_matches("sha256:")]);
}

/// Catches a damaged or hostile layer leaving something behind: a gzip layer cut short, a
/// layer with an entry that climbs out of the root, an owner beyond the namespace and a
/// header that claims more bytes than the unpack's bound each fail the check, publish no
/// image directory and leave no stage or blobs, and a source that then serves a good
/// image is unpacked at once.
#[tokio::test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
async fn a_layer_that_fails_leaves_no_published_image_and_a_retry_succeeds() {
    let good = tar_of(&[file("f", &[7; 4000], 0o644)], (0, 0));
    let zipped = gzip(&good);
    let layers = Arc::new(Layers::default());
    let cell = Cell::new("failing", layers.clone());
    let image = pinned();
    let bad: [(&str, Vec<u8>); 4] = [
        ("cut", zipped[..zipped.len() / 2].to_vec()),
        (
            "climb",
            gzip(&tar_of(&[file("../escape", b"x", 0o644)], (0, 0))),
        ),
        (
            "owner",
            gzip(&tar_of(&[file("f", b"x", 0o644)], (70_000, 0))),
        ),
        ("bound", claiming_40_gib()),
    ];
    for (name, bytes) in bad {
        layers.serve(vec![("0.tar.gz".to_owned(), bytes)]);
        let got = cell.oci.check_pinned(&image).await;
        assert!(
            matches!(got, Err(ImageCheckError::Failed(_))),
            "{name}: {got:?}"
        );
        assert_eq!(cell.store(), Vec::<String>::new(), "{name}");
        assert!(!cell.scratch.join("escape").exists(), "{name}");
    }
    layers.serve(vec![("0.tar.gz".to_owned(), zipped)]);
    cell.oci.check_pinned(&image).await.expect("the retry");
    assert_eq!(cell.store(), [image.digest.trim_start_matches("sha256:")]);
}

/// Catches a collection that cannot remove what the helper owns, or that removes what is
/// named: the unpacked files belong to the first subordinate id, so the unprivileged
/// daemon cannot empty the directories itself and the removal has to go through a helper
/// container. Collecting with the digest named keeps the image and its files; collecting
/// with nothing named removes it (`removed` names it, `failed` is empty) and the store is
/// empty.
#[tokio::test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
async fn a_collection_removes_an_image_the_daemon_cannot_empty_itself() {
    let layers = Arc::new(Layers::of(vec![layer(
        "0.tar",
        &[
            Item::Dir {
                path: "bin",
                mode: 0o755,
            },
            file("bin/tool", b"tool", 0o755),
            file("etc/passwd", b"root:x:0:0\n", 0o644),
        ],
    )]));
    let cell = Cell::new("gc", layers);
    let image = pinned();
    cell.oci.check_pinned(&image).await.expect("unpacked");
    let tool = cell.image_dir(&image.digest).join(ROOTFS).join("bin/tool");
    assert_eq!(std::fs::metadata(&tool).expect("tool").uid(), ids().sub_uid);

    let named = std::slice::from_ref(&image.digest);
    let kept = cell.oci.collect_images(named, &[]).expect("collected");
    assert!(kept.removed.is_empty() && kept.failed.is_empty(), "{kept}");
    assert_eq!(kept.kept, 1, "{kept}");
    assert_eq!(std::fs::read(&tool).expect("kept"), b"tool");

    let all = cell.oci.collect_images(&[], &[]).expect("collected");
    assert_eq!(all.removed, named, "{all}");
    assert!(all.failed.is_empty(), "{all}");
    assert_eq!(cell.store(), Vec::<String>::new());
}
