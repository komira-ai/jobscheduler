//! The file-system fields of a job row: the layers, mounts and host ports, the agent's code of
//! a refused start, the code of an input that failed, and the format byte of a job without an
//! image; their round trips, what a job without them writes, and every way their values can
//! fail to parse.

use js_agent_wire::{JobMount, Layer, MountMode, MountSource, Propagation};

use super::{FORMAT_NO_IMAGE, job_rootfs_tag as t};
use crate::codec::tests::{bare_job, full_job, job_row, with, without};
use crate::codec::{
    DecodeError, FORMAT, decode_job, field_of, items_of, job_tag, list, rearranged,
};
use crate::record::{JobRecord, JobState, StartFailure};

/// A list value of `items`.
fn items(items: &[&[u8]]) -> Vec<u8> {
    list(items.iter().copied())
}

/// [`full_job`] without layers, mounts, host ports, refused start's code or input failure.
pub(crate) fn plain_job() -> JobRecord {
    let mut job = full_job();
    job.spec.layers.clear();
    job.spec.mounts.clear();
    job.spec.host_ports.clear();
    job.start_failure_detail = None;
    if let Some(result) = job.result.as_mut() {
        result.input_failure = None;
    }
    job
}

/// [`full_job`] with no image: a root of layers alone.
fn imageless_job() -> JobRecord {
    let mut job = full_job();
    job.spec.image = None;
    job
}

/// The parts of a read-only host-path mount of `/srv/a` at `/work/a`, with no option set.
const PARTS: [&[u8]; 6] = [b"\x00/srv/a", b"/work/a", &[0], &[], &[0], &[0]];

/// `PARTS` with the part at `at` replaced by `part`.
fn parts_with(at: usize, part: &[u8]) -> Vec<&[u8]> {
    let mut parts = PARTS.to_vec();
    parts[at] = part;
    parts
}

// Catches a field written and not read back, or read into another: a job with two layers of
// each kind, a copy-on-write mount with a sub_path and no_exec, a read-write one with neither,
// two host ports, a refused start's code and an input failure reads back as written.
#[test]
fn every_rootfs_field_reads_back_as_written() {
    let job = full_job();
    assert!(!job.spec.layers.is_empty() & !job.spec.mounts.is_empty());
    assert!(!job.spec.host_ports.is_empty() & job.start_failure_detail.is_some());
    assert_eq!(decode_job(&job_row(&job)), Ok(job));
}

// Catches a job that names none of the fields writing a tag for them, so that its row would
// not be the bytes a build before them wrote: the row of a job without them is the full row
// without tags 54 to 58, and a bare job writes none of them.
#[test]
fn a_job_without_the_fields_writes_the_row_it_wrote_before() {
    let full = job_row(&full_job());
    let stripped = [
        t::LAYERS,
        t::MOUNTS,
        t::HOST_PORTS,
        t::START_FAILURE_DETAIL,
        t::RESULT_INPUT_FAILURE,
    ]
    .iter()
    .fold(full, |row, tag| without(&row, *tag));
    assert_eq!(stripped, job_row(&plain_job()));
    let bare = job_row(&bare_job());
    for tag in 54..=58 {
        assert_eq!(field_of(&bare, tag), None, "tag {tag}");
    }
}

// Catches a row of a build from before the fields read with any of them: such a row reads with
// no layer, mount or host port, no refused start's code and no input failure.
#[test]
fn a_row_from_before_the_fields_reads_with_none() {
    let row = job_row(&plain_job());
    let job = decode_job(&row).expect("reads");
    assert_eq!(job, plain_job());
    let refused = JobRecord {
        state: JobState::FailedToStart(StartFailure::AgentRefused),
        ..plain_job()
    };
    let job = decode_job(&job_row(&refused)).expect("reads");
    assert_eq!(job.start_failure_detail, None);
}

// Catches the row of a job without an image written with the format byte a build before the
// field reads as its own (it would then fail the row, and a listing with it, as corrupt), and
// such a row not read back.
#[test]
fn a_job_without_an_image_is_written_with_the_second_format_byte() {
    let job = imageless_job();
    let row = job_row(&job);
    assert_eq!(row.first(), Some(&FORMAT_NO_IMAGE));
    assert_eq!(FORMAT_NO_IMAGE, 2);
    assert_eq!(field_of(&row, job_tag::IMAGE), None);
    assert_eq!(decode_job(&row), Ok(job));
    assert_eq!(job_row(&full_job()).first(), Some(&FORMAT));
}

// Catches a format-1 row without an image read as a job without one (a build from before the
// field never wrote one), a second-format row that holds an image refused, and a field
// rewritten in a second-format row losing its format byte.
#[test]
fn only_the_second_format_byte_may_lack_an_image() {
    let row = without(&job_row(&full_job()), job_tag::IMAGE);
    assert_eq!(row.first(), Some(&FORMAT));
    assert_eq!(
        decode_job(&row),
        Err(DecodeError::Missing {
            tag: job_tag::IMAGE
        })
    );
    let mut second = job_row(&full_job());
    second[0] = FORMAT_NO_IMAGE;
    assert_eq!(decode_job(&second), Ok(full_job()));
    let imageless = job_row(&imageless_job());
    let edited = with(&imageless, t::START_FAILURE_DETAIL, b"input-unavailable");
    assert_eq!(edited.first(), Some(&FORMAT_NO_IMAGE));
    let rearranged = rearranged(&imageless, &[]).expect("parses");
    assert_eq!(decode_job(&rearranged), Ok(imageless_job()));
}

// Catches a kind, a mode, a propagation or a protocol of a later build read as one this build
// knows, and a mount or host port with an option this build does not read taken without it:
// each is a later build's row.
#[test]
fn a_kind_value_or_option_this_build_does_not_know_is_newer() {
    let row = job_row(&plain_job());
    let later = [
        (t::LAYERS, items(&[b"\x03/srv/a"])),
        (t::MOUNTS, items(&[&items(&parts_with(0, b"\x01/srv/a"))])),
        (t::MOUNTS, items(&[&items(&parts_with(0, b"\x03/srv/a"))])),
        (t::MOUNTS, items(&[&items(&parts_with(2, &[3]))])),
        (t::MOUNTS, items(&[&items(&parts_with(5, &[1]))])),
        (
            t::MOUNTS,
            items(&[&items(&[PARTS.as_slice(), &[&[0]]].concat())]),
        ),
        (t::HOST_PORTS, items(&[&[0, 80, 0, 0, 1]])),
        (t::HOST_PORTS, items(&[&[0, 80, 0, 0, 0, 0]])),
    ];
    for (tag, value) in later {
        assert_eq!(
            decode_job(&with(&row, tag, &value)),
            Err(DecodeError::Newer),
            "tag {tag}: {value:?}"
        );
    }
}

// Catches a malformed value read as something: an empty layer, a mount of too few parts, a
// sub_path or no_exec of a value no build writes, a part that is not one byte, a host port
// that is too short, text that is not UTF-8, and a list that ends inside an item. Each names
// its tag.
#[test]
fn a_malformed_value_is_refused_by_its_tag() {
    let row = job_row(&plain_job());
    let bad = |tag| DecodeError::BadValue { tag };
    let length = |tag| DecodeError::BadLength { tag };
    let utf8 = |tag| DecodeError::NotUtf8 { tag };
    let cases = [
        (t::LAYERS, items(&[b""]), bad(t::LAYERS)),
        (t::LAYERS, items(&[b"\x00\xff"]), utf8(t::LAYERS)),
        (t::LAYERS, vec![0, 0, 0, 9, 0], bad(t::LAYERS)),
        (t::MOUNTS, items(&[&items(&PARTS[..5])]), bad(t::MOUNTS)),
        (t::MOUNTS, items(&[b"\x00\x00\x00\x09"]), bad(t::MOUNTS)),
        (
            t::MOUNTS,
            items(&[&items(&parts_with(0, b""))]),
            bad(t::MOUNTS),
        ),
        (
            t::MOUNTS,
            items(&[&items(&parts_with(1, b"\xff"))]),
            utf8(t::MOUNTS),
        ),
        (
            t::MOUNTS,
            items(&[&items(&parts_with(2, &[0, 0]))]),
            length(t::MOUNTS),
        ),
        (
            t::MOUNTS,
            items(&[&items(&parts_with(3, b"\x02lib"))]),
            bad(t::MOUNTS),
        ),
        (
            t::MOUNTS,
            items(&[&items(&parts_with(4, &[2]))]),
            bad(t::MOUNTS),
        ),
        (
            t::MOUNTS,
            items(&[&items(&parts_with(5, &[]))]),
            length(t::MOUNTS),
        ),
        (
            t::HOST_PORTS,
            items(&[&[0, 80, 0, 0]]),
            length(t::HOST_PORTS),
        ),
        (
            t::START_FAILURE_DETAIL,
            vec![0xff],
            utf8(t::START_FAILURE_DETAIL),
        ),
        (
            t::RESULT_INPUT_FAILURE,
            vec![0xff],
            utf8(t::RESULT_INPUT_FAILURE),
        ),
    ];
    for (tag, value, error) in cases {
        assert_eq!(
            decode_job(&with(&row, tag, &value)),
            Err(error),
            "tag {tag}: {value:?}"
        );
    }
}

// Catches an empty sub_path told from an absent one, and a job port of 0 (absent) read as a
// port: each reads back as written.
#[test]
fn an_empty_sub_path_and_an_absent_job_port_read_back_as_written() {
    let mut job = full_job();
    job.spec.mounts[0].sub_path = Some(String::new());
    job.spec.mounts[1].sub_path = None;
    job.spec.host_ports[0].container_port = None;
    let row = job_row(&job);
    assert_eq!(decode_job(&row), Ok(job));
    let ports = field_of(&row, t::HOST_PORTS).expect("written");
    assert_eq!(ports[4..9], [0x1b, 0x9e, 0, 0, 0]);
}

// Catches an input failure read without a result (it is a result field, read only beside the
// reason), and kept through a rewrite all the same.
#[test]
fn an_input_failure_is_read_only_beside_a_result() {
    let row = with(
        &job_row(&bare_job()),
        t::RESULT_INPUT_FAILURE,
        b"input-unreadable",
    );
    let job = decode_job(&row).expect("reads");
    assert_eq!(job.result, None);
    assert_eq!(
        field_of(&job_row(&job), t::RESULT_INPUT_FAILURE),
        Some(b"input-unreadable".to_vec())
    );
}

/// `items` framed as a list value: each item's length (`u32`, big-endian), then its bytes.
fn framed(items: &[&[u8]]) -> Vec<u8> {
    items
        .iter()
        .flat_map(|item| [&(item.len() as u32).to_be_bytes()[..], item].concat())
        .collect()
}

// Catches a change to the bytes of tags 54 to 58: a tag's number, a kind, mode or option
// byte, the order of a mount's parts, the width of a host port, or the framing of a list. The
// values are written out here with literal tags and bytes, independent of the codec.
#[test]
fn the_rootfs_fields_are_pinned_byte_for_byte() {
    let image = [
        b"\x01team/base@sha256:".as_slice(),
        "d".repeat(64).as_bytes(),
    ]
    .concat();
    let src = [
        b"\x00/srv/trees/src".as_slice(),
        b"/work",
        &[2],
        b"\x01lib",
        &[1],
        &[0],
    ];
    let cache = [
        b"\x00/srv/cache".as_slice(),
        b"/work/cache",
        &[1],
        &[],
        &[0],
        &[0],
    ];
    let pinned: [(u16, Vec<u8>); 5] = [
        (54, framed(&[&image, b"\x00/srv/trees/sdk"])),
        (55, framed(&[&framed(&src), &framed(&cache)])),
        (
            56,
            framed(&[&[0x1b, 0x9e, 0x1c, 0x03, 0], &[0x23, 0x28, 0x23, 0x28, 0]]),
        ),
        (57, b"input-not-allowed".to_vec()),
        (58, b"input-unreadable".to_vec()),
    ];
    let row = job_row(&full_job());
    for (tag, value) in pinned {
        assert_eq!(field_of(&row, tag), Some(value.clone()), "tag {tag}");
        let rebuilt = with(&job_row(&plain_job()), tag, &value);
        assert_eq!(field_of(&rebuilt, tag), Some(value), "tag {tag}");
    }
    let rebuilt = [54, 55, 56, 57, 58]
        .iter()
        .fold(job_row(&plain_job()), |row, tag| {
            let value = field_of(&job_row(&full_job()), *tag).expect("written");
            with(&row, *tag, &value)
        });
    assert_eq!(decode_job(&rebuilt), Ok(full_job()));
}

/// A provider's layer and mount source, as a job names them.
fn provider_layer(provider: &str, reference: &str) -> Layer {
    Layer::Provider {
        provider: provider.into(),
        reference: reference.into(),
    }
}

/// A mount of the provider `provider`'s `reference` at `target`, with no option set.
fn provider_mount(provider: &str, reference: &str, target: &str) -> JobMount {
    JobMount {
        source: MountSource::Provider {
            provider: provider.into(),
            reference: reference.into(),
        },
        target: target.into(),
        mode: MountMode::ReadOnly,
        sub_path: None,
        no_exec: false,
        propagation: Propagation::Private,
    }
}

/// [`plain_job`] with `layers` and `mounts`.
fn provider_job(layers: Vec<Layer>, mounts: Vec<JobMount>) -> JobRecord {
    let mut job = plain_job();
    job.spec.layers = layers;
    job.spec.mounts = mounts;
    job
}

// Catches a provider source written and not read back, a name and a ref run together or
// split in the wrong place, and a ref that does not survive its length: a job with provider
// layers and mounts among host-path and image ones reads back as written, with a ref of
// 200 bytes (a list's length prefix would then not be UTF-8), one with non-ASCII letters, one
// that is empty and one with a NUL, which the first separator keeps whole.
#[test]
fn a_provider_layer_and_mount_read_back_as_written() {
    let long = "r".repeat(200);
    let mut job = provider_job(
        vec![
            Layer::HostPath {
                path: "/srv/a".into(),
            },
            provider_layer("tierstore", &long),
            provider_layer("scratch-2", "caf\u{e9}/\u{4e2d}"),
            provider_layer("a", ""),
            provider_layer("b", "x\0y\0"),
        ],
        vec![
            provider_mount("tierstore", &long, "/data"),
            provider_mount("scratch-2", "cache", "/data/cache"),
        ],
    );
    job.spec.mounts[1].mode = MountMode::ReadWrite;
    job.spec.mounts[1].sub_path = Some("lib".into());
    job.spec.mounts[1].no_exec = true;
    assert_eq!(decode_job(&job_row(&job)), Ok(job));
}

// Catches a change to the bytes of a provider source: its kind byte, the byte between the
// name and the ref, or the place of a mount's source. The values are written out here with
// literal bytes, independent of the codec: a layer is `\x02`, the name, a NUL and the ref; a
// mount is the six parts with that as the first.
#[test]
fn a_provider_source_is_pinned_byte_for_byte() {
    let job = provider_job(
        vec![provider_layer("tierstore", "toolchains/clang-18")],
        vec![provider_mount("tierstore", "datasets/v3", "/data")],
    );
    let row = job_row(&job);
    let layer = b"\x02tierstore\x00toolchains/clang-18";
    assert_eq!(field_of(&row, t::LAYERS), Some(framed(&[layer])));
    let parts = [
        b"\x02tierstore\x00datasets/v3".as_slice(),
        b"/data",
        &[0],
        &[],
        &[0],
        &[0],
    ];
    assert_eq!(field_of(&row, t::MOUNTS), Some(framed(&[&framed(&parts)])));
}

// Catches a provider source of no separator read as one, an empty text of a kind read as one,
// and a source that is not text read as one: each names its tag.
#[test]
fn a_malformed_provider_source_is_refused_by_its_tag() {
    let row = job_row(&plain_job());
    let bad = |tag| DecodeError::BadValue { tag };
    let utf8 = |tag| DecodeError::NotUtf8 { tag };
    let cases = [
        (t::LAYERS, items(&[b"\x02tierstore"]), bad(t::LAYERS)),
        (t::LAYERS, items(&[b"\x02"]), bad(t::LAYERS)),
        (t::LAYERS, items(&[b"\x02a\x00\xff"]), utf8(t::LAYERS)),
        (
            t::MOUNTS,
            items(&[&items(&parts_with(0, b"\x02tierstore"))]),
            bad(t::MOUNTS),
        ),
        (
            t::MOUNTS,
            items(&[&items(&parts_with(0, b"\x02a\x00\xff"))]),
            utf8(t::MOUNTS),
        ),
    ];
    for (tag, value, error) in cases {
        assert_eq!(
            decode_job(&with(&row, tag, &value)),
            Err(error),
            "tag {tag}: {value:?}"
        );
    }
}

/// The error the build before the provider kind gave an item of `LAYERS` or the source part
/// of a mount of `MOUNTS`, its `layer_of` and `mount_of` as they were: the kind byte, the
/// rest as text (a fault in it before the kind is looked at), and a kind that is not a host
/// path or, for a layer, an image is a later build's. `Ok` when it reads the item.
fn previous_build_reads(tag: u16, item: &[u8]) -> Result<(), DecodeError> {
    let (&kind, rest) = item.split_first().ok_or(DecodeError::BadValue { tag })?;
    std::str::from_utf8(rest).map_err(|_| DecodeError::NotUtf8 { tag })?;
    match (tag, kind) {
        (_, 0) | (t::LAYERS, 1) => Ok(()),
        _ => Err(DecodeError::Newer),
    }
}

// Catches a provider source a build from before kind 2 would not take for a later build's
// row: that build reads each item of the layers, and the source of each mount, as a kind and
// a text, and a kind it does not know is [`DecodeError::Newer`], so the store answers the row
// as an unknown one instead of failing it as corrupt. The refs here are of every length around
// the 128 bytes at which a list's length prefix stops being UTF-8, and each is read as `Newer`.
#[test]
fn a_build_before_the_provider_kind_reads_a_provider_row_as_newer() {
    for len in [0, 1, 127, 128, 200, 255, 256, 1024] {
        let reference = "r".repeat(len);
        let job = provider_job(
            vec![provider_layer("tierstore", &reference)],
            vec![provider_mount("tierstore", &reference, "/data")],
        );
        let row = job_row(&job);
        let layers = field_of(&row, t::LAYERS).expect("written");
        for item in items_of(t::LAYERS, &layers).expect("a list") {
            assert_eq!(
                previous_build_reads(t::LAYERS, item),
                Err(DecodeError::Newer),
                "layer, ref of {len} bytes"
            );
        }
        let mounts = field_of(&row, t::MOUNTS).expect("written");
        for item in items_of(t::MOUNTS, &mounts).expect("a list") {
            let parts = items_of(t::MOUNTS, item).expect("a list");
            assert_eq!(
                previous_build_reads(t::MOUNTS, parts[0]),
                Err(DecodeError::Newer),
                "mount, ref of {len} bytes"
            );
        }
    }
    let plain = [b"\x00/srv/a".as_slice(), b"\x01img@sha256:x"];
    for item in plain {
        assert_eq!(previous_build_reads(t::LAYERS, item), Ok(()));
    }
    assert_eq!(previous_build_reads(t::MOUNTS, b"\x00/srv/a"), Ok(()));
    assert_eq!(
        previous_build_reads(t::MOUNTS, b"\x01/srv/a"),
        Err(DecodeError::Newer)
    );
    assert_eq!(
        previous_build_reads(t::LAYERS, b""),
        Err(DecodeError::BadValue { tag: t::LAYERS })
    );
    assert_eq!(
        previous_build_reads(t::LAYERS, b"\x00\xff"),
        Err(DecodeError::NotUtf8 { tag: t::LAYERS })
    );
}

// Catches a row of a job with host-path and image sources only changing with the provider
// kind: it is the bytes the build before wrote, with no kind 2 byte at the start of any layer
// or of a mount's source.
#[test]
fn a_job_without_a_provider_writes_no_provider_kind() {
    let job = full_job();
    let row = job_row(&job);
    let layers = field_of(&row, t::LAYERS).expect("written");
    for item in items_of(t::LAYERS, &layers).expect("a list") {
        assert_ne!(item.first(), Some(&2));
    }
    let mounts = field_of(&row, t::MOUNTS).expect("written");
    for item in items_of(t::MOUNTS, &mounts).expect("a list") {
        let parts = items_of(t::MOUNTS, item).expect("a list");
        assert_ne!(parts[0].first(), Some(&2));
    }
}
