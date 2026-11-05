//! The object layout: names, their order, the prefix rules and the gap marker's body.

use js_logstore::layout::{
    self, ChunkName, FIRST_BYTE_AT_MS, GAP_SUFFIX, Gap, JobPrefix, LayoutError, MAX_CHUNK_BYTES,
    MAX_JOB_ID_LEN, MAX_PREFIX_LEN, MAX_PUT_BYTES, OFFSET_WIDTH, Stream,
};

/// The offsets the order test uses: each side of a digit-count boundary, a chunk's size, and
/// the largest.
const OFFSETS: [u64; 8] = [0, 9, 10, 99, 999, 1000, 1_048_576, u64::MAX];

/// Asserts that chunk, gap and final keys for offsets across digit-count boundaries, 1 MiB and
/// `u64::MAX` sort in numeric order as byte strings, a gap marker right after the chunk of its
/// offset; that each name parses back to its offset and kind; and that the limits are the
/// layout's. Catches a wrong width or a name that does not round-trip.
#[test]
fn names_sort_in_offset_order_and_parse_back() {
    let job = JobPrefix::new("js", "build-42").unwrap();
    assert_eq!(job.as_str(), "js/build-42/");
    let mut keys = Vec::new();
    for offset in OFFSETS {
        keys.push(job.chunk_key(Stream::Stdout, offset));
        keys.push(job.gap_key(Stream::Stdout, offset));
    }
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(sorted, keys);

    assert_eq!(
        job.chunk_key(Stream::Stdout, 1_048_576),
        "js/build-42/stdout/00000000000001048576"
    );
    assert_eq!(
        job.gap_key(Stream::Stderr, 67_108_864),
        "js/build-42/stderr/00000000000067108864.gap"
    );
    assert_eq!(
        job.chunk_key(Stream::Stderr, u64::MAX),
        "js/build-42/stderr/18446744073709551615"
    );
    assert_eq!(job.final_key(Stream::Stdout), "js/build-42/stdout.log");
    assert_eq!(job.final_key(Stream::Stderr), "js/build-42/stderr.log");
    assert_eq!(job.stream_prefix(Stream::Stderr), "js/build-42/stderr/");

    let pairs = [Stream::Stdout, Stream::Stderr]
        .into_iter()
        .flat_map(|stream| OFFSETS.map(|offset| (stream, offset)));
    for (stream, offset) in pairs {
        let prefix = job.stream_prefix(stream);
        let chunk = job.chunk_key(stream, offset);
        let gap = job.gap_key(stream, offset);
        let name = chunk.strip_prefix(&prefix).unwrap();
        assert_eq!(name.len(), OFFSET_WIDTH);
        let chunk_name = Some(ChunkName {
            offset,
            is_gap: false,
        });
        let gap_name = Some(ChunkName {
            offset,
            is_gap: true,
        });
        assert_eq!(layout::parse_chunk_name(name), chunk_name);
        assert_eq!(
            layout::parse_chunk_name(gap.strip_prefix(&prefix).unwrap()),
            gap_name
        );
    }
    for offset in OFFSETS {
        let name = format!("{offset:020}");
        assert_eq!(
            layout::parse_chunk_name(&name).map(|parsed| parsed.offset),
            Some(offset)
        );
    }

    for refused in [
        "+0000000000000000001",
        "-0000000000000000001",
        "0000000000000000001",
        "000000000000000000001",
        "99999999999999999999",
        "00000000000000000001.log",
        "00000000000000000001.gap.gap",
        "0000000000000000000x",
        "",
        ".gap",
    ] {
        assert_eq!(layout::parse_chunk_name(refused), None, "{refused:?}");
    }

    assert_eq!(OFFSET_WIDTH, 20);
    assert_eq!(MAX_CHUNK_BYTES, 1_048_576);
    assert_eq!(MAX_PUT_BYTES, 67_108_864);
    assert_eq!(FIRST_BYTE_AT_MS, "first-byte-at-ms");
    assert_eq!(GAP_SUFFIX, ".gap");
}

/// Asserts that every base and job id the rules refuse is refused with the rule it breaks, that
/// each key built from an accepted pair begins with `P`, and that `P` holds no character of a
/// resource pattern. Catches a key that names another job's objects or a prefix that widens a
/// session policy's pattern.
#[test]
fn a_job_prefix_cannot_escape() {
    let long_base = format!("{}/b", "a".repeat(MAX_PREFIX_LEN - 1));
    let refused_bases = [
        "..", ".", "a/../b", "a/./b", "/a", "a/", "a//b", "a\nb", "a\u{7f}b", "a*", "a?b", "$a",
        "{a}", "a b", "\u{e9}",
    ];
    for base in refused_bases {
        assert_eq!(
            JobPrefix::new(base, "job"),
            Err(LayoutError::Prefix),
            "{base:?}"
        );
    }
    assert_eq!(
        JobPrefix::new(&long_base, "job"),
        Err(LayoutError::PrefixTooLong)
    );
    let at_limit = format!("{}/b", "a".repeat(MAX_PREFIX_LEN - 2));
    assert!(JobPrefix::new(&at_limit, "job").is_ok());

    let long_id = "j".repeat(MAX_JOB_ID_LEN + 1);
    for job_id in [
        ".",
        "..",
        "a/b",
        "_job",
        "-job",
        ".job",
        "",
        long_id.as_str(),
        "j*",
        "j?",
    ] {
        assert_eq!(
            JobPrefix::new("js", job_id),
            Err(LayoutError::JobId),
            "{job_id:?}"
        );
    }
    let max_id = "j".repeat(MAX_JOB_ID_LEN);

    let accepted = [
        ("", "build-42"),
        ("js", "build-42"),
        ("logs/js.v1/run_9", "Job.7-x_y"),
        ("a", max_id.as_str()),
        ("..a/b..", "0"),
    ];
    for (base, job_id) in accepted {
        let job = JobPrefix::new(base, job_id).unwrap();
        let p = job.as_str();
        let expected = match base.is_empty() {
            true => format!("{job_id}/"),
            false => format!("{base}/{job_id}/"),
        };
        assert_eq!(p, expected);
        assert!(!p.contains(['*', '?', '$', '{', '}', '[', ']']), "{p}");
        for stream in [Stream::Stdout, Stream::Stderr] {
            for key in [
                job.chunk_key(stream, 7),
                job.gap_key(stream, 7),
                job.final_key(stream),
                job.stream_prefix(stream),
            ] {
                assert!(key.starts_with(p), "{key}");
                assert!(!key[p.len()..].contains(".."), "{key}");
            }
        }
        assert_eq!(JobPrefix::parse(p), Ok(job));
    }

    assert_eq!(JobPrefix::parse("js/build-42"), Err(LayoutError::JobPrefix));
    assert_eq!(JobPrefix::parse("/"), Err(LayoutError::JobId));
    assert_eq!(JobPrefix::parse("js//"), Err(LayoutError::JobId));
    assert_eq!(JobPrefix::parse("../job/"), Err(LayoutError::Prefix));
}

/// Asserts the gap marker's body: exactly `{"length":N}` written, the same read back with the
/// marker's offset, and a body that is not one object with only an integer `length` of at least
/// 1, or a gap past `u64::MAX`, refused.
#[test]
fn a_gap_marker_body_is_exactly_its_length() {
    let gap = Gap::new(67_108_864, 6_291_456).unwrap();
    assert_eq!((gap.offset(), gap.length()), (67_108_864, 6_291_456));
    assert_eq!(layout::encode_gap(&gap), b"{\"length\":6291456}");
    assert_eq!(
        layout::decode_gap(67_108_864, b"{\"length\":6291456}"),
        Ok(gap)
    );
    assert_eq!(Gap::new(u64::MAX - 1, 1).map(|gap| gap.length()), Ok(1));

    assert_eq!(Gap::new(5, 0), Err(LayoutError::Gap));
    assert_eq!(Gap::new(u64::MAX, 1), Err(LayoutError::Gap));
    assert_eq!(
        layout::decode_gap(0, b"{\"length\":0}"),
        Err(LayoutError::Gap)
    );
    assert_eq!(
        layout::decode_gap(u64::MAX, b"{\"length\":1}"),
        Err(LayoutError::Gap)
    );
    for body in [
        &b"{\"length\":1,\"more\":2}"[..],
        b"{\"length\":-1}",
        b"{\"length\":\"1\"}",
        b"{\"length\":1.5}",
        b"{}",
        b"{\"len\":1}",
        b"[1]",
        b"not json",
        b"",
    ] {
        assert_eq!(
            layout::decode_gap(0, body),
            Err(LayoutError::GapBody),
            "{}",
            String::from_utf8_lossy(body)
        );
    }
}

/// Asserts the text of every layout error: each names the rule broken and no input.
#[test]
fn layout_errors_name_the_rule() {
    let texts = [
        (
            LayoutError::PrefixTooLong,
            "the log prefix is longer than 512 bytes",
        ),
        (
            LayoutError::Prefix,
            "the log prefix must be `/`-separated segments of [A-Za-z0-9._-], none empty, `.` or `..`",
        ),
        (
            LayoutError::JobId,
            "the job id must be 1 to 63 bytes of [A-Za-z0-9._-] beginning with a letter or digit",
        ),
        (
            LayoutError::JobPrefix,
            "a job prefix is `prefix/job_id/` or `job_id/`",
        ),
        (
            LayoutError::Gap,
            "a gap is at least one byte long and ends at most at u64::MAX",
        ),
        (
            LayoutError::GapBody,
            "a gap marker's body must be {\"length\":N}",
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
    assert_eq!(Stream::Stdout.name(), "stdout");
    assert_eq!(Stream::Stderr.name(), "stderr");
}
