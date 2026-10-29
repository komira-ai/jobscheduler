//! Helpers the test modules share.

use js_pack_format::layout::{FANOUT_LEN, IndexEntry, TRAILER_LEN, encode_footer};
use js_pack_format::{Digest, Finished, Footer, PackEntry, PackLimits, PackWriter};

/// CRC-32C one bit at a time, straight from the definition: the oracle the tests use
/// instead of the crate the format uses.
pub fn bitwise_crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// An empty blob, a short one, a 4 KiB one, and enough numbered ones that several
/// share a first hash byte and exercise the fan-out.
pub fn blobs() -> Vec<Vec<u8>> {
    let mut out = vec![Vec::new(), b"hello".to_vec(), vec![0xa5; 4096]];
    out.extend((0u32..600).map(|i| format!("blob number {i}").into_bytes()));
    out
}

/// A blob of `len` bytes, all `fill`.
pub fn blob(fill: u8, len: usize) -> Vec<u8> {
    vec![fill; len]
}

/// `blobs` written in order to a pack within `limits`.
pub fn write_pack(limits: PackLimits, blobs: &[Vec<u8>]) -> (Vec<u8>, Finished) {
    let mut out = Vec::new();
    let mut writer = PackWriter::new(limits);
    for b in blobs {
        writer.push(&mut out, b).unwrap();
    }
    let finished = writer.finish(&mut out).unwrap();
    (out, finished)
}

/// The footer length of `pack`, from its trailer.
pub fn footer_len_of(pack: &[u8]) -> usize {
    usize::try_from(Footer::len_from_trailer(&pack[pack.len() - TRAILER_LEN..]).unwrap()).unwrap()
}

/// Where the index starts in `pack`.
pub fn index_start(pack: &[u8]) -> usize {
    pack.len() - footer_len_of(pack) + FANOUT_LEN
}

/// Rewrites the footer CRC so that a test can reach the checks behind it.
pub fn reseal(pack: &mut [u8]) {
    let n = pack.len();
    let start = n - footer_len_of(pack);
    let crc = bitwise_crc32c(&pack[start..n - TRAILER_LEN + 16]);
    pack[n - 16..n - 12].copy_from_slice(&crc.to_le_bytes());
}

/// A pack whose record area is `records_len` zero bytes and whose footer indexes each
/// blob at the given offset, whether or not the records agree with it, with its CRC
/// valid. Returns the pack and the manifest that matches its footer.
pub fn forged(records_len: u64, placed: &[(Vec<u8>, u64)]) -> (Vec<u8>, Vec<PackEntry>) {
    let mut entries: Vec<IndexEntry> = placed
        .iter()
        .map(|(blob, offset)| IndexEntry {
            digest: Digest::of(blob),
            offset: *offset,
            crc32c: bitwise_crc32c(blob),
        })
        .collect();
    entries.sort_by_key(|e| e.digest);
    let mut bytes = vec![0u8; usize::try_from(records_len).unwrap()];
    bytes.extend(encode_footer(records_len, &entries));
    (bytes, entries.iter().map(PackEntry::from).collect())
}
