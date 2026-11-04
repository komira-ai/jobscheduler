//! The row encoding: round trips, every enum value, the layout byte for byte, the fields a
//! later build adds, and every way a row can fail to parse.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::StoreTime;
use js_types::{ExecMode, HostId, Labels};

use super::*;
use crate::suite::{full_spec, whole_host_spec};

fn at(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(secs))
}

/// A job with every optional field set.
fn full_job() -> JobRecord {
    JobRecord {
        id: JobId::parse("full-1").expect("an id"),
        spec: full_spec(),
        priority: -4,
        state: JobState::Running,
        submitted_at: at(1),
        placed_at: Some(at(2)),
        started_at: Some(at(3)),
        ended_at: Some(at(4)),
        host: Some(HostId::new("host-a").expect("a host")),
        cancel_requested: true,
        cancel_requested_at: Some(at(5)),
        retention_ends: Some(at(6)),
        carried: Carried::default(),
    }
}

/// A job with every optional field unset and every list empty.
fn bare_job() -> JobRecord {
    let mut spec = whole_host_spec();
    spec.command.clear();
    spec.args.clear();
    spec.env.clear();
    spec.constraints = Constraints::default();
    spec.network = NetworkPolicy::None;
    spec.ports.clear();
    spec.files.clear();
    spec.labels = Labels::default();
    JobRecord {
        id: JobId::parse("bare-1").expect("an id"),
        spec,
        priority: 0,
        state: JobState::Queued,
        submitted_at: StoreTime::ZERO,
        placed_at: None,
        started_at: None,
        ended_at: None,
        host: None,
        cancel_requested: false,
        cancel_requested_at: None,
        retention_ends: None,
        carried: Carried::default(),
    }
}

fn allocation(booked: Booked) -> AllocationRecord {
    AllocationRecord {
        job: JobId::parse("full-1").expect("an id"),
        allocation_id: AllocationId::from_bits([7; 16]),
        host: HostId::new("host-a").expect("a host"),
        booked,
        step: AllocationStep::Freed,
        created_at: at(1),
        freed_at: Some(at(2)),
        end_reason: Some(EndReason::Exited),
        carried: Carried::default(),
    }
}

fn shape() -> Booked {
    Booked::Shape {
        cpu_millis: 1,
        memory_bytes: 2,
        disk_bytes: 3,
        accelerators: 4,
    }
}

fn job_row(job: &JobRecord) -> Vec<u8> {
    encode_job(job).expect("encodes")
}

fn allocation_row(allocation: &AllocationRecord) -> Vec<u8> {
    encode_allocation(allocation).expect("encodes")
}

/// `row` without its field `tag`.
fn without(row: &[u8], tag: u16) -> Vec<u8> {
    let mut fields = Fields::of(row).expect("the row parses");
    fields.0.remove(&tag);
    Writer(fields.0)
        .finish(&Carried::default())
        .expect("encodes")
}

/// `row` with its field `tag` set to `value`.
fn with(row: &[u8], tag: u16, value: &[u8]) -> Vec<u8> {
    set_field(row, tag, value).expect("the row parses")
}

/// A list value of `items`.
fn items(items: &[&[u8]]) -> Vec<u8> {
    list(items.iter().copied())
}

// Catches a field written and not read back (or read into another field), on a record with
// everything set and on one with nothing optional set, for both kinds of row.
#[test]
fn a_record_reads_back_as_written() {
    for job in [full_job(), bare_job()] {
        assert_eq!(decode_job(&job_row(&job)), Ok(job));
    }
    let mut booked = allocation(Booked::WholeHost);
    booked.step = AllocationStep::Booked;
    booked.freed_at = None;
    booked.end_reason = None;
    for allocation in [allocation(shape()), allocation(Booked::WholeHost), booked] {
        assert_eq!(
            decode_allocation(&allocation_row(&allocation)),
            Ok(allocation)
        );
    }
}

// Catches two values of an enum sharing a code, or a code read back as another value.
#[test]
fn every_enum_value_reads_back() {
    use StartFailure::*;
    for state in [
        JobState::Queued,
        JobState::Placed,
        JobState::Starting,
        JobState::Running,
        JobState::Succeeded,
        JobState::Failed,
        JobState::Cancelled,
        JobState::Lost,
        JobState::FailedToStart(NoCapacity),
        JobState::FailedToStart(StartTimeout),
        JobState::FailedToStart(ExclusiveNotReady),
        JobState::FailedToStart(AgentRefused),
    ] {
        let job = JobRecord {
            state,
            ..full_job()
        };
        assert_eq!(decode_job(&job_row(&job)).map(|j| j.state), Ok(state));
    }
    for exec in [ExecMode::Container, ExecMode::SandboxExec] {
        let mut job = full_job();
        job.spec.constraints.exec = Some(exec);
        assert_eq!(decode_job(&job_row(&job)), Ok(job));
    }
    for network in [
        NetworkPolicy::None,
        NetworkPolicy::AllowList,
        NetworkPolicy::Open,
    ] {
        let mut job = full_job();
        job.spec.network = network;
        assert_eq!(decode_job(&job_row(&job)), Ok(job));
    }
    for step in [
        AllocationStep::Booked,
        AllocationStep::Releasing,
        AllocationStep::Freed,
    ] {
        let allocation = AllocationRecord {
            step,
            ..allocation(shape())
        };
        assert_eq!(
            decode_allocation(&allocation_row(&allocation)),
            Ok(allocation)
        );
    }
    for reason in [
        EndReason::Exited,
        EndReason::WallLimit,
        EndReason::Memory,
        EndReason::Cancelled,
        EndReason::HostAbsent,
        EndReason::FailedPrepare,
        EndReason::StartTimeout,
        EndReason::Refused,
    ] {
        let allocation = AllocationRecord {
            end_reason: Some(reason),
            ..allocation(shape())
        };
        assert_eq!(
            decode_allocation(&allocation_row(&allocation)),
            Ok(allocation)
        );
    }
}

// Catches a value of a later build read as some value this build knows (or refused as
// corrupt), which would let an old build act on a state it does not understand.
#[test]
fn an_enum_value_this_build_does_not_know_is_newer() {
    let job = job_row(&full_job());
    for (tag, code) in [
        (job_tag::STATE, 12),
        (job_tag::EXEC, 2),
        (job_tag::NETWORK, 3),
    ] {
        assert_eq!(
            decode_job(&with(&job, tag, &[code])),
            Err(DecodeError::Newer),
            "tag {tag}"
        );
    }
    let allocation = allocation_row(&allocation(shape()));
    for (tag, code) in [(alloc_tag::STEP, 3), (alloc_tag::END_REASON, 8)] {
        assert_eq!(
            decode_allocation(&with(&allocation, tag, &[code])),
            Err(DecodeError::Newer),
            "tag {tag}"
        );
    }
    assert_eq!(decode_job(&[FORMAT + 1]), Err(DecodeError::Newer));
}

// Catches a change to the bytes of a row: the format byte, the tag order, the width of a tag
// or a length, a time's unit, or a list's framing. A build that wrote these differently
// could not read its predecessor's rows.
#[test]
fn the_layout_is_pinned_byte_for_byte() {
    let mut allocation = allocation(Booked::WholeHost);
    allocation.allocation_id = AllocationId::from_bits([0xab; 16]);
    allocation.end_reason = None;
    let mut want = vec![FORMAT];
    let mut field = |tag: u16, value: &[u8]| {
        want.extend_from_slice(&tag.to_be_bytes());
        want.extend_from_slice(&u32::try_from(value.len()).expect("small").to_be_bytes());
        want.extend_from_slice(value);
    };
    field(alloc_tag::JOB, b"full-1");
    field(alloc_tag::ALLOCATION_ID, &[0xab; 16]);
    field(alloc_tag::HOST, b"host-a");
    field(alloc_tag::WHOLE_HOST, b"");
    field(alloc_tag::STEP, &[2]);
    field(alloc_tag::CREATED_AT, &1_000_000_000u64.to_be_bytes());
    field(alloc_tag::FREED_AT, &2_000_000_000u64.to_be_bytes());
    assert_eq!(allocation_row(&allocation), want);

    let job = JobRecord {
        spec: JobSpec {
            command: vec!["a".to_owned(), "bc".to_owned()],
            env: BTreeMap::from([("K".to_owned(), "V".to_owned())]),
            ..bare_job().spec
        },
        ..bare_job()
    };
    let row = job_row(&job);
    assert_eq!(
        field_of(&row, job_tag::COMMAND),
        Some(vec![0, 0, 0, 1, b'a', 0, 0, 0, 2, b'b', b'c'])
    );
    assert_eq!(
        field_of(&row, job_tag::ENV),
        Some(vec![0, 0, 0, 1, b'K', 0, 0, 0, 1, b'V'])
    );
    assert_eq!(
        field_of(&row, job_tag::PORTS),
        Some(Vec::new()),
        "an empty list is written empty"
    );
    assert_eq!(
        field_of(&row, job_tag::PLACED_AT),
        None,
        "None is not written"
    );
    assert_eq!(job_row(&decode_job(&row).expect("reads")), row);
}

// Catches a reader that drops a field it does not know, or a writer that does not put it
// back, so a build one release back would lose a newer build's data.
#[test]
fn a_field_this_build_does_not_know_is_written_back() {
    let job = with(&job_row(&full_job()), 900, b"later");
    let read = decode_job(&job).expect("reads");
    assert_eq!(read.carried.0, BTreeMap::from([(900, b"later".to_vec())]));
    let moved = JobRecord {
        state: JobState::Succeeded,
        ..read
    };
    assert_eq!(field_of(&job_row(&moved), 900), Some(b"later".to_vec()));

    let allocation = with(&allocation_row(&allocation(shape())), 901, b"later");
    let read = decode_allocation(&allocation).expect("reads");
    assert_eq!(
        field_of(&allocation_row(&read), 901),
        Some(b"later".to_vec())
    );
}

// Catches a field the row lacks that is refused instead of read as its default, so a row an
// older build wrote would not read.
#[test]
fn a_field_the_row_lacks_reads_as_its_default() {
    let mut row = job_row(&full_job());
    for tag in [
        job_tag::COMMAND,
        job_tag::ARGS,
        job_tag::ENV,
        job_tag::CPU_MILLIS,
        job_tag::MEMORY_BYTES,
        job_tag::GPUS,
        job_tag::GPU_CLASS,
        job_tag::DISK_BYTES,
        job_tag::CAPABILITIES,
        job_tag::NETWORK,
        job_tag::PORTS,
        job_tag::FILES,
        job_tag::LABELS,
        job_tag::PRIORITY,
        job_tag::CANCEL_REQUESTED,
    ] {
        row = without(&row, tag);
    }
    let job = decode_job(&row).expect("reads");
    assert_eq!(
        (
            job.spec.command.len(),
            job.spec.args.len(),
            job.spec.env.len(),
            job.spec.constraints.capabilities.len(),
            job.spec.ports.len(),
            job.spec.files.len(),
        ),
        (0, 0, 0, 0, 0, 0)
    );
    assert_eq!(job.spec.needs, Needs::Shape(js_types::Resources::default()));
    assert_eq!(
        (job.spec.network, job.priority, job.cancel_requested),
        (NetworkPolicy::None, 0, false)
    );
    assert_eq!(job.spec.labels, Labels::default());

    let mut row = allocation_row(&allocation(shape()));
    for tag in [
        alloc_tag::CPU_MILLIS,
        alloc_tag::MEMORY_BYTES,
        alloc_tag::DISK_BYTES,
        alloc_tag::ACCELERATORS,
    ] {
        row = without(&row, tag);
    }
    assert_eq!(
        decode_allocation(&row).map(|a| a.booked),
        Ok(Booked::Shape {
            cpu_millis: 0,
            memory_bytes: 0,
            disk_bytes: 0,
            accelerators: 0,
        })
    );
}

// Catches a framing error read as a record (or as a panic): a row too short for its format
// byte, tag, length or value, a format byte of 0, and a tag written twice.
#[test]
fn a_row_whose_framing_is_broken_says_how() {
    for (row, why) in [
        (vec![], DecodeError::Empty),
        (vec![0], DecodeError::BadFormat),
        (vec![FORMAT, 0], DecodeError::Truncated),
        (vec![FORMAT, 0, 1, 0, 0], DecodeError::Truncated),
        (vec![FORMAT, 0, 1, 0, 0, 0, 5, b'a'], DecodeError::Truncated),
        (
            vec![FORMAT, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0],
            DecodeError::DuplicateTag { tag: 1 },
        ),
    ] {
        assert_eq!(decode_job(&row), Err(why), "{row:?}");
        assert_eq!(decode_allocation(&row), Err(why), "{row:?}");
    }
}

// Catches a required field whose absence is read as a default (a job with no id, image, wall
// limit, state or submit time; an allocation with no job, id, host, step or creation time).
#[test]
fn a_row_without_a_required_field_is_refused() {
    let job = job_row(&full_job());
    for tag in [
        job_tag::ID,
        job_tag::IMAGE,
        job_tag::WALL_LIMIT_SECS,
        job_tag::STATE,
        job_tag::SUBMITTED_AT,
    ] {
        assert_eq!(
            decode_job(&without(&job, tag)),
            Err(DecodeError::Missing { tag })
        );
    }
    let allocation = allocation_row(&allocation(shape()));
    for tag in [
        alloc_tag::JOB,
        alloc_tag::ALLOCATION_ID,
        alloc_tag::HOST,
        alloc_tag::STEP,
        alloc_tag::CREATED_AT,
    ] {
        assert_eq!(
            decode_allocation(&without(&allocation, tag)),
            Err(DecodeError::Missing { tag })
        );
    }
}

// Catches a value of the wrong width, text that is not UTF-8, or a value its field does not
// allow, read as something rather than refused by tag.
#[test]
fn a_value_that_is_not_valid_for_its_field_is_refused_by_tag() {
    use DecodeError::{BadLength, BadValue, NotUtf8};
    use job_tag as t;
    let job = job_row(&full_job());
    let not_utf8: &[u8] = &[0xff];
    let cases: Vec<(u16, Vec<u8>, DecodeError)> = vec![
        (
            t::WALL_LIMIT_SECS,
            vec![0; 7],
            BadLength {
                tag: t::WALL_LIMIT_SECS,
            },
        ),
        (t::PRIORITY, vec![0; 3], BadLength { tag: t::PRIORITY }),
        (t::STATE, vec![0, 0], BadLength { tag: t::STATE }),
        (t::PORTS, vec![0; 3], BadLength { tag: t::PORTS }),
        (t::ID, not_utf8.to_vec(), NotUtf8 { tag: t::ID }),
        (t::COMMAND, items(&[not_utf8]), NotUtf8 { tag: t::COMMAND }),
        (t::ENV, items(&[b"K", not_utf8]), NotUtf8 { tag: t::ENV }),
        (t::ENV, items(&[not_utf8, b"V"]), NotUtf8 { tag: t::ENV }),
        (
            t::FILES,
            items(&[not_utf8, b"x"]),
            NotUtf8 { tag: t::FILES },
        ),
        (
            t::CANCEL_REQUESTED,
            vec![2],
            BadValue {
                tag: t::CANCEL_REQUESTED,
            },
        ),
        (
            t::COMMAND,
            vec![0, 0, 0, 5, b'a'],
            BadValue { tag: t::COMMAND },
        ),
        (t::COMMAND, vec![0, 0], BadValue { tag: t::COMMAND }),
        (t::ENV, items(&[b"K"]), BadValue { tag: t::ENV }),
        (t::ID, b"a b".to_vec(), BadValue { tag: t::ID }),
        (t::HOST, b"Host A".to_vec(), BadValue { tag: t::HOST }),
        (t::IMAGE, b"app:latest".to_vec(), BadValue { tag: t::IMAGE }),
        (
            t::LABELS,
            items(&[b"jobscheduler.dev/unknown", b"x"]),
            BadValue { tag: t::LABELS },
        ),
        (
            t::LABELS,
            items(&[b"team", b"NOT LOWER"]),
            BadValue { tag: t::LABELS },
        ),
        (
            t::LABELS,
            items(&[b"jobscheduler.dev/run", b"NOT LOWER"]),
            BadValue { tag: t::LABELS },
        ),
    ];
    for (tag, value, why) in cases {
        assert_eq!(decode_job(&with(&job, tag, &value)), Err(why), "tag {tag}");
    }
    let no_class = without(&job, t::GPU_CLASS);
    assert_eq!(
        decode_job(&no_class),
        Err(BadValue { tag: t::GPUS }),
        "accelerators of no class"
    );
    let allocation = allocation_row(&allocation(shape()));
    assert_eq!(
        decode_allocation(&with(&allocation, alloc_tag::ALLOCATION_ID, &[0; 15])),
        Err(BadLength {
            tag: alloc_tag::ALLOCATION_ID
        })
    );
}

// Catches an error text that does not say what is wrong and where.
#[test]
fn a_decode_error_says_what_and_which_field() {
    for (why, said) in [
        (DecodeError::Empty, "the row is empty"),
        (DecodeError::BadFormat, "the row's format byte is 0"),
        (DecodeError::Newer, "the row was written by a later build"),
        (DecodeError::Truncated, "the row ends inside a field"),
        (
            DecodeError::DuplicateTag { tag: 7 },
            "field 7 appears twice",
        ),
        (DecodeError::Missing { tag: 7 }, "field 7 is missing"),
        (
            DecodeError::BadLength { tag: 7 },
            "field 7 has the wrong length",
        ),
        (DecodeError::NotUtf8 { tag: 7 }, "field 7 is not UTF-8"),
        (
            DecodeError::BadValue { tag: 7 },
            "field 7 holds a value that is not valid",
        ),
    ] {
        assert_eq!(why.to_string(), said);
    }
    assert_eq!(
        crate::JobStoreError::row("job/x", DecodeError::Truncated).to_string(),
        "the row job/x cannot be read: the row ends inside a field"
    );
    assert_eq!(
        crate::JobStoreError::row("job/x", DecodeError::Newer).to_string(),
        "the row job/x was written by a later build"
    );
}
