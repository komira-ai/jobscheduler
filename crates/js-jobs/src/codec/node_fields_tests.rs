//! The node row codec: round trips with every field set and with none of the optional ones,
//! the layout byte for byte, the fields a later build adds, the defaults of a row that holds
//! only the required fields, and every way a node row can fail to parse.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::Phase;
use js_state::StoreTime;
use js_types::{HostId, Resources, Shape};

use super::{decode_node, encode_node};
use crate::codec::tests::{at, framed, with, without};
use crate::codec::{Carried, DecodeError, FORMAT, LATER_FORMAT, field_of};
use crate::ids::JobId;
use crate::node::{AppliedBeat, LoadReading, NodeRecord, NodeState, SessionDigest};

fn host(text: &str) -> HostId {
    HostId::new(text).expect("a host")
}

fn job(text: &str) -> JobId {
    JobId::parse(text).expect("an id")
}

fn pairs(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// A claim with GPUs, a class, disk and capabilities.
fn claim() -> Shape {
    Shape::new(
        Resources::new(2000, 4096, 2, Some("a100".to_owned()))
            .expect("a resource vector")
            .with_disk_bytes(9),
        pairs(&[("zone", "a")]),
    )
}

/// A booked shape, different from the claim in every field.
fn booked() -> Shape {
    Shape::new(
        Resources::new(4000, 8192, 1, Some("h100".to_owned()))
            .expect("a resource vector")
            .with_disk_bytes(7),
        pairs(&[("kind", "big"), ("zone", "b")]),
    )
}

fn load() -> LoadReading {
    LoadReading {
        seq: 41,
        mono_usec: 42,
        cpu_usage_usec: 43,
        working_set_bytes: 44,
        active_file_bytes: 45,
        actions_cpu_limited: true,
    }
}

/// A record with every field set.
fn full_node() -> NodeRecord {
    NodeRecord {
        host: host("host-a"),
        epoch: 3,
        session: Some(SessionDigest::new([7; 32])),
        pool: "pool-1".to_owned(),
        claim: Some(claim()),
        shape: Some(booked()),
        labels: pairs(&[("os", "linux"), ("rack", "r1")]),
        state: NodeState {
            phase: Phase::Draining,
            load: Some(load()),
            first_listing_pending: true,
            last_listing_clean: false,
            missing_in_a_row: BTreeMap::from([(job("job-1"), 1), (job("job-2"), 2)]),
        },
        last_beat: Some(AppliedBeat {
            epoch: 3,
            seq: Some(12),
        }),
        heard_at: StoreTime::from_duration(Duration::new(10, 5)),
        carried: Carried(BTreeMap::from([(77, vec![1, 2, 3])])),
    }
}

/// A record with none of the optional fields set and the flags at their defaults.
fn bare_node() -> NodeRecord {
    NodeRecord {
        host: host("host-b"),
        epoch: 1,
        session: None,
        pool: "pool-2".to_owned(),
        claim: None,
        shape: None,
        labels: BTreeMap::new(),
        state: NodeState {
            phase: Phase::Serving,
            load: None,
            first_listing_pending: false,
            last_listing_clean: true,
            missing_in_a_row: BTreeMap::new(),
        },
        last_beat: None,
        heard_at: at(2),
        carried: Carried::default(),
    }
}

fn node_row(node: &NodeRecord) -> Vec<u8> {
    encode_node(node).expect("encodes")
}

fn number(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// A time as a row holds it.
fn time(secs: u64, nanos: u32) -> Vec<u8> {
    let mut out = number(secs);
    out.extend_from_slice(&nanos.to_be_bytes());
    out
}

/// A row of format 1 holding `fields` in the order given.
fn literal_row(fields: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![1];
    for (tag, value) in fields {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&u32::try_from(value.len()).expect("small").to_be_bytes());
        out.extend_from_slice(value);
    }
    out
}

/// A row holding only the required fields.
fn required_only() -> Vec<u8> {
    literal_row(&[
        (1, b"host-c".to_vec()),
        (2, number(5)),
        (4, b"pool-3".to_vec()),
        (5, time(6, 0)),
    ])
}

// Catches a field written and not read back (or read into another): a record with every
// field set, one with none of the optional ones, a beat without a sequence, a load without a
// CPU limit and each phase read back as written.
#[test]
fn a_node_reads_back_as_written() {
    let full = full_node();
    assert_eq!(decode_node(&node_row(&full)), Ok(full.clone()));
    let bare = bare_node();
    assert_eq!(decode_node(&node_row(&bare)), Ok(bare.clone()));

    let mut no_seq = full.clone();
    no_seq.last_beat = Some(AppliedBeat {
        epoch: 9,
        seq: None,
    });
    assert_eq!(decode_node(&node_row(&no_seq)), Ok(no_seq));

    let mut unlimited = full.clone();
    unlimited.state.load = Some(LoadReading {
        actions_cpu_limited: false,
        ..load()
    });
    assert_eq!(decode_node(&node_row(&unlimited)), Ok(unlimited));

    for phase in [
        Phase::Serving,
        Phase::Preparing,
        Phase::Restoring,
        Phase::MemoryPressure,
        Phase::Draining,
        Phase::Quarantined,
        Phase::Tainted,
    ] {
        let mut node = bare.clone();
        node.state.phase = phase;
        assert_eq!(decode_node(&node_row(&node)), Ok(node), "phase {phase:?}");
    }
}

// Catches a change of the bytes an earlier build reads, a phase stored as anything but its
// wire text, and a field written for a record that does not hold it: each phase is pinned by
// its text, and a bare record writes only the always-present fields.
#[test]
fn each_phase_is_written_as_its_wire_text() {
    let texts = [
        (Phase::Serving, "serving"),
        (Phase::Preparing, "preparing"),
        (Phase::Restoring, "restoring"),
        (Phase::MemoryPressure, "memory-pressure"),
        (Phase::Draining, "draining"),
        (Phase::Quarantined, "quarantined"),
        (Phase::Tainted, "tainted"),
    ];
    for (phase, text) in texts {
        let mut node = bare_node();
        node.state.phase = phase;
        assert_eq!(
            field_of(&node_row(&node), 6),
            Some(text.as_bytes().to_vec()),
            "phase {phase:?}"
        );
    }
    assert_eq!(
        node_row(&bare_node()),
        literal_row(&[
            (1, b"host-b".to_vec()),
            (2, number(1)),
            (4, b"pool-2".to_vec()),
            (5, time(2, 0)),
            (6, b"serving".to_vec()),
            (7, Vec::new()),
            (12, Vec::new()),
        ])
    );
}

// Catches a change of the bytes an earlier build reads: a record with every field set is
// written as this literal row (numeric tags, in tag order) and the literal row reads back as
// the record.
#[test]
fn the_node_layout_is_pinned() {
    let shape_fields =
        |marker: u16, cpu: u64, mem: u64, gpus: u64, class: &str, disk: u64, caps: &[&[u8]]| {
            vec![
                (marker, Vec::new()),
                (marker + 1, number(cpu)),
                (marker + 2, number(mem)),
                (marker + 3, number(gpus)),
                (marker + 4, class.as_bytes().to_vec()),
                (marker + 5, number(disk)),
                (marker + 6, framed(caps)),
            ]
        };
    let mut fields = vec![
        (1, b"host-a".to_vec()),
        (2, number(3)),
        (3, vec![7; 32]),
        (4, b"pool-1".to_vec()),
        (5, time(10, 5)),
        (6, b"draining".to_vec()),
        (7, framed(&[b"os", b"linux", b"rack", b"r1"])),
        (8, Vec::new()),
        (9, Vec::new()),
        (10, number(3)),
        (11, number(12)),
        (
            12,
            framed(&[b"job-1", &1u32.to_be_bytes(), b"job-2", &2u32.to_be_bytes()]),
        ),
    ];
    fields.extend(shape_fields(20, 2000, 4096, 2, "a100", 9, &[b"zone", b"a"]));
    fields.extend(shape_fields(
        30,
        4000,
        8192,
        1,
        "h100",
        7,
        &[b"kind", b"big", b"zone", b"b"],
    ));
    fields.extend([
        (40, Vec::new()),
        (41, number(41)),
        (42, number(42)),
        (43, number(43)),
        (44, number(44)),
        (45, number(45)),
        (46, Vec::new()),
        (77, vec![1, 2, 3]),
    ]);
    let literal = literal_row(&fields);
    let full = full_node();
    assert_eq!(literal[0], FORMAT);
    assert_eq!(node_row(&full), literal);
    assert_eq!(decode_node(&literal), Ok(full));
}

// Catches a load written without its CPU-limit marker, or a marker written for an unlimited
// load, and the two listing flags written when they hold their defaults.
#[test]
fn the_optional_markers_are_written_only_when_set() {
    let mut node = full_node();
    node.state.load = Some(LoadReading {
        actions_cpu_limited: false,
        ..load()
    });
    node.state.first_listing_pending = false;
    node.state.last_listing_clean = true;
    let row = node_row(&node);
    for tag in [8, 9, 46] {
        assert_eq!(field_of(&row, tag), None, "tag {tag}");
    }
    assert_eq!(field_of(&row, 40), Some(Vec::new()));
    assert_eq!(field_of(&node_row(&bare_node()), 40), None);
    assert_eq!(field_of(&node_row(&bare_node()), 20), None);
    assert_eq!(field_of(&node_row(&bare_node()), 30), None);
}

// Catches a row holding only the required fields read as anything but the defaults: no
// session, shape, labels, load, beat or missed container, the phase quarantined, the first
// listing applied and the last listing clean.
#[test]
fn a_row_with_only_the_required_fields_reads_as_the_defaults() {
    assert_eq!(
        decode_node(&required_only()),
        Ok(NodeRecord {
            host: host("host-c"),
            epoch: 5,
            session: None,
            pool: "pool-3".to_owned(),
            claim: None,
            shape: None,
            labels: BTreeMap::new(),
            state: NodeState {
                phase: Phase::Quarantined,
                load: None,
                first_listing_pending: false,
                last_listing_clean: true,
                missing_in_a_row: BTreeMap::new(),
            },
            last_beat: None,
            heard_at: at(6),
            carried: Carried::default(),
        })
    );
}

// Catches a marker with no fields after it read as a failure or as something but zeros: a
// shape marker alone is a zero shape with no capabilities, a load marker alone is a zero
// load, and a shape with a class but no GPU count is refused.
#[test]
fn a_marker_alone_reads_as_zeros() {
    let base = required_only();
    let read = decode_node(&with(&with(&base, 20, &[]), 40, &[])).expect("reads");
    let zero = Shape::new(
        Resources::new(0, 0, 0, None).expect("a resource vector"),
        BTreeMap::new(),
    );
    assert_eq!(read.claim, Some(zero));
    assert_eq!(read.shape, None);
    assert_eq!(read.state.load, Some(LoadReading::default()));
    let class_only = with(&with(&base, 30, &[]), 34, b"a100");
    assert_eq!(
        decode_node(&class_only),
        Err(DecodeError::BadValue { tag: 33 })
    );
}

// Catches a later build's field dropped on a rewrite, a phase this build does not know read
// as serving, and a row of a later format taken for this one's: an unknown tag is carried and
// written back with the same bytes, an unknown phase reads quarantined and a later format byte
// is Newer.
#[test]
fn a_node_row_is_additive() {
    let row = with(&node_row(&bare_node()), 99, &[4, 5, 6]);
    let read = decode_node(&row).expect("a later field is carried");
    assert_eq!(read.carried, Carried(BTreeMap::from([(99, vec![4, 5, 6])])));
    let rewritten = node_row(&read);
    assert_eq!(rewritten, row, "the rewrite is not the row it read");
    assert_eq!(field_of(&rewritten, 99), Some(vec![4, 5, 6]));

    let unknown = with(&node_row(&bare_node()), 6, b"hovering");
    assert_eq!(
        decode_node(&unknown).map(|node| node.state.phase),
        Ok(Phase::Quarantined)
    );
    assert_eq!(decode_node(&[LATER_FORMAT]), Err(DecodeError::Newer));
}

// Catches a row missing a required field taken for a node: each of the host, epoch, pool and
// time heard is the error that names it, and a beat sequence without its epoch is refused.
#[test]
fn a_required_field_the_row_lacks_is_refused() {
    let row = node_row(&full_node());
    for tag in [1, 2, 4, 5] {
        assert_eq!(
            decode_node(&without(&row, tag)),
            Err(DecodeError::Missing { tag }),
            "without field {tag}"
        );
    }
    assert_eq!(
        decode_node(&without(&row, 10)),
        Err(DecodeError::Missing { tag: 10 })
    );
    assert_eq!(decode_node(&[]), Err(DecodeError::Empty));
}

// Catches a missed-container map that does not parse taken for one: a count that is not four
// bytes, a job named twice, a key that is not a job id, a key that is not UTF-8 and a key
// without its count are each the error that names the field.
#[test]
fn every_malformed_missing_map_is_an_error() {
    let row = node_row(&full_node());
    let one = 1u32.to_be_bytes();
    let cases: [(&[&[u8]], DecodeError); 6] = [
        (&[b"job-1", &[0, 0, 1]], DecodeError::BadLength { tag: 12 }),
        (
            &[b"job-1", &one, b"job-1", &one],
            DecodeError::BadValue { tag: 12 },
        ),
        (&[b"job 1", &one], DecodeError::BadValue { tag: 12 }),
        (&[&[0xff], &one], DecodeError::NotUtf8 { tag: 12 }),
        (&[b"job-1"], DecodeError::BadValue { tag: 12 }),
        (&[b"", &one], DecodeError::BadValue { tag: 12 }),
    ];
    for (items, error) in cases {
        assert_eq!(
            decode_node(&with(&row, 12, &framed(items))),
            Err(error),
            "missing {items:?}"
        );
    }
}

// Catches a shape whose GPUs and class disagree taken for a shape: a claim and a booked
// shape with GPUs and no class, or a class and no GPUs, are BadValue at their GPU tag.
#[test]
fn a_shape_whose_gpus_and_class_disagree_is_refused() {
    let row = node_row(&full_node());
    for (gpus, class) in [(23, 24), (33, 34)] {
        assert_eq!(
            decode_node(&without(&row, class)),
            Err(DecodeError::BadValue { tag: gpus }),
            "gpus without a class at {gpus}"
        );
        assert_eq!(
            decode_node(&with(&row, gpus, &number(0))),
            Err(DecodeError::BadValue { tag: gpus }),
            "a class without gpus at {gpus}"
        );
    }
}

// Catches a marker that holds a value read as set: each marker with a byte in it is BadValue
// at its tag.
#[test]
fn a_marker_with_a_value_is_refused() {
    let row = node_row(&full_node());
    for tag in [8, 9, 20, 30, 40, 46] {
        assert_eq!(
            decode_node(&with(&row, tag, &[1])),
            Err(DecodeError::BadValue { tag }),
            "marker {tag}"
        );
    }
}

// Catches a field of the wrong width or text taken for a value: each width, digest, host,
// pool, phase, labels and time that does not parse is the error that names its tag.
#[test]
fn a_value_that_is_not_valid_for_its_field_is_refused_by_tag() {
    let row = node_row(&full_node());
    let bad_length = [
        (2, vec![0; 7]),
        (3, vec![7; 31]),
        (3, vec![7; 33]),
        (5, vec![0; 11]),
        (10, vec![0; 4]),
        (11, vec![0; 9]),
        (21, vec![0; 3]),
        (22, vec![0; 9]),
        (25, Vec::new()),
        (31, vec![0; 1]),
        (35, vec![0; 2]),
        (41, vec![0; 7]),
        (42, vec![0; 7]),
        (43, vec![0; 7]),
        (44, vec![0; 7]),
        (45, vec![0; 7]),
    ];
    for (tag, value) in bad_length {
        assert_eq!(
            decode_node(&with(&row, tag, &value)),
            Err(DecodeError::BadLength { tag }),
            "field {tag}"
        );
    }
    for tag in [4, 6, 24, 34] {
        assert_eq!(
            decode_node(&with(&row, tag, &[0xff])),
            Err(DecodeError::NotUtf8 { tag }),
            "field {tag}"
        );
    }
    let bad_value = [
        (1, b"host a".to_vec()),
        (5, time(1, 1_000_000_000)),
        (7, framed(&[b"only-a-key"])),
        (7, framed(&[b"k", b"v", b"k", b"w"])),
        (26, vec![0, 0, 0, 9]),
        (36, framed(&[b"k"])),
    ];
    for (tag, value) in bad_value {
        assert_eq!(
            decode_node(&with(&row, tag, &value)),
            Err(DecodeError::BadValue { tag }),
            "field {tag}"
        );
    }
}
