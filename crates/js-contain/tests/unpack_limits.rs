//! The bounds of the unpacker against inputs that are small on the wire and large once
//! read: a compressed layer that inflates far beyond its size, and a header that claims
//! more than any honest layer holds.

#[allow(dead_code)]
mod support;

use std::path::PathBuf;

use js_contain::unpack::{Compression, Ownership, Unpack, UnpackError};
use support::{Item, gzip, my_ids, scratch, tar_of};

fn mine() -> Ownership {
    let (uid, gid) = my_ids();
    Ownership::AllTo { uid, gid }
}

/// Catches a byte bound that counts the compressed layer instead of what it inflates to:
/// a gzip layer of under 256 KiB that holds a 16 MiB file of zeros is refused under a
/// 1 MiB bound, before the file is written (nothing of it is in the root), and the same
/// layer is accepted under a bound that fits it, so the refusal is the bound's.
#[test]
fn a_compressed_layer_is_bounded_by_what_it_inflates_to() {
    let zeros = vec![0u8; 16 << 20];
    let layer = gzip(&tar_of(
        &[Item::File {
            path: "bomb",
            data: &zeros,
            mode: 0o644,
        }],
        (u64::from(my_ids().0), u64::from(my_ids().1)),
    ));
    assert!(
        layer.len() < 256 << 10,
        "the fixture must be small: {}",
        layer.len()
    );
    let base = scratch("bomb");
    let path = base.join("0.tar.gz");
    std::fs::write(&path, &layer).expect("layer");
    let layers = [(path, Compression::Gzip)];

    let root = base.join("refused");
    let got = Unpack::new(mine()).max_bytes(1 << 20).run(&root, &layers);
    assert!(
        matches!(got, Err(UnpackError::TooBig(n)) if n == 1 << 20),
        "{got:?}"
    );
    assert!(
        std::fs::symlink_metadata(root.join("bomb")).is_err(),
        "the file was written before the bound was checked"
    );

    let root = base.join("accepted");
    let got = Unpack::new(mine()).max_bytes(16 << 20).run(&root, &layers);
    assert_eq!(got.expect("within the bound").bytes, 16 << 20);
}

/// Catches a bound that waits for the bytes to arrive: a header that claims 1 GiB with
/// nothing after it is refused as too big, not read until the tar ends (which would be
/// an archive error, and a gigabyte of reading for a lie).
#[test]
fn a_header_that_claims_too_much_is_refused_by_its_claim() {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_path("big").expect("path");
    header.set_size(1 << 30);
    header.set_mode(0o644);
    header.set_uid(u64::from(my_ids().0));
    header.set_gid(u64::from(my_ids().1));
    header.set_cksum();
    let base = scratch("claim");
    let path: PathBuf = base.join("0.tar");
    std::fs::write(&path, header.as_bytes()).expect("layer");
    let got = Unpack::new(mine())
        .max_bytes(1 << 20)
        .run(&base.join("rootfs"), &[(path, Compression::None)]);
    assert!(
        matches!(got, Err(UnpackError::TooBig(n)) if n == 1 << 20),
        "{got:?}"
    );
}
