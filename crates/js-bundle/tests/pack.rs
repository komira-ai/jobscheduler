//! The packer: the same bytes from the same tree, whatever the order, times and modes
//! around it; and the refusals that keep it from writing what the extractor would refuse.

mod layer;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use js_bundle::{Defect, LinkDefect, PackError, TreeRefusal, pack_layer, pack_tar};
use layer::{Scratch, Seen, fixture, link, snapshot, write};

/// The digest of the fixture tree's tar stream.
const GOLDEN_DIFF_ID: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";
/// The digest of the fixture tree's compressed layer.
const GOLDEN_BLOB_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";
/// The length of the fixture's tar stream before its end blocks.
const GOLDEN_UNCOMPRESSED_BYTES: u64 = 0;
/// The entries in the fixture tree.
const GOLDEN_ENTRIES: u64 = 15;

fn fixture_tree(reverse: bool) -> Scratch {
    let scratch = Scratch::new();
    fixture(&scratch.root(), reverse);
    scratch
}

/// Changes everything about a tree that a layer must not depend on: modification times and
/// the permission bits other than the execute bit.
fn disturb(root: &Path) {
    let later = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    for (path, seen) in snapshot(root) {
        let full = root.join(&path);
        let mode = match seen {
            Seen::File { exec: true, .. } => 0o711,
            Seen::File { exec: false, .. } => 0o666,
            Seen::Dir | Seen::Link(_) => continue,
        };
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(mode)).unwrap();
        std::fs::File::open(&full)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }
}

fn pack_error(root: &Path) -> PackError {
    match pack_tar(root) {
        Err(error) => error,
        Ok(_) => panic!("packed"),
    }
}

/// Packing the same tree twice gives the same bytes.
#[test]
fn packing_twice_gives_identical_bytes() {
    let scratch = fixture_tree(false);
    assert_eq!(
        pack_layer(&scratch.root()).unwrap(),
        pack_layer(&scratch.root()).unwrap()
    );
}

/// The order the entries were created in, their times, and their permission bits other than
/// execute do not change a byte.
#[test]
fn creation_order_times_and_modes_do_not_change_the_layer() {
    let one = fixture_tree(false);
    let other = fixture_tree(true);
    disturb(&other.root());
    assert_eq!(
        pack_layer(&one.root()).unwrap(),
        pack_layer(&other.root()).unwrap()
    );
}

/// The things a layer does depend on change it: a byte of content, the execute bit, a link
/// target, a name.
#[test]
fn content_execute_bit_targets_and_names_change_the_layer() {
    let base = pack_layer(&fixture_tree(false).root()).unwrap().diff_id;
    let changes: [(&str, fn(&Path)); 4] = [
        ("content", |r| write(r, "lib/data.txt", b"hello!\n", 0o644)),
        ("execute bit", |r| {
            write(r, "lib/data.txt", b"hello\n", 0o755)
        }),
        ("target", |r| {
            std::fs::remove_file(r.join("bin/alias")).unwrap();
            link(r, "bin/alias", "./tool");
        }),
        ("name", |r| write(r, "lib/data2.txt", b"hello\n", 0o644)),
    ];
    for (what, change) in changes {
        let scratch = fixture_tree(false);
        if what == "name" {
            std::fs::remove_file(scratch.root().join("lib/data.txt")).unwrap();
        }
        change(&scratch.root());
        assert_ne!(pack_layer(&scratch.root()).unwrap().diff_id, base, "{what}");
    }
}

/// The digests of the fixture are pinned: a change of the tar layout, the metadata fixed in
/// the headers or the compressor moves them, and with them every layer ever pushed.
#[test]
fn the_fixture_layer_has_these_digests() {
    let packed = pack_layer(&fixture_tree(false).root()).unwrap();
    assert_eq!(packed.diff_id.as_str(), GOLDEN_DIFF_ID);
    assert_eq!(packed.blob_digest, GOLDEN_BLOB_DIGEST);
    assert_eq!(packed.uncompressed_bytes, GOLDEN_UNCOMPRESSED_BYTES);
    assert_eq!(packed.entries, GOLDEN_ENTRIES);
}

/// Every header has owner 0:0, no names, time 0 and one of the fixed modes; entries are in
/// path order, parents first; long paths and targets are written with long-name records.
#[test]
fn headers_carry_fixed_metadata_in_path_order() {
    let scratch = fixture_tree(false);
    let packed = pack_tar(&scratch.root()).unwrap();
    assert_eq!(packed.body_bytes(), packed.bytes.len() as u64 - 1024);
    assert!(
        packed.bytes[packed.bytes.len() - 1024..]
            .iter()
            .all(|b| *b == 0)
    );
    let mut archive = tar::Archive::new(&packed.bytes[..]);
    let mut paths = Vec::new();
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        let header = entry.header();
        assert_eq!(header.uid().unwrap(), 0);
        assert_eq!(header.gid().unwrap(), 0);
        assert_eq!(header.mtime().unwrap(), 0);
        assert!(header.username_bytes().unwrap_or_default().is_empty());
        assert!(header.groupname_bytes().unwrap_or_default().is_empty());
        let mode = header.mode().unwrap();
        let expected: &[u32] = match header.entry_type() {
            tar::EntryType::Directory => &[0o755],
            tar::EntryType::Symlink => &[0o777],
            _ => &[0o644, 0o755],
        };
        assert!(expected.contains(&mode), "{mode:o}");
        paths.push(String::from_utf8(entry.path_bytes().into_owned()).unwrap());
    }
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    assert_eq!(paths.len() as u64, GOLDEN_ENTRIES);
    assert!(paths.iter().any(|p| p.len() > 100));
}

/// Paths and targets around the 100-byte header field all survive a round trip, which shows
/// the packer and the extractor agree where the long-name records begin.
#[test]
fn names_and_targets_around_the_header_width_round_trip() {
    for length in [99usize, 100, 101, 255] {
        let scratch = Scratch::new();
        let root = scratch.root();
        let name = "n".repeat(length);
        write(&root, &name, b"x", 0o644);
        link(&root, "l", &name);
        let packed = pack_layer(&root).unwrap();
        let out = Scratch::new();
        let fd = out.root_fd();
        let done = js_bundle::unpack_layer(
            &packed.blob[..],
            js_bundle::media::LayerEncoding::TarZstd,
            std::os::fd::AsFd::as_fd(&fd),
            &packed.diff_id,
            &layer::limits(),
        )
        .unwrap();
        assert_eq!(done.entries, 2);
        assert_eq!(snapshot(&root), snapshot(&out.root()));
    }
}

/// A set-user-id, set-group-id or sticky bit on a file is refused, since the extractor would
/// drop it.
#[test]
fn files_with_set_id_or_sticky_bits_are_refused() {
    for mode in [0o4755, 0o2755, 0o1644] {
        let scratch = Scratch::new();
        write(&scratch.root(), "f", b"x", mode);
        match pack_error(&scratch.root()) {
            PackError::SetId { path, mode: got } => {
                assert_eq!(path, "f");
                assert_eq!(got, mode);
            }
            other => panic!("{other:?}"),
        }
    }
}

/// A file with another name is refused: a layer holds no hard link.
#[test]
fn hard_linked_files_are_refused() {
    let scratch = Scratch::new();
    write(&scratch.root(), "a", b"x", 0o644);
    std::fs::hard_link(scratch.root().join("a"), scratch.root().join("b")).unwrap();
    match pack_error(&scratch.root()) {
        PackError::HardLinked { links, .. } => assert_eq!(links, 2),
        other => panic!("{other:?}"),
    }
}

/// A socket, and on Linux a fifo, are neither directories, files nor symlinks.
#[test]
fn sockets_and_fifos_are_refused() {
    let scratch = Scratch::new();
    let _listener = std::os::unix::net::UnixListener::bind(scratch.root().join("s")).unwrap();
    assert!(matches!(pack_error(&scratch.root()), PackError::Special { path } if path == "s"));
    drop(_listener);
    std::fs::remove_file(scratch.root().join("s")).unwrap();
    #[cfg(target_os = "linux")]
    {
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            scratch.root().join("p"),
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        assert!(matches!(pack_error(&scratch.root()), PackError::Special { path } if path == "p"));
    }
}

/// Names the rules refuse are refused when packing: not ASCII, a space, a volume name.
#[test]
fn names_outside_the_rules_are_refused() {
    let cases = [
        ("caf\u{e9}", Defect::ForbiddenByte),
        ("a b", Defect::ForbiddenByte),
        (".Trashes", Defect::RootReserved),
        ("._x", Defect::AppleDouble),
    ];
    for (name, expected) in cases {
        let scratch = Scratch::new();
        write(&scratch.root(), name, b"x", 0o644);
        match pack_error(&scratch.root()) {
            PackError::Tree(TreeRefusal::Path { defect, .. }) => {
                assert_eq!(defect, expected, "{name}")
            }
            other => panic!("{other:?}"),
        }
    }
}

/// A symlink that leaves the tree, and an absolute one, are refused; one that stays inside
/// is not.
#[test]
fn symlinks_are_held_to_the_walker() {
    let scratch = Scratch::new();
    link(&scratch.root(), "up", "..");
    assert!(matches!(
        pack_error(&scratch.root()),
        PackError::Tree(TreeRefusal::Link {
            defect: LinkDefect::Escapes,
            ..
        })
    ));
    let scratch = Scratch::new();
    link(&scratch.root(), "abs", "/etc");
    assert!(matches!(
        pack_error(&scratch.root()),
        PackError::Tree(TreeRefusal::Target {
            defect: Defect::Absolute,
            ..
        })
    ));
    let scratch = Scratch::new();
    write(&scratch.root(), "d/f", b"x", 0o644);
    link(&scratch.root(), "d/ok", "../d/f");
    pack_tar(&scratch.root()).unwrap();
}

/// Two names that differ only in case are refused where the volume tells them apart.
#[cfg(target_os = "linux")]
#[test]
fn names_that_differ_only_in_case_are_refused() {
    let scratch = Scratch::new();
    write(&scratch.root(), "a", b"1", 0o644);
    write(&scratch.root(), "A", b"2", 0o644);
    assert!(matches!(
        pack_error(&scratch.root()),
        PackError::Tree(TreeRefusal::Entry {
            defect: js_bundle::EntryDefect::CaseCollision,
            ..
        })
    ));
}

/// A directory that is missing, or is a file, cannot be packed.
#[test]
fn a_directory_that_cannot_be_read_is_an_io_error() {
    let scratch = Scratch::new();
    assert!(matches!(
        pack_error(&scratch.root().join("missing")),
        PackError::Io { .. }
    ));
    write(&scratch.root(), "f", b"x", 0o644);
    assert!(matches!(
        pack_error(&scratch.root().join("f")),
        PackError::Io { .. }
    ));
}

/// An empty tree is an empty layer.
#[test]
fn an_empty_tree_packs_to_two_end_blocks() {
    let packed = pack_tar(&Scratch::new().root()).unwrap();
    assert_eq!(packed.entries, 0);
    assert_eq!(packed.bytes, vec![0u8; 1024]);
}
