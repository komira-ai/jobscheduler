//! The strict check an untrusted pack goes through, and the reader's record checks.
//!
//! Each refusal test starts from a good pack, damages exactly one thing, and requires the
//! one error that names it. Where the damage lies behind the footer CRC, the CRC is
//! rewritten so the check under test is the one that fires.

use js_pack_format::layout::{ENTRY_LEN, FANOUT_LEN, IndexEntry, MAGIC, TRAILER_LEN};
use js_pack_format::{Digest, Footer, PackEntry, PackError, PackLimits, PackReader, verify_pack};

use crate::common::{bitwise_crc32c, blob, footer_len_of, forged, index_start, reseal, write_pack};

/// A pack of five blobs, one of them empty, and its manifest.
fn good() -> (Vec<u8>, Vec<PackEntry>) {
    let blobs = vec![
        blob(1, 10),
        blob(2, 20),
        Vec::new(),
        blob(3, 5),
        b"hello".to_vec(),
    ];
    let (pack, finished) = write_pack(PackLimits::default(), &blobs);
    (pack, finished.manifest)
}

fn check(pack: &[u8], manifest: &[PackEntry]) -> Result<Footer, PackError> {
    verify_pack(pack, pack.len() as u64, manifest, &PackLimits::default())
}

fn check_err(pack: &[u8], manifest: &[PackEntry]) -> PackError {
    check(pack, manifest).unwrap_err()
}

/// Catches: a verifier that refuses a pack the writer makes. The whole object and the
/// footer alone (what a range reader fetches) both pass, and the footer returned is the
/// manifest's.
#[test]
fn a_good_pack_verifies() {
    let (pack, manifest) = good();
    let footer = check(&pack, &manifest).unwrap();
    assert_eq!(footer.manifest(), manifest);
    let tail = &pack[pack.len() - footer_len_of(&pack)..];
    let limits = PackLimits::default();
    assert_eq!(
        verify_pack(tail, pack.len() as u64, &manifest, &limits),
        Ok(footer)
    );
}

/// Catches: a contiguity check that mistakes an empty record for an overlap or a gap.
/// An empty record shares its offset with the record after it (pushed first) or sits at
/// the end of the records (pushed last); both orders verify.
#[test]
fn empty_records_verify_in_either_position() {
    for pushed in [
        vec![Vec::new(), b"hello".to_vec()],
        vec![b"hello".to_vec(), Vec::new()],
        vec![b"a".to_vec(), Vec::new(), b"b".to_vec()],
    ] {
        let (pack, finished) = write_pack(PackLimits::default(), &pushed);
        check(&pack, &finished.manifest).unwrap();
    }
}

/// Catches: a verifier that skips the magic, version, digest function or reserved bytes
/// and so reads another object, or a future layout, as this one; and a short tail that
/// panics instead of saying how much it needed.
#[test]
fn a_bad_trailer_is_refused() {
    let (pack, manifest) = good();
    let n = pack.len();
    assert_eq!(&pack[n - 8..], &MAGIC);

    let mut bad = pack.clone();
    bad[n - 1] ^= 0xff;
    assert_eq!(check_err(&bad, &manifest), PackError::BadMagic);

    let mut bad = pack.clone();
    bad[n - 12..n - 8].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(check_err(&bad, &manifest), PackError::UnsupportedVersion(2));

    let mut bad = pack.clone();
    bad[n - 20] = 2;
    assert_eq!(
        check_err(&bad, &manifest),
        PackError::UnsupportedDigestFunction(2)
    );
    bad[n - 20] = 0;
    assert_eq!(
        check_err(&bad, &manifest),
        PackError::UnsupportedDigestFunction(0)
    );

    for at in [n - 19, n - 18, n - 17] {
        let mut bad = pack.clone();
        bad[at] = 1;
        assert_eq!(check_err(&bad, &manifest), PackError::ReservedNotZero);
        assert_eq!(
            Footer::len_from_trailer(&bad[n - TRAILER_LEN..]),
            Err(PackError::ReservedNotZero)
        );
    }

    let short = PackError::Truncated {
        needed: TRAILER_LEN as u64,
        available: TRAILER_LEN as u64 - 1,
    };
    assert_eq!(check_err(&pack[..TRAILER_LEN - 1], &manifest), short);
    assert_eq!(
        Footer::len_from_trailer(&[]),
        Err(PackError::Truncated {
            needed: TRAILER_LEN as u64,
            available: 0
        })
    );
}

/// Catches: a verifier that accepts an object cut short anywhere in its footer, or a
/// range read that fetched fewer bytes than the footer needs.
#[test]
fn a_truncated_footer_is_refused() {
    let (pack, manifest) = good();
    let footer_len = footer_len_of(&pack);
    for cut in 1..=footer_len {
        assert!(
            check(&pack[..pack.len() - cut], &manifest).is_err(),
            "cut {cut}"
        );
    }
    let tail = &pack[pack.len() - footer_len + 1..];
    assert_eq!(
        check_err(tail, &manifest),
        PackError::Truncated {
            needed: footer_len as u64,
            available: footer_len as u64 - 1
        }
    );
}

/// Catches: `index_offset + footer length == object size` left unchecked, so a pack with
/// bytes in front of or behind it, or a store that reports another length, is adopted.
#[test]
fn the_trailer_must_describe_the_object_length() {
    let (pack, manifest) = good();
    let n = pack.len() as u64;
    // Records cut from the front leave an intact footer that no longer fits the object.
    assert_eq!(
        verify_pack(&pack[1..], n - 1, &manifest, &PackLimits::default()).err(),
        Some(PackError::LengthMismatch {
            recorded: n,
            actual: n - 1
        })
    );
    // Bytes appended after the trailer.
    assert_eq!(
        verify_pack(&pack, n + 1, &manifest, &PackLimits::default()).err(),
        Some(PackError::LengthMismatch {
            recorded: n,
            actual: n + 1
        })
    );
}

/// Catches: a pack over the size limit adopted because only the blob sizes were summed,
/// and an off-by-one at the limit.
#[test]
fn an_oversize_object_is_refused() {
    let (pack, manifest) = good();
    let n = pack.len() as u64;
    let under = PackLimits {
        max_bytes: n - 1,
        ..PackLimits::default()
    };
    assert_eq!(
        verify_pack(&pack, n, &manifest, &under).err(),
        Some(PackError::ObjectTooLarge {
            size: n,
            limit: n - 1
        })
    );
    let exact = PackLimits {
        max_bytes: n,
        ..PackLimits::default()
    };
    verify_pack(&pack, n, &manifest, &exact).unwrap();
}

/// Catches: a pack with more records than the grant allows, adopted; and an off-by-one at
/// the limit.
#[test]
fn too_many_blobs_are_refused() {
    let (pack, manifest) = good();
    let count = manifest.len() as u32;
    let under = PackLimits {
        max_blobs: count - 1,
        ..PackLimits::default()
    };
    assert_eq!(
        verify_pack(&pack, pack.len() as u64, &manifest, &under).err(),
        Some(PackError::TooManyBlobs {
            count: u64::from(count),
            limit: u64::from(count - 1)
        })
    );
    let exact = PackLimits {
        max_blobs: count,
        ..PackLimits::default()
    };
    verify_pack(&pack, pack.len() as u64, &manifest, &exact).unwrap();
}

/// Catches: a verifier that skips the footer CRC: one flipped bit in the fan-out, the
/// index, the trailer prefix or the CRC itself is refused.
#[test]
fn a_damaged_footer_is_refused() {
    let (pack, manifest) = good();
    let n = pack.len();
    let index = index_start(&pack);
    for at in [index - FANOUT_LEN, index + 3, n - 16] {
        let mut bad = pack.clone();
        bad[at] ^= 0x10;
        let err = check_err(&bad, &manifest);
        assert!(
            matches!(err, PackError::FooterCrc { .. }),
            "byte {at}: {err}"
        );
    }
}

/// Catches: an unsorted index accepted (a lookup would miss blobs), and a duplicate entry
/// accepted (two records for one blob). Both are behind the CRC, so it is rewritten.
#[test]
fn an_unsorted_or_duplicate_index_is_refused() {
    let (pack, manifest) = good();
    let index = index_start(&pack);

    let mut swapped = pack.clone();
    let (a, b) = swapped[index..index + 2 * ENTRY_LEN].split_at_mut(ENTRY_LEN);
    a.swap_with_slice(b);
    reseal(&mut swapped);
    assert_eq!(
        check_err(&swapped, &manifest),
        PackError::IndexOrder { position: 1 }
    );

    let mut duplicate = pack.clone();
    let first = duplicate[index..index + ENTRY_LEN].to_vec();
    duplicate[index + ENTRY_LEN..index + 2 * ENTRY_LEN].copy_from_slice(&first);
    reseal(&mut duplicate);
    assert_eq!(
        check_err(&duplicate, &manifest),
        PackError::IndexOrder { position: 1 }
    );
}

/// Catches: a fan-out that disagrees with the index accepted, so a lookup misses a blob
/// the pack holds.
#[test]
fn a_wrong_fan_out_is_refused() {
    let (pack, manifest) = good();
    let mut bad = pack.clone();
    let at = index_start(&pack) - FANOUT_LEN;
    bad[at] = bad[at].wrapping_add(1);
    reseal(&mut bad);
    assert_eq!(check_err(&bad, &manifest), PackError::FanOut { byte: 0 });
}

/// Catches: a record that ends past `index_offset` accepted, which would let a read run
/// into the footer: a record too long for the record area, and an offset so large that
/// offset + size overflows.
#[test]
fn a_record_past_index_offset_is_refused() {
    let (bytes, manifest) = forged(8, &[(blob(1, 10), 0)]);
    assert_eq!(
        check_err(&bytes, &manifest),
        PackError::RecordOutOfBounds {
            digest: Digest::of(&blob(1, 10))
        }
    );

    let (pack, manifest) = good();
    let mut bad = pack.clone();
    let offset_at = index_start(&pack) + 40;
    bad[offset_at..offset_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    reseal(&mut bad);
    assert!(matches!(
        check_err(&bad, &manifest),
        PackError::RecordOutOfBounds { .. }
    ));
}

/// Catches: records that overlap accepted (one stored byte vouching for two blobs): two
/// records sharing bytes, an empty record inside another, and two records at one offset.
#[test]
fn overlapping_records_are_refused() {
    for (records_len, placed) in [
        (15, vec![(blob(1, 10), 0), (blob(2, 10), 5)]),
        (10, vec![(blob(1, 10), 0), (Vec::new(), 5)]),
        (10, vec![(blob(1, 10), 0), (blob(2, 10), 0)]),
    ] {
        let (bytes, manifest) = forged(records_len, &placed);
        assert_eq!(
            check_err(&bytes, &manifest),
            PackError::Overlap { position: 1 }
        );
    }
}

/// Catches: records that leave a gap accepted (bytes in the pack that no digest covers):
/// before the first record, between two, and between the last and the footer. The control
/// is the same two records laid back to back.
#[test]
fn gaps_between_records_are_refused() {
    let (bytes, manifest) = forged(13, &[(blob(1, 10), 0), (blob(2, 3), 10)]);
    check(&bytes, &manifest).unwrap();

    let cases = [
        (13, vec![(blob(1, 10), 3)], 0),
        (15, vec![(blob(1, 10), 0), (blob(2, 3), 12)], 1),
        (12, vec![(blob(1, 10), 0)], 1),
    ];
    for (records_len, placed, position) in cases {
        let (bytes, manifest) = forged(records_len, &placed);
        assert_eq!(check_err(&bytes, &manifest), PackError::Gap { position });
    }
}

/// Catches: a footer that is not the manifest accepted (the host reports one thing and
/// stores another): a changed digest, size or offset, a missing or extra entry, entries
/// out of order, and no manifest at all.
#[test]
fn a_footer_that_is_not_the_manifest_is_refused() {
    let (pack, manifest) = good();
    let edits: [fn(&mut Vec<PackEntry>); 7] = [
        |m| m[1].digest[0] ^= 1,
        |m| m[1].size += 1,
        |m| m[1].offset += 1,
        |m| {
            m.pop();
        },
        |m| m.push(m[0]),
        |m| m.reverse(),
        |m| m.clear(),
    ];
    for (i, edit) in edits.iter().enumerate() {
        let mut claimed = manifest.clone();
        edit(&mut claimed);
        assert_eq!(
            check_err(&pack, &claimed),
            PackError::ManifestMismatch,
            "edit {i}"
        );
    }
}

/// Catches: a reader that skips the per-record CRC (the error would then come from the
/// digest check instead), or one whose damage to one record spills into others.
#[test]
fn a_flipped_byte_in_a_record_is_refused() {
    let (clean, _) = good();
    let footer = PackReader::open(&clean).unwrap().footer().clone();
    for entry in footer.entries().iter().filter(|e| e.digest.size_bytes > 0) {
        let mut damaged = clean.clone();
        let at = entry.offset + entry.digest.size_bytes / 2;
        damaged[at as usize] ^= 0x01;
        let r = PackReader::open(&damaged).unwrap();
        assert_eq!(
            r.get(&entry.digest),
            Err(PackError::RecordCrc {
                digest: entry.digest
            })
        );
        let other = footer
            .entries()
            .iter()
            .find(|e| e.digest != entry.digest)
            .unwrap();
        assert!(r.get(&other.digest).unwrap().is_some());
    }
}

/// Catches: a record check that trusts the CRC alone, or the length: a record whose CRC
/// matches but whose bytes are another blob's is `RecordDigest`; a record of the wrong
/// length is `RecordLength`; wrong bytes with a wrong CRC are `RecordCrc`; the right
/// record passes; and its range is its offset and size.
#[test]
fn every_record_check_has_its_own_error() {
    let digest = Digest::of(b"right");
    let entry = IndexEntry {
        digest,
        offset: 7,
        crc32c: bitwise_crc32c(b"wrong"),
    };
    assert_eq!(entry.range(), 7..12);
    assert_eq!(
        entry.verify(b"wrong"),
        Err(PackError::RecordDigest { digest })
    );
    assert_eq!(
        entry.verify(b"righ"),
        Err(PackError::RecordLength { digest, actual: 4 })
    );
    assert_eq!(entry.verify(b"right"), Err(PackError::RecordCrc { digest }));
    let honest = IndexEntry {
        crc32c: bitwise_crc32c(b"right"),
        ..entry
    };
    assert_eq!(honest.verify(b"right"), Ok(()));
}

/// Catches: error text that is empty or names the wrong thing.
#[test]
fn errors_say_what_they_refused() {
    let digest = Digest::of(b"");
    let text = PackError::RecordCrc { digest }.to_string();
    assert!(text.contains("e3b0c44298fc1c14"), "{text}");
    assert!(text.contains("/0"), "{text}");
    assert_eq!(
        PackError::Gap { position: 2 }.to_string(),
        "pack records leave a gap at record 2"
    );
}
