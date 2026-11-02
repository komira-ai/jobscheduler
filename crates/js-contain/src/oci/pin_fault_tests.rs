//! Unit tests of the pins of the image store ([`super::pin`]) against what is not as it
//! should be: names that are not a digest or a lease, pins' files and directories another
//! user could change or that cannot be used, and a store an agent from before the pins made
//! or reads. Each fault is put in place, its effect asserted, and the fault taken away again
//! to show that the pins work once more.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::image::{PinnedImage, ROOTFS, published_root};
use super::pin::{LEASES_DIR, PINS_DIR, private};
use super::pin_tests::{collect, digest, hexn, image, kept, names, no_pins, pair, pinned, whole};

/// Puts something at a path.
type Put<'a> = dyn Fn(&Path) + 'a;

fn mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A `sha256:` image named by `digest`.
fn named(digest: &str) -> PinnedImage {
    PinnedImage {
        reference: format!("registry.example/tool@{digest}"),
        digest: digest.to_owned(),
    }
}

/// Catches a digest or a lease name that reaches the file system unchecked (an absolute
/// path or `..` would pin, or remove, outside the pins): each is refused with what it was,
/// and nothing is made outside the pins' directory, which ends empty.
#[tokio::test]
async fn a_name_that_is_not_a_digest_or_a_lease_is_refused_before_any_file() {
    let p = pair("pin-names");
    let before = names(&p.scratch);
    let short = format!("sha256:{}", "c".repeat(63));
    let capital = format!("sha256:{}", "C".repeat(64));
    for bad in [
        "",
        "sha256:",
        "sha256:../../x",
        "/etc/passwd",
        "..",
        &short,
        &capital,
    ] {
        let why = p.b.pin_image(&named(bad), "lease-b").expect_err(bad);
        let hex = bad.strip_prefix("sha256:").unwrap_or(bad);
        let said = format!("{}: {hex:?} is not a sha256 digest", p.pins.display());
        assert_eq!(why.to_string(), said);
        let removal = p.a.remove_image(&named(bad)).await.expect_err(bad);
        assert_eq!(removal.kind(), std::io::ErrorKind::InvalidInput, "{bad}");
    }
    for bad in ["", ".", "..", ".hidden", "a/b", "../x", "/abs", "a b"] {
        let said = format!("{}: {bad:?} is not a lease name", p.pins.display());
        let why = p.b.pin_image(&pinned('c'), bad).expect_err(bad);
        assert_eq!(why.to_string(), said);
        let why = p.b.unpin_image(bad).expect_err(bad);
        assert_eq!(why.to_string(), said);
    }
    p.b.unpin_image("none").expect("the pins are made");
    let mut after = names(&p.scratch);
    after.retain(|name| name != PINS_DIR);
    assert_eq!(after, before);
    no_pins(&p);
}

/// Catches a pins' directory that another user could change being used (one who could
/// write it could remove a lease's pin), or one reached through a link: a directory open to
/// others, one that is a link to a private directory, a file in its place, one the running
/// user may not read, and `leases` open to others are each refused, by the pin, the unpin
/// and the collection (which keeps the image and reports it), and each works again once
/// put right. The owner check is the same comparison for another uid. Assumes a non-root
/// user (root ignores the mode).
#[test]
fn a_pins_directory_that_is_not_private_is_refused_and_the_image_kept() {
    let p = pair("pin-dirs");
    image(&p.images, 'c');
    p.b.pin_image(&pinned('c'), "made").expect("made");
    p.b.unpin_image("made").expect("unpinned");
    let refused = |what: &str| {
        let why = p.b.pin_image(&pinned('c'), "lease-b").expect_err(what);
        assert!(
            why.to_string().starts_with(&p.pins.display().to_string()),
            "{why}"
        );
        assert!(p.b.unpin_image("lease-b").is_err(), "{what}");
        let report = collect(&p.a);
        assert!(report.removed.is_empty(), "{what}: {report:?}");
        assert_eq!(report.failed.len(), 1, "{what}: {report:?}");
        let failed = format!("{}: reading its pins: ", digest('c'));
        assert!(report.failed[0].starts_with(&failed), "{report:?}");
        assert!(whole(&p.images, 'c'), "{what}");
        why.to_string()
    };
    let works = || {
        p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
        kept(&collect(&p.a), 1);
        p.b.unpin_image("lease-b").expect("unpinned");
    };
    mode(&p.pins, 0o755);
    let why = refused("open to others");
    assert!(why.contains(": not a directory of uid "), "{why}");
    mode(&p.pins, 0o700);
    works();

    let real = p.scratch.join("real-pins");
    std::fs::rename(&p.pins, &real).expect("moved");
    std::os::unix::fs::symlink(&real, &p.pins).expect("link");
    refused("a link");
    std::fs::remove_file(&p.pins).expect("unlinked");
    std::fs::write(&p.pins, b"").expect("a file");
    refused("a file");
    std::fs::remove_file(&p.pins).expect("removed");
    std::fs::rename(&real, &p.pins).expect("back");
    works();

    mode(&p.pins, 0o000);
    refused("unreadable");
    mode(&p.pins, 0o700);
    mode(&p.pins.join(LEASES_DIR), 0o777);
    refused("leases open to others");
    mode(&p.pins.join(LEASES_DIR), 0o700);
    works();

    let dir = rustix::fs::open(
        &p.pins,
        rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
    )
    .expect("open");
    let me = rustix::process::geteuid().as_raw();
    assert!(private(&dir, me).is_ok());
    let why = private(&dir, me + 1)
        .expect_err("another owner")
        .to_string();
    assert_eq!(
        why,
        format!(
            "not a directory of uid {} closed to others (owner {me}, mode 700)",
            me + 1
        )
    );
}

/// Catches a lock file that is not one being followed or used: a link in its place (to a
/// file the running user owns), a directory, and a FIFO are each refused by the pin, the
/// collection (keeping the image and reporting it) and the removal of the image (refused,
/// not as busy), and the pins work again once the name is a plain file once more.
#[tokio::test]
async fn a_lock_file_that_is_not_a_plain_file_is_refused_and_the_image_kept() {
    let p = pair("pin-lock-files");
    image(&p.images, 'c');
    p.a.unpin_image("none").expect("the pins are made");
    let lock = p.pins.join(hexn('c'));
    let target = p.scratch.join("target");
    std::fs::write(&target, b"").expect("target");
    let fifo = |path: &Path| {
        let kind = rustix::fs::FileType::Fifo;
        let made = rustix::fs::mknodat(rustix::fs::CWD, path, kind, rustix::fs::Mode::RUSR, 0);
        made.expect("fifo");
    };
    let faults: [(&str, &Put<'_>); 3] = [
        ("a link", &|path: &Path| {
            std::os::unix::fs::symlink(&target, path).expect("link")
        }),
        ("a directory", &|path: &Path| {
            std::fs::create_dir(path).expect("dir")
        }),
        ("a fifo", &fifo),
    ];
    for (what, put) in faults {
        put(&lock);
        let why = p.b.pin_image(&pinned('c'), "lease-b").expect_err(what);
        assert!(
            why.to_string().starts_with(&p.pins.display().to_string()),
            "{why}"
        );
        let report = collect(&p.a);
        assert_eq!(
            (report.removed.len(), report.failed.len()),
            (0, 1),
            "{what}: {report:?}"
        );
        let removal = p.a.remove_image(&pinned('c')).await.expect_err(what);
        assert_ne!(removal.kind(), std::io::ErrorKind::ResourceBusy, "{what}");
        assert!(whole(&p.images, 'c'), "{what}");
        let _ = std::fs::remove_dir(&lock);
        let _ = std::fs::remove_file(&lock);
    }
    p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
    kept(&collect(&p.a), 1);
    assert_eq!(std::fs::read_to_string(&target).expect("target"), "");
}

/// Catches a pin that cannot be read being taken for no pin: a lease's pin file the running
/// user may not read counts as naming every digest, so X is kept; readable again, it names
/// what it names. Assumes a non-root user.
#[test]
fn a_pin_that_cannot_be_read_keeps_the_image() {
    let p = pair("pin-unreadable");
    image(&p.images, 'c');
    p.b.pin_image(&pinned('d'), "lease-b").expect("pinned");
    let pin = p.pins.join(LEASES_DIR).join("lease-b");
    assert_eq!(std::fs::read_to_string(&pin).expect("pin"), hexn('d'));
    mode(&pin, 0o000);
    kept(&collect(&p.a), 1);
    mode(&pin, 0o600);
    assert_eq!(collect(&p.a).removed, [digest('c')]);
}

/// Compatibility, older store to newer agent. Catches the pins needing anything an agent
/// from before them did not leave: on a store with images and no pins' directory, a
/// collection removes the unpinned images as before (making the pins as it goes), and a pin
/// then works.
#[test]
fn the_pins_need_nothing_from_a_store_from_before_them() {
    let p = pair("pin-old-store");
    image(&p.images, 'c');
    image(&p.images, 'd');
    assert!(!p.pins.exists(), "a fresh store has no pins");
    assert_eq!(
        p.a.collect_images(&[digest('c')], &[])
            .expect("collected")
            .removed,
        [digest('d')]
    );
    p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
    kept(&collect(&p.a), 1);
}

/// Compatibility, newer store to older agent. Catches the pins writing anything into what
/// an agent from before them reads (the store `oci-images`, whose layout and published
/// roots it relies on): after pins, unpins, a claim, a collection and a removal, the store
/// holds only digest directories, the pins are all in their own directory beside it, and
/// an image's published root is where it was.
#[tokio::test]
async fn the_pins_write_nothing_into_the_store_an_older_agent_reads() {
    let p = pair("pin-new-store");
    image(&p.images, 'c');
    image(&p.images, 'd');
    image(&p.images, 'e');
    p.b.pin_image(&pinned('c'), "lease-b").expect("pinned");
    p.b.pin_image(&pinned('d'), "lease-d").expect("pinned");
    p.b.unpin_image("lease-d").expect("unpinned");
    assert_eq!(collect(&p.a).removed, [digest('d'), digest('e')]);
    image(&p.images, 'e');
    p.a.remove_image(&pinned('e')).await.expect("removed");
    assert_eq!(names(&p.images), [hexn('c')]);
    assert_eq!(
        published_root(&p.scratch, &digest('c')),
        p.images.join(hexn('c')).join(ROOTFS)
    );
    assert!(whole(&p.images, 'c'));
    assert_eq!(names(&p.pins), [hexn('c'), LEASES_DIR.to_owned()]);
}
