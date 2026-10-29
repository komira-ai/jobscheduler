//! Pack, then unpack: the same tree.

mod layer;

use std::os::fd::AsFd as _;

use js_bundle::media::LayerEncoding;
use js_bundle::{Limits, StreamError, UnpackError, pack_layer, pack_tar, unpack_layer};
use layer::{Scratch, fixture, limits, mode_of, snapshot};

fn packed_fixture() -> (Scratch, js_bundle::Packed) {
    let scratch = Scratch::new();
    fixture(&scratch.root(), false);
    let packed = pack_layer(&scratch.root()).unwrap();
    (scratch, packed)
}

/// A packed tree comes back as the same tree, and the extractor's count and digest are the
/// packer's.
#[test]
fn a_packed_layer_unpacks_to_the_same_tree() {
    let (source, packed) = packed_fixture();
    let out = Scratch::new();
    let fd = out.root_fd();
    let done = unpack_layer(
        &packed.blob[..],
        LayerEncoding::TarZstd,
        fd.as_fd(),
        &packed.diff_id,
        &limits(),
    )
    .unwrap();
    assert_eq!(snapshot(&source.root()), snapshot(&out.root()));
    assert_eq!(done.entries, packed.entries);
    assert_eq!(done.diff_id, packed.diff_id);
    assert_eq!(done.stream_bytes, packed.uncompressed_bytes + 1024);
    assert_eq!(mode_of(&out.root().join("bin")), 0o755);
    assert_eq!(mode_of(&out.root().join("bin/tool")), 0o755);
    assert_eq!(mode_of(&out.root().join("lib/empty.bin")), 0o644);
}

/// The same holds for a layer stored as a plain tar.
#[test]
fn a_packed_tar_unpacks_to_the_same_tree() {
    let source = Scratch::new();
    fixture(&source.root(), true);
    let tar = pack_tar(&source.root()).unwrap();
    let out = Scratch::new();
    let fd = out.root_fd();
    let done = unpack_layer(
        &tar.bytes[..],
        LayerEncoding::Tar,
        fd.as_fd(),
        &tar.diff_id,
        &limits(),
    )
    .unwrap();
    assert_eq!(snapshot(&source.root()), snapshot(&out.root()));
    assert_eq!(done.diff_id, tar.diff_id);
}

/// The numbers the packer reports for the config are exactly enough: one entry or one byte
/// less is refused.
#[test]
fn the_configs_numbers_are_exactly_the_extractors_limits() {
    let (_, packed) = packed_fixture();
    let extract = |limits: Limits| {
        let out = Scratch::new();
        let fd = out.root_fd();
        unpack_layer(
            &packed.blob[..],
            LayerEncoding::TarZstd,
            fd.as_fd(),
            &packed.diff_id,
            &limits,
        )
    };
    let exact = Limits {
        entries: packed.entries,
        bytes: packed.uncompressed_bytes,
        model_bytes: 1 << 28,
    };
    extract(exact).unwrap();
    let fewer = Limits {
        entries: packed.entries - 1,
        ..exact
    };
    assert!(matches!(
        extract(fewer),
        Err(UnpackError::TooManyEntries { .. })
    ));
    let smaller = Limits {
        bytes: packed.uncompressed_bytes - 1,
        ..exact
    };
    assert!(matches!(
        extract(smaller),
        Err(UnpackError::Stream(StreamError::Ceiling { .. }))
    ));
}
