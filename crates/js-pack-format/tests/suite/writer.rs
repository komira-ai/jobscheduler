//! The streaming writer: round trips, the limits, and a failing sink.

use std::io::{self, Write};

use js_pack_format::layout::{encode_footer, footer_len};
use js_pack_format::{
    Digest, PackError, PackLimits, PackReader, PackWriter, WriteError, verify_pack,
};

use crate::common::{blob, blobs, write_pack};

/// A sink that refuses every write.
struct Refusing;

impl Write for Refusing {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Catches: a writer or reader that loses, misplaces or mislabels a blob; a fan-out or
/// binary search that misses entries in some bucket; a lookup that ignores the size half
/// of the digest; a duplicate push that writes a second record; a `len()` that does not
/// predict the finished size; a manifest that is not the footer's index.
#[test]
fn round_trip() {
    let blobs = blobs();
    let mut out = Vec::new();
    let mut w = PackWriter::new(PackLimits::default());
    assert!(w.is_empty());
    let digests: Vec<Digest> = blobs.iter().map(|b| w.push(&mut out, b).unwrap()).collect();
    assert!(!w.is_empty());
    let len_before = w.len();
    let written_before = out.len();
    assert_eq!(w.push(&mut out, &blobs[1]).unwrap(), digests[1]);
    assert_eq!(w.len(), len_before, "a duplicate push adds nothing");
    assert_eq!(out.len(), written_before, "a duplicate push writes nothing");
    assert_eq!(w.blob_count(), blobs.len());
    let finished = w.finish(&mut out).unwrap();
    assert_eq!(
        out.len() as u64,
        len_before,
        "len() predicts the finished size"
    );
    assert_eq!(finished.size, len_before);

    let r = PackReader::open(&out).unwrap();
    assert_eq!(r.footer().entries().len(), blobs.len());
    for (blob, digest) in blobs.iter().zip(&digests) {
        assert_eq!(r.get(digest).unwrap(), Some(blob.as_slice()), "{digest}");
    }
    assert_eq!(r.get(&Digest::of(b"absent")).unwrap(), None);
    let mut wrong_size = digests[1];
    wrong_size.size_bytes += 1;
    assert_eq!(r.get(&wrong_size).unwrap(), None);
    assert_eq!(r.get(&Digest::of(b"x")).unwrap(), None);
    // A digest whose first byte is 0 searches the first fan-out bucket.
    assert_eq!(
        r.footer().find(&Digest {
            hash: [0; 32],
            size_bytes: 0
        }),
        None
    );

    assert_eq!(r.footer().manifest(), finished.manifest);
    verify_pack(
        &out,
        out.len() as u64,
        &finished.manifest,
        &PackLimits::default(),
    )
    .unwrap();
}

/// Catches: records that are not back to back from offset 0 in push order: the manifest
/// offsets are the running sum of the sizes pushed, though the manifest lists them by hash.
#[test]
fn records_are_back_to_back_in_push_order() {
    let pushed = [blob(1, 10), blob(2, 3), Vec::new(), blob(3, 7)];
    let (pack, finished) = write_pack(PackLimits::default(), &pushed);
    let mut offset = 0u64;
    for b in &pushed {
        let digest = Digest::of(b);
        let entry = finished
            .manifest
            .iter()
            .find(|e| e.digest == digest.hash && e.size == digest.size_bytes)
            .unwrap();
        assert_eq!(entry.offset, offset);
        assert_eq!(
            &pack[offset as usize..offset as usize + b.len()],
            b.as_slice()
        );
        offset += b.len() as u64;
    }
    assert_eq!(finished.manifest.len(), pushed.len());
}

/// Catches: a finished pack that reports the wrong key or size in its report.
#[test]
fn finished_becomes_a_report() {
    let (pack, finished) = write_pack(PackLimits::default(), &[blob(9, 4)]);
    let report = finished.clone().into_report("cas/b/x/n-0".to_string());
    assert_eq!(report.key, "cas/b/x/n-0");
    assert_eq!(report.size, pack.len() as u64);
    assert_eq!(report.manifest, finished.manifest);
}

/// Catches: a writer that lets a pack exceed its limit, refuses a blob that fits exactly
/// (an off-by-one), or reports a blob that can never fit as merely `Full`; and one that
/// changes state or writes bytes on a refusal.
#[test]
fn writer_respects_the_size_limit() {
    let limits = PackLimits {
        max_bytes: footer_len(2) + 100,
        ..PackLimits::default()
    };
    let mut out = Vec::new();
    let mut w = PackWriter::new(limits);
    w.push(&mut out, &[1u8; 60]).unwrap();
    assert_eq!(
        w.push(&mut out, &[2u8; 41]),
        Err(WriteError::Full { size: 41 })
    );
    assert_eq!(out.len(), 60, "a refused blob writes nothing");
    w.push(&mut out, &[2u8; 40]).unwrap();
    assert_eq!(w.len(), limits.max_bytes);
    assert_eq!(w.push(&mut out, &[]), Err(WriteError::Full { size: 0 }));
    w.finish(&mut out).unwrap();
    assert_eq!(out.len() as u64, limits.max_bytes);

    let one = footer_len(1);
    let limits = PackLimits {
        max_bytes: one + 10,
        ..PackLimits::default()
    };
    let mut w = PackWriter::new(limits);
    assert_eq!(
        w.push(&mut Vec::new(), &[0u8; 11]),
        Err(WriteError::TooLarge {
            size: 11,
            limit: one + 10
        })
    );
    assert!(w.is_empty());
    w.push(&mut Vec::new(), &[0u8; 10]).unwrap();
}

/// Catches: a pack with more blobs than the grant allows (the verifier would refuse it,
/// and the upload would be wasted): the third distinct blob is refused, a blob already
/// held is still a no-op, and the finished pack passes the verifier at the same limit.
#[test]
fn writer_respects_the_blob_limit() {
    let limits = PackLimits {
        max_blobs: 2,
        ..PackLimits::default()
    };
    let mut out = Vec::new();
    let mut w = PackWriter::new(limits);
    w.push(&mut out, b"one").unwrap();
    w.push(&mut out, b"two").unwrap();
    assert_eq!(
        w.push(&mut out, b"three"),
        Err(WriteError::Full { size: 5 })
    );
    assert_eq!(w.push(&mut out, b"one").unwrap(), Digest::of(b"one"));
    assert_eq!(w.blob_count(), 2);
    let finished = w.finish(&mut out).unwrap();
    verify_pack(&out, out.len() as u64, &finished.manifest, &limits).unwrap();
}

/// Catches: the own-pack rule applied one way only. A blob at or over the threshold must
/// not join a pack that holds anything (it is `Full`, the caller starts another), nothing
/// may join a pack that holds such a blob, and such a blob alone in a pack is written.
#[test]
fn a_large_blob_has_a_pack_of_its_own() {
    let limits = PackLimits {
        own_pack_bytes: 100,
        ..PackLimits::default()
    };
    // Small first, then large: the large one is refused.
    let mut out = Vec::new();
    let mut w = PackWriter::new(limits);
    w.push(&mut out, &blob(1, 99)).unwrap();
    assert_eq!(
        w.push(&mut out, &blob(2, 100)),
        Err(WriteError::Full { size: 100 })
    );
    w.push(&mut out, &blob(3, 1)).unwrap();

    // Large into an empty pack, then anything: refused.
    let mut out = Vec::new();
    let mut w = PackWriter::new(limits);
    w.push(&mut out, &blob(4, 100)).unwrap();
    assert_eq!(
        w.push(&mut out, &blob(5, 1)),
        Err(WriteError::Full { size: 1 })
    );
    let finished = w.finish(&mut out).unwrap();
    assert_eq!(finished.manifest.len(), 1);
    verify_pack(&out, out.len() as u64, &finished.manifest, &limits).unwrap();
}

/// Catches: a failing sink that is reported as success, or a failed push that is still
/// counted: the error names the kind, and nothing is added. A sink that fails at the
/// footer fails `finish`.
#[test]
fn a_failing_sink_is_an_error() {
    let mut w = PackWriter::new(PackLimits::default());
    assert_eq!(
        w.push(&mut Refusing, b"data"),
        Err(WriteError::Io(io::ErrorKind::BrokenPipe))
    );
    assert!(w.is_empty());
    assert_eq!(
        w.finish(&mut Refusing),
        Err(WriteError::Io(io::ErrorKind::BrokenPipe))
    );
}

/// Catches: an empty pack that cannot be written or read back, for example a footer
/// length or fan-out check that is off by one at zero entries; and an empty pack the
/// strict check would accept.
#[test]
fn an_empty_pack_reads_but_does_not_verify() {
    let (pack, finished) = write_pack(PackLimits::default(), &[]);
    assert_eq!(pack.len() as u64, footer_len(0));
    assert!(finished.manifest.is_empty());
    let r = PackReader::open(&pack).unwrap();
    assert!(r.footer().entries().is_empty());
    assert_eq!(r.get(&Digest::of(b"")).unwrap(), None);
    assert_eq!(
        verify_pack(&pack, pack.len() as u64, &[], &PackLimits::default()).err(),
        Some(PackError::Empty)
    );
}

/// Catches: a footer builder whose output differs from the one the writer uses, and an
/// `index_offset` that is not the record area: the footer built from a written pack's
/// entries is that pack's footer, byte for byte.
#[test]
fn encode_footer_is_the_writers_footer() {
    let (pack, _) = write_pack(PackLimits::default(), &blobs());
    let r = PackReader::open(&pack).unwrap();
    let built = encode_footer(r.footer().records_len(), r.footer().entries());
    let records_len = usize::try_from(r.footer().records_len()).unwrap();
    assert_eq!(built, pack[records_len..]);
    assert_eq!(built.len() as u64, footer_len(blobs().len() as u64));
}
