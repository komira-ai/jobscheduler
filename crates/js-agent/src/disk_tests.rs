//! The image budget's part of the disk offered, and how the image store is measured, over
//! directories of a scratch directory.

use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;

use super::{
    IMAGES_DIR, ImageReserve, held_images, image_budget_bytes, image_kept_back,
    image_reserve_bytes, image_store, tree_bytes,
};
use crate::facts::GIB;
use crate::host::offered_disk_bytes;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// Makes the image `c` (its digest's directory and `rootfs`) in the store `dir`, with a file
/// of `len` bytes (sparse) in its root, and returns its digest.
fn image(dir: &Path, c: char, len: u64) -> String {
    let hex = c.to_string().repeat(64);
    let rootfs = dir.join(&hex).join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::File::create(rootfs.join("big"))
        .unwrap()
        .set_len(len)
        .unwrap();
    format!("sha256:{hex}")
}

/// Proves the flag's meaning: unset is no budget, 0 is refused by name (as
/// `--bundle-budget-gib` refuses it), and a value is that many GiB.
#[test]
fn the_budget_flag_is_unset_or_above_zero() {
    assert_eq!(image_budget_bytes(None), Ok(None));
    assert_eq!(
        image_budget_bytes(Some(0)),
        Err("--image-budget-gib: above 0 (leave it out for no budget)".to_owned())
    );
    assert_eq!(image_budget_bytes(Some(3)), Ok(Some(3 * GIB)));
    assert_eq!(image_budget_bytes(Some(u64::MAX)), Ok(Some(u64::MAX)));
}

/// Proves the disk offered with a budget is the disk offered without one less the budget
/// for an empty store, less only what the store does not hold for a store in use, and is
/// as it was without a budget or for a store at or over it. Catches a budget left out of the
/// offer, and images counted twice (in the free space read and in the reserve).
#[test]
fn the_offer_is_less_the_part_of_the_budget_the_store_does_not_hold() {
    let (free, low) = (100 * GIB, Some(5));
    let offered = |kept_back: u64| offered_disk_bytes(free.saturating_sub(kept_back), low);
    let before = offered(0);
    assert_eq!(offered(image_kept_back(None, 0)), before);
    assert_eq!(
        offered(image_kept_back(Some(20 * GIB), 0)),
        before - 20 * GIB
    );
    assert_eq!(
        offered(image_kept_back(Some(20 * GIB), 8 * GIB)),
        before - 12 * GIB
    );
    assert_eq!(offered(image_kept_back(Some(20 * GIB), 20 * GIB)), before);
    assert_eq!(offered(image_kept_back(Some(20 * GIB), 30 * GIB)), before);
}

/// Proves where the image store of a scratch root is: `oci/oci-images` in the directory
/// above it (the root itself when it has none), the directory js-contain unpacks into.
#[test]
fn the_image_store_is_beside_the_scratch_root() {
    assert_eq!(
        image_store(Path::new("/srv/scratch")),
        PathBuf::from("/srv/oci/oci-images")
    );
    assert_eq!(
        image_store(Path::new("/")),
        PathBuf::from("/oci/oci-images")
    );
    assert_eq!(IMAGES_DIR, js_contain::oci::image::IMAGES_DIR);
}

/// Proves a tree is measured by the larger of its entries' blocks and lengths, links not
/// followed: a sparse file counts by its length, a link to a large file by the link alone,
/// and a missing tree is an error.
#[test]
fn a_tree_counts_lengths_and_follows_no_link() {
    let dir = scratch("disk-tree");
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::File::create(tree.join("sub").join("sparse"))
        .unwrap()
        .set_len(GIB)
        .unwrap();
    let measured = tree_bytes(&tree).unwrap();
    assert!(measured >= GIB, "{measured}");
    let outside = dir.path().join("outside");
    std::fs::File::create(&outside)
        .unwrap()
        .set_len(4 * GIB)
        .unwrap();
    std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();
    let linked = tree_bytes(&tree).unwrap();
    assert!(linked > measured && linked < 2 * GIB, "{linked}");
    assert!(tree_bytes(&dir.path().join("none")).is_err());
}

/// Proves only complete images are held: a digest directory with a `rootfs` directory, by a
/// name of 64 lowercase hex digits, not a link. A stage, a digest with no root, a root that
/// is a file, a name too short or in capitals and a link are not; a missing store holds
/// none, and a store that is a file cannot be read. The held images come in digest order.
#[test]
fn only_complete_images_are_held() {
    let dir = scratch("disk-held");
    let store = dir.path().join("images");
    assert_eq!(held_images(&store).unwrap(), Vec::new());
    let (b, a) = (image(&store, 'b', GIB), image(&store, 'a', 2 * GIB));
    std::fs::create_dir_all(store.join("c".repeat(64))).unwrap();
    std::fs::create_dir_all(store.join("d".repeat(64))).unwrap();
    std::fs::write(store.join("d".repeat(64)).join("rootfs"), b"").unwrap();
    std::fs::create_dir_all(store.join("e".repeat(63)).join("rootfs")).unwrap();
    std::fs::create_dir_all(store.join("F".repeat(64)).join("rootfs")).unwrap();
    std::fs::create_dir_all(
        store
            .join(format!(".stage-{}", "a".repeat(64)))
            .join("rootfs"),
    )
    .unwrap();
    std::os::unix::fs::symlink(store.join("a".repeat(64)), store.join("9".repeat(64))).unwrap();
    let held = held_images(&store).unwrap();
    let digests: Vec<&str> = held.iter().map(|(digest, _)| digest.as_str()).collect();
    assert_eq!(digests, [a.as_str(), b.as_str()]);
    assert!(held[0].1 >= 2 * GIB && held[0].1 < 3 * GIB, "{held:?}");
    assert!(held[1].1 >= GIB && held[1].1 < 2 * GIB, "{held:?}");
    let file = dir.path().join("file");
    std::fs::write(&file, b"").unwrap();
    assert!(held_images(&file).is_err());
}

/// Proves the reserve of a store: the budget less what the store holds, and the whole
/// budget, with a warning word for word, when the store cannot be read.
#[test]
fn the_reserve_is_the_unused_budget_or_all_of_it() {
    let dir = scratch("disk-reserve");
    let store = dir.path().join("images");
    image(&store, 'a', 3 * GIB);
    let reserve = ImageReserve {
        budget_bytes: 10 * GIB,
        store,
    };
    let (kept, warning) = image_reserve_bytes(&reserve);
    assert!(kept > 6 * GIB && kept <= 7 * GIB, "{kept}");
    assert_eq!(warning, None);
    let file = dir.path().join("file");
    std::fs::write(&file, b"").unwrap();
    let unreadable = ImageReserve {
        budget_bytes: 10 * GIB,
        store: file.clone(),
    };
    let (kept, warning) = image_reserve_bytes(&unreadable);
    assert_eq!(kept, 10 * GIB);
    assert_eq!(
        warning,
        Some(format!(
            "--image-budget-gib: cannot read the bytes the image store {} holds (Not a \
             directory (os error 20)); its whole budget is kept back from jobs",
            file.display()
        ))
    );
}
