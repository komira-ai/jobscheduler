//! The file fields of a job row: a scrubbed record round-trips with its digests, a row from
//! before the digests reads as unscrubbed, and every way the digest field can fail to parse.

use std::collections::BTreeMap;

use crate::codec::tests::{framed, full_job, job_row, with, without};
use crate::codec::{DecodeError, decode_job, field_of};
use crate::record::{FileDigest, JobFiles, JobRecord, JobSpec, JobState};
use crate::suite::full_spec;

/// The SHA-256 of `upload-target`, the contents of [`full_spec`]'s file, as `sha256sum`
/// gives it.
const UPLOAD_TARGET_SHA256: [u8; 32] = [
    0x20, 0x8b, 0x0c, 0xa2, 0x1d, 0x7b, 0xd3, 0xf1, 0x1e, 0xa8, 0xe7, 0x6d, 0x5e, 0x72, 0xb2, 0x71,
    0xef, 0x36, 0xb1, 0x1c, 0x7e, 0x57, 0x7d, 0xc0, 0x6a, 0x14, 0x99, 0x95, 0x08, 0x0a, 0x23, 0x9b,
];

/// [`full_job`] ended, as the store writes it: succeeded, with its file's digest.
fn scrubbed_job() -> JobRecord {
    let job = full_job();
    JobRecord {
        spec: job.spec.clone().digested(),
        state: JobState::Succeeded,
        ..job
    }
}

// Catches a scrubbed record whose digests are read back as contents (or the reverse), a
// digest written beside the contents it replaces, and a row from before the digests that no
// longer reads: the digest field is pinned by tag and bytes, the contents' field is absent, a
// row without the digests reads as unscrubbed, and a row an older build rewrote (an empty
// contents field beside the digests) still reads as scrubbed.
#[test]
fn scrubbed_record_codec_round_trip() {
    let scrubbed = scrubbed_job();
    assert_eq!(
        scrubbed.spec.files,
        JobFiles::Digests(BTreeMap::from([(
            "inputs/upload".to_owned(),
            FileDigest::from_bytes(UPLOAD_TARGET_SHA256),
        )]))
    );
    let row = job_row(&scrubbed);
    assert_eq!(decode_job(&row), Ok(scrubbed.clone()));
    assert_eq!(
        field_of(&row, 44),
        Some(framed(&[b"inputs/upload", &UPLOAD_TARGET_SHA256])),
        "the digests are tag 44, a map of name to 32 bytes"
    );
    assert_eq!(
        field_of(&row, 19),
        None,
        "a scrubbed row has no contents field"
    );
    assert!(
        !row.windows(b"upload-target".len())
            .any(|window| window == b"upload-target"),
        "the scrubbed row holds the file's contents"
    );
    // Decoded and written again, the digests stay digests.
    let again = decode_job(&row).map(|job| job_row(&job));
    assert_eq!(again, Ok(row.clone()));

    // A row from before the digests: contents and no tag 44, read as unscrubbed even though
    // the job has ended.
    let before = without(&row, 44);
    let before = with(&before, 19, &framed(&[b"inputs/upload", b"upload-target"]));
    assert_eq!(
        decode_job(&before).map(|job| job.spec.files),
        Ok(full_spec().files)
    );

    // A row an older build rewrote: it read no files, wrote an empty tag 19, and carried 44.
    let rewritten = with(&row, 19, &[]);
    assert_eq!(decode_job(&rewritten), Ok(scrubbed.clone()));

    // No files at all: scrubbed and unscrubbed stay apart.
    let none_scrubbed = JobRecord {
        spec: JobSpec {
            files: JobFiles::Digests(BTreeMap::new()),
            ..scrubbed.spec.clone()
        },
        ..scrubbed.clone()
    };
    let none_live = JobRecord {
        spec: JobSpec {
            files: JobFiles::default(),
            ..scrubbed.spec.clone()
        },
        ..scrubbed
    };
    for job in [none_scrubbed, none_live] {
        assert_eq!(decode_job(&job_row(&job)), Ok(job));
    }
}

// Catches a digest field read past its framing or at another width, a name read twice, and
// contents beside digests (which no build writes) read as either.
#[test]
fn a_file_digest_field_that_is_not_valid_is_refused_by_tag() {
    let row = job_row(&scrubbed_job());
    let short = [0u8; 31];
    let digest = [0u8; 32];
    let cases: [(&[u8], u16, DecodeError); 4] = [
        (
            &framed(&[b"a", &short]),
            44,
            DecodeError::BadLength { tag: 44 },
        ),
        (
            &framed(&[b"a", &digest, b"a", &digest]),
            44,
            DecodeError::BadValue { tag: 44 },
        ),
        (
            &framed(&[&[0xff], &digest]),
            44,
            DecodeError::NotUtf8 { tag: 44 },
        ),
        (&[0, 0, 0, 9], 44, DecodeError::BadValue { tag: 44 }),
    ];
    for (value, tag, error) in cases {
        assert_eq!(decode_job(&with(&row, tag, value)), Err(error), "{value:?}");
    }
    let both = with(&row, 19, &framed(&[b"inputs/upload", b"upload-target"]));
    assert_eq!(decode_job(&both), Err(DecodeError::BadValue { tag: 19 }));
}
