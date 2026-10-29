//! The bytes of a pack are the bytes kbf's segment writer produces.
//!
//! The reference assembler below is written from the layout in kbf's
//! `docs/design/storage.md` and `kbf-segments/src/layout.rs` alone (it shares no code with
//! this crate except the SHA-256), with a CRC-32C taken from the definition. A pack the
//! writer produces must equal its output byte for byte, and a pack it produces must be
//! read by this crate's reader: so a pack written by kbf's writer, which follows the same
//! layout, is read here, and the reverse.

use js_pack_format::layout::{
    DIGEST_SHA256, ENTRY_LEN, FANOUT_LEN, MAGIC, TRAILER_LEN, VERSION, footer_len,
};
use js_pack_format::{Digest, PackLimits, PackReader, verify_pack};

use crate::common::{bitwise_crc32c, blobs, footer_len_of, write_pack};

/// The SHA-256 of no bytes, written out from the standard value.
const EMPTY_SHA256: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// A pack of `blobs` (distinct, pushed in this order) assembled field by field.
fn reference_pack(blobs: &[Vec<u8>]) -> Vec<u8> {
    let mut records = Vec::new();
    // (hash, size, offset, crc32c): tuple order is the index order.
    let mut index = Vec::new();
    for blob in blobs {
        index.push((
            Digest::of(blob).hash,
            blob.len() as u64,
            records.len() as u64,
            bitwise_crc32c(blob),
        ));
        records.extend_from_slice(blob);
    }
    index.sort();

    let mut footer = Vec::new();
    for byte in 0..=255u8 {
        let upto = index.iter().filter(|e| e.0[0] <= byte).count() as u32;
        footer.extend(upto.to_le_bytes());
    }
    for (hash, size, offset, crc) in &index {
        footer.extend(hash);
        footer.extend(size.to_le_bytes());
        footer.extend(offset.to_le_bytes());
        footer.extend(crc.to_le_bytes());
    }
    footer.extend((records.len() as u64).to_le_bytes());
    footer.extend((index.len() as u32).to_le_bytes());
    footer.extend([1u8, 0, 0, 0]);
    let crc = bitwise_crc32c(&footer);
    footer.extend(crc.to_le_bytes());
    footer.extend(1u32.to_le_bytes());
    footer.extend(b"KBFSEG\r\n");

    let mut pack = records;
    pack.extend(footer);
    pack
}

/// Catches: a CRC oracle that is not CRC-32C, which would make every other assertion in
/// this file vacuous. The check value is the standard one.
#[test]
fn the_oracle_is_crc32c() {
    assert_eq!(bitwise_crc32c(b"123456789"), 0xe306_9283);
    assert_eq!(bitwise_crc32c(b""), 0);
}

/// Catches: a layout constant that drifted from the format.
#[test]
fn constants_are_the_formats() {
    assert_eq!(MAGIC, *b"KBFSEG\r\n");
    assert_eq!(VERSION, 1);
    assert_eq!(DIGEST_SHA256, 1);
    assert_eq!(TRAILER_LEN, 32);
    assert_eq!(ENTRY_LEN, 52);
    assert_eq!(FANOUT_LEN, 1024);
    assert_eq!(footer_len(0), 1056);
    assert_eq!(footer_len(2), 1056 + 2 * 52);
    let limits = PackLimits::default();
    assert_eq!(limits.max_bytes, 128 * 1024 * 1024);
    assert_eq!(limits.max_blobs, 16_384);
    assert_eq!(limits.own_pack_bytes, 16 * 1024 * 1024);
}

/// Catches: any change to a field's position, width, byte order or value in the writer:
/// the two-blob pack is compared with the assembler's, and its fixed fields are checked
/// against numbers written out by hand. "hello" is pushed first, so it sits at offset 0
/// and the empty blob at offset 5, but the index lists them by hash.
#[test]
fn the_small_pack_is_byte_for_byte_the_layout() {
    let pushed = vec![b"hello".to_vec(), Vec::new()];
    let (pack, finished) = write_pack(PackLimits::default(), &pushed);
    assert_eq!(pack, reference_pack(&pushed));

    let n = pack.len();
    assert_eq!(n, 5 + 1056 + 2 * 52);
    assert_eq!(finished.size, n as u64);
    assert_eq!(&pack[..5], b"hello");
    assert_eq!(Digest::of(b"").hash, EMPTY_SHA256);

    // The empty blob's index entry: hash, size 0, offset 5 (after "hello"), crc32c 0.
    let index = &pack[5 + FANOUT_LEN..n - TRAILER_LEN];
    let entry = index
        .chunks_exact(ENTRY_LEN)
        .find(|e| e[..32] == EMPTY_SHA256)
        .unwrap();
    assert_eq!(&entry[32..40], &0u64.to_le_bytes());
    assert_eq!(&entry[40..48], &5u64.to_le_bytes());
    assert_eq!(&entry[48..52], &0u32.to_le_bytes());

    // The trailer: index_offset 5, two entries, SHA-256 code 1 and three zero bytes,
    // then (after the CRC) version 1 and the magic.
    assert_eq!(
        &pack[n - 32..n - 16],
        [5, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0]
    );
    assert_eq!(
        &pack[n - 12..],
        [1, 0, 0, 0, b'K', b'B', b'F', b'S', b'E', b'G', b'\r', b'\n']
    );
    let crc = bitwise_crc32c(&pack[5..n - 16]);
    assert_eq!(&pack[n - 16..n - 12], &crc.to_le_bytes());
}

/// Catches: a divergence that only shows with many blobs (a fan-out that counts wrongly,
/// an index sorted by something other than (hash, size)): 603 blobs, several per first
/// hash byte, equal the assembler's pack.
#[test]
fn a_large_pack_is_byte_for_byte_the_layout() {
    let blobs = blobs();
    let (pack, finished) = write_pack(PackLimits::default(), &blobs);
    assert_eq!(pack, reference_pack(&blobs));
    assert_eq!(finished.manifest.len(), blobs.len());
}

/// Catches: a reader or verifier that only accepts what this crate's writer makes. The
/// pack here comes from the assembler (the layout, not this crate's writer): the reader
/// finds every blob and the strict check accepts it against the manifest it implies.
#[test]
fn a_pack_assembled_from_the_layout_is_read_and_verified() {
    let blobs = blobs();
    let pack = reference_pack(&blobs);
    let reader = PackReader::open(&pack).unwrap();
    for blob in &blobs {
        assert_eq!(
            reader.get(&Digest::of(blob)).unwrap(),
            Some(blob.as_slice())
        );
    }
    let manifest = reader.footer().manifest();
    let footer = verify_pack(&pack, pack.len() as u64, &manifest, &PackLimits::default()).unwrap();
    assert_eq!(
        footer.records_len() as usize,
        pack.len() - footer_len_of(&pack)
    );
}
