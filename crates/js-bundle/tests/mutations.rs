//! A fixed-seed stand-in for a fuzzer: thousands of damaged copies of good archives go
//! through the extractor. None may panic, none may create anything outside the target,
//! and whatever is extracted contains no symlink that resolves outside it. The same holds
//! for the layer media-type allow-list. (The fuzz targets in `fuzz/` run the same checks
//! under a coverage-guided fuzzer.)

mod layer;

use std::os::fd::AsFd as _;
use std::path::Path;

use js_bundle::media::{self, LayerEncoding};
use js_bundle::{Limits, pack_tar, unpack_tar};
use layer::{Entry, Scratch, Tar, fixture};

/// xorshift64: a seeded stream, so a failure replays.
struct Rng(u64);

impl Rng {
    fn step(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.step() % bound as u64) as usize
    }
}

/// Damages `bytes` in one of five ways.
fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    match rng.below(5) {
        0 => {
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(out.len());
                out[at] = rng.step() as u8;
            }
        }
        1 => out.truncate(rng.below(out.len())),
        2 => {
            let at = rng.below(out.len() / 512) * 512;
            let block = out[at..(at + 512).min(out.len())].to_vec();
            let _ = out.splice(at..at, block);
        }
        3 => {
            let at = rng.below(out.len());
            let _ = out.splice(at..at, [0u8; 512]);
        }
        _ => {
            let at = rng.below(out.len() / 512) * 512;
            let end = (at + 512).min(out.len());
            let _ = out.drain(at..end);
        }
    }
    out
}

/// Every symlink below `root` that resolves resolves inside `root`.
fn assert_links_inside(root: &Path) {
    let canonical_root = root.canonicalize().unwrap();
    for path in layer::snapshot(root).keys() {
        let full = root.join(path);
        if std::fs::symlink_metadata(&full).unwrap().is_symlink()
            && let Ok(resolved) = full.canonicalize()
        {
            assert!(
                resolved.starts_with(&canonical_root),
                "{path} -> {resolved:?}"
            );
        }
    }
}

fn seeds() -> Vec<Vec<u8>> {
    let source = Scratch::new();
    fixture(&source.root(), false);
    let packed = pack_tar(&source.root()).unwrap().bytes;
    let hostile = Tar::new()
        .dir("d")
        .symlink("d/s", "..")
        .symlink("t", "d/s/..")
        .entry(Entry::new(b"h", b'1').link(b"d"))
        .pax(&[("path", b"p")])
        .file("p", b"x")
        .end();
    vec![packed, hostile]
}

/// Damaged archives never escape the target and never panic.
#[test]
fn damaged_archives_never_escape_or_panic() {
    let limits = Limits {
        entries: 64,
        bytes: 1 << 20,
    };
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for seed in seeds() {
        for _ in 0..1500 {
            let damaged = mutate(&mut rng, &seed);
            let scratch = Scratch::new();
            let fd = scratch.root_fd();
            let result = unpack_tar(&damaged[..], fd.as_fd(), &limits);
            assert_eq!(std::fs::read_dir(scratch.outside()).unwrap().count(), 0);
            // A refused archive leaves a partial tree for the caller to discard, symlinks
            // included; an admitted one has none that leaves.
            if result.is_ok() {
                assert_links_inside(&scratch.root());
            }
        }
    }
}

/// Only the two layer media types are accepted, however they are damaged.
#[test]
fn damaged_media_types_are_never_a_layer() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for valid in [media::LAYER_TAR_ZSTD, media::LAYER_TAR] {
        assert!(LayerEncoding::from_media_type(valid).is_some());
        for _ in 0..2000 {
            let mut bytes = valid.as_bytes().to_vec();
            match rng.below(3) {
                0 => {
                    let at = rng.below(bytes.len());
                    bytes[at] ^= 1 << rng.below(7);
                }
                1 => bytes.truncate(rng.below(bytes.len())),
                _ => bytes.push(b"+/. "[rng.below(4)]),
            }
            let text = String::from_utf8_lossy(&bytes);
            let found = LayerEncoding::from_media_type(&text);
            assert!(found.is_none() || text == media::LAYER_TAR || text == media::LAYER_TAR_ZSTD);
        }
    }
    for generic in [
        "application/vnd.oci.image.layer.v1.tar",
        "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.oci.image.layer.v1.tar+zstd",
        "",
    ] {
        assert!(LayerEncoding::from_media_type(generic).is_none());
    }
}
