//! The layout of the rows, pinned byte for byte.

use super::*;

// Catches a change to the bytes of a row: the format byte, a tag's number, the tag order,
// the width of a tag, a length or a time, or the framing of a list or map. The rows are
// written out here with literal tags and codes, independent of the codec's constants, so a
// renumbering done consistently in the encoder and decoder still fails.
#[test]
fn the_layout_is_pinned_byte_for_byte() {
    let until = [vec![1], seconds(8)].concat();
    let bars = framed(&[b"host-b", &until, b"host-c", &[2]]);
    let job = literal_row(&[
        (1, b"full-1".to_vec()),
        (
            2,
            format!("team/app:1@sha256:{}", "a".repeat(64)).into_bytes(),
        ),
        (3, framed(&[b"run"])),
        (4, framed(&[b"--fast", b"--seed=7"])),
        (5, framed(&[b"LEVEL", b"2", b"MODE", b"test"])),
        (7, 2000u64.to_be_bytes().to_vec()),
        (8, (1u64 << 30).to_be_bytes().to_vec()),
        (9, 1u64.to_be_bytes().to_vec()),
        (10, b"accel-a".to_vec()),
        (11, (1u64 << 32).to_be_bytes().to_vec()),
        (12, b"linux".to_vec()),
        (13, b"x86_64".to_vec()),
        (14, vec![0]),
        (15, framed(&[b"driver", b"550"])),
        (16, 600u64.to_be_bytes().to_vec()),
        (17, vec![1]),
        (18, vec![0x1f, 0x90, 0, 80]),
        (19, framed(&[b"inputs/upload", b"upload-target"])),
        (
            20,
            framed(&[b"jobscheduler.dev/run", b"run-1", b"team", b"infra"]),
        ),
        (21, (-4i32).to_be_bytes().to_vec()),
        (22, vec![3]),
        (23, seconds(1)),
        (24, seconds(2)),
        (25, seconds(3)),
        (26, seconds(4)),
        (27, b"host-a".to_vec()),
        (28, vec![1]),
        (29, seconds(5)),
        (30, seconds(6)),
        (31, vec![1]),
        (32, (-9i32).to_be_bytes().to_vec()),
        (33, b"out".to_vec()),
        (34, b"err".to_vec()),
        (35, vec![1]),
        (36, seconds(1)),
        (37, seconds(2)),
        (38, seconds(3)),
        (39, seconds(4)),
        (40, [seconds(7), 4096u64.to_be_bytes().to_vec()].concat()),
        (41, seconds(7)),
        (42, b"ci-1".to_vec()),
        (
            43,
            [2_621_440u64, 100, 1, 2]
                .iter()
                .flat_map(|word| word.to_be_bytes())
                .collect(),
        ),
        (45, 2u32.to_be_bytes().to_vec()),
        (46, bars),
    ]);
    // The rootfs fields (tags 54 to 58) are pinned by `rootfs_fields`' own tests.
    let full = super::rootfs_fields::tests::plain_job();
    assert_eq!(job_row(&full), job);
    assert_eq!(decode_job(&job), Ok(full));

    let shared = literal_row(&[
        (1, b"full-1".to_vec()),
        (2, vec![7; 16]),
        (3, b"host-a".to_vec()),
        (5, 1u64.to_be_bytes().to_vec()),
        (6, 2u64.to_be_bytes().to_vec()),
        (7, 3u64.to_be_bytes().to_vec()),
        (8, 4u64.to_be_bytes().to_vec()),
        (9, vec![2]),
        (10, seconds(1)),
        (11, seconds(2)),
        (12, vec![0]),
    ]);
    assert_eq!(allocation_row(&allocation(shape())), shared);
    assert_eq!(decode_allocation(&shared), Ok(allocation(shape())));

    let whole = literal_row(&[
        (1, b"full-1".to_vec()),
        (2, vec![7; 16]),
        (3, b"host-a".to_vec()),
        (4, Vec::new()),
        (9, vec![2]),
        (10, seconds(1)),
        (11, seconds(2)),
        (12, vec![0]),
    ]);
    assert_eq!(allocation_row(&allocation(Booked::WholeHost)), whole);

    let ported = AllocationRecord {
        host_ports: vec![80, 8080],
        ..allocation(shape())
    };
    let ported_row = literal_row(&[
        (1, b"full-1".to_vec()),
        (2, vec![7; 16]),
        (3, b"host-a".to_vec()),
        (5, 1u64.to_be_bytes().to_vec()),
        (6, 2u64.to_be_bytes().to_vec()),
        (7, 3u64.to_be_bytes().to_vec()),
        (8, 4u64.to_be_bytes().to_vec()),
        (9, vec![2]),
        (10, seconds(1)),
        (11, seconds(2)),
        (12, vec![0]),
        (13, vec![0, 80, 0x1f, 0x90]),
    ]);
    assert_eq!(allocation_row(&ported), ported_row);
    assert_eq!(decode_allocation(&ported_row), Ok(ported));

    let waiting = AllocationRecord {
        prepare_deadline: Some(at(9)),
        wall_limit_secs: Some(1800),
        ..allocation(Booked::WholeHost)
    };
    let waiting_row = literal_row(&[
        (1, b"full-1".to_vec()),
        (2, vec![7; 16]),
        (3, b"host-a".to_vec()),
        (4, Vec::new()),
        (9, vec![2]),
        (10, seconds(1)),
        (11, seconds(2)),
        (12, vec![0]),
        (14, seconds(9)),
        (15, 1800u64.to_be_bytes().to_vec()),
    ]);
    assert_eq!(allocation_row(&waiting), waiting_row);
    assert_eq!(decode_allocation(&waiting_row), Ok(waiting));

    let bare = job_row(&bare_job());
    assert_eq!(
        field_of(&bare, 18),
        Some(Vec::new()),
        "an empty list is written empty"
    );
    assert_eq!(field_of(&bare, 24), None, "None is not written");
    assert_eq!(field_of(&bare, 45), None, "a count of zero is not written");
    assert_eq!(field_of(&bare, 46), None, "no bars are not written");
    assert_eq!(field_of(&bare, 6), Some(Vec::new()), "a marker is empty");
    for tag in (31..=40).chain([job_tag::RESULT_LOGS, 58]) {
        assert_eq!(
            field_of(&bare, tag),
            None,
            "a job with no result has no field {tag}"
        );
    }
}
