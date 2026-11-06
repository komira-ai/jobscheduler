//! The row encoding: round trips, every enum value, the layout byte for byte, the fields a
//! later build adds, and every way a row can fail to parse.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::StoreTime;
use js_types::{ExecMode, HostId, Labels};

use super::*;
use crate::result::{JobResult, JobTiming, ResourceUsage, ResultReason};
use crate::suite::{full_spec, whole_host_spec};

pub(super) fn at(secs: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(secs))
}

/// A result with every optional field set.
pub(super) fn full_result() -> JobResult {
    JobResult {
        exit_code: Some(-9),
        reason: ResultReason::WallLimit,
        stdout_tail: "out".to_owned(),
        stderr_tail: "err".to_owned(),
        truncated: true,
        timing: JobTiming {
            queued_at: at(1),
            placed_at: Some(at(2)),
            started_at: Some(at(3)),
            ended_at: at(4),
        },
        usage: Some(ResourceUsage {
            cpu_time: Duration::from_secs(7),
            peak_memory_bytes: 4096,
        }),
    }
}

/// A job with every optional field set.
pub(super) fn full_job() -> JobRecord {
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
        result: Some(full_result()),
        retention_ends: Some(at(6)),
        deadline_floor: Some(at(7)),
        client: Some(ClientName::parse("ci-1").expect("a client name")),
        carried: Carried::default(),
    }
}

/// A job with every optional field unset and every list empty.
pub(super) fn bare_job() -> JobRecord {
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
        result: None,
        retention_ends: None,
        deadline_floor: None,
        client: None,
        carried: Carried::default(),
    }
}

pub(super) fn allocation(booked: Booked) -> AllocationRecord {
    AllocationRecord {
        job: JobId::parse("full-1").expect("an id"),
        allocation_id: AllocationId::from_bits([7; 16]),
        host: HostId::new("host-a").expect("a host"),
        booked,
        host_ports: Vec::new(),
        step: AllocationStep::Freed,
        created_at: at(1),
        freed_at: Some(at(2)),
        end_reason: Some(EndReason::Exited),
        carried: Carried::default(),
    }
}

pub(super) fn shape() -> Booked {
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
    let bare_result = JobRecord {
        result: Some(JobResult {
            exit_code: None,
            reason: ResultReason::Cancelled,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            truncated: false,
            timing: JobTiming {
                queued_at: at(1),
                placed_at: None,
                started_at: None,
                ended_at: at(2),
            },
            usage: None,
        }),
        ..bare_job()
    };
    for job in [full_job(), bare_job(), bare_result] {
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

// Catches a code renumbered in the encoder and the decoder alike: each value is compared with
// its code as a literal here, both ways, so two values cannot share one and none can move.
#[test]
fn every_enum_value_has_its_own_fixed_code() {
    use StartFailure::*;
    let job = job_row(&full_job());
    let states = [
        (JobState::Queued, 0),
        (JobState::Placed, 1),
        (JobState::Starting, 2),
        (JobState::Running, 3),
        (JobState::Succeeded, 4),
        (JobState::Failed, 5),
        (JobState::Cancelled, 6),
        (JobState::Lost, 7),
        (JobState::FailedToStart(NoCapacity), 8),
        (JobState::FailedToStart(StartTimeout), 9),
        (JobState::FailedToStart(ExclusiveNotReady), 10),
        (JobState::FailedToStart(AgentRefused), 11),
    ];
    for (state, code) in states {
        let written = job_row(&JobRecord {
            state,
            ..full_job()
        });
        assert_eq!(field_of(&written, 22), Some(vec![code]), "{state:?}");
        assert_eq!(
            decode_job(&with(&job, 22, &[code])).map(|j| j.state),
            Ok(state)
        );
    }
    for (exec, code) in [(ExecMode::Container, 0), (ExecMode::SandboxExec, 1)] {
        let mut record = full_job();
        record.spec.constraints.exec = Some(exec);
        assert_eq!(
            field_of(&job_row(&record), 14),
            Some(vec![code]),
            "{exec:?}"
        );
        assert_eq!(decode_job(&with(&job, 14, &[code])), Ok(record));
    }
    for (network, code) in [
        (NetworkPolicy::None, 0),
        (NetworkPolicy::AllowList, 1),
        (NetworkPolicy::Open, 2),
    ] {
        let mut record = full_job();
        record.spec.network = network;
        assert_eq!(
            field_of(&job_row(&record), 17),
            Some(vec![code]),
            "{network:?}"
        );
        assert_eq!(decode_job(&with(&job, 17, &[code])), Ok(record));
    }
    let row = allocation_row(&allocation(shape()));
    for (step, code) in [
        (AllocationStep::Booked, 0),
        (AllocationStep::Releasing, 1),
        (AllocationStep::Freed, 2),
    ] {
        let record = AllocationRecord {
            step,
            ..allocation(shape())
        };
        assert_eq!(
            field_of(&allocation_row(&record), 9),
            Some(vec![code]),
            "{step:?}"
        );
        assert_eq!(decode_allocation(&with(&row, 9, &[code])), Ok(record));
    }
    for (reason, code) in [
        (EndReason::Exited, 0),
        (EndReason::WallLimit, 1),
        (EndReason::Memory, 2),
        (EndReason::Cancelled, 3),
        (EndReason::HostAbsent, 4),
        (EndReason::FailedPrepare, 5),
        (EndReason::StartTimeout, 6),
        (EndReason::Refused, 7),
    ] {
        let record = AllocationRecord {
            end_reason: Some(reason),
            ..allocation(shape())
        };
        assert_eq!(
            field_of(&allocation_row(&record), 12),
            Some(vec![code]),
            "{reason:?}"
        );
        assert_eq!(decode_allocation(&with(&row, 12, &[code])), Ok(record));
    }
    for (reason, code) in [
        (ResultReason::Exit, 0),
        (ResultReason::WallLimit, 1),
        (ResultReason::Memory, 2),
        (ResultReason::Cancelled, 3),
    ] {
        let mut record = full_job();
        if let Some(result) = record.result.as_mut() {
            result.reason = reason;
        }
        assert_eq!(
            field_of(&job_row(&record), 31),
            Some(vec![code]),
            "{reason:?}"
        );
        assert_eq!(decode_job(&with(&job, 31, &[code])), Ok(record));
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
        (job_tag::RESULT_REASON, 4),
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
    let later_label = items(&[b"jobscheduler.dev/later", b"x"]);
    assert_eq!(
        decode_job(&with(&job, job_tag::LABELS, &later_label)),
        Err(DecodeError::Newer),
        "a reserved label a later build sets"
    );
}

/// A list value framed here, apart from the codec's own `list`.
fn framed(items: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(&u32::try_from(item.len()).expect("small").to_be_bytes());
        out.extend_from_slice(item);
    }
    out
}

/// A time of whole seconds as a row holds it.
fn seconds(secs: u64) -> Vec<u8> {
    let mut out = secs.to_be_bytes().to_vec();
    out.extend_from_slice(&0u32.to_be_bytes());
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

// Catches a change to the bytes of a row: the format byte, a tag's number, the tag order,
// the width of a tag, a length or a time, or the framing of a list or map. The rows are
// written out here with literal tags and codes, independent of the codec's constants, so a
// renumbering done consistently in the encoder and decoder still fails.
#[test]
fn the_layout_is_pinned_byte_for_byte() {
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
    ]);
    assert_eq!(job_row(&full_job()), job);
    assert_eq!(decode_job(&job), Ok(full_job()));

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

    let bare = job_row(&bare_job());
    assert_eq!(
        field_of(&bare, 18),
        Some(Vec::new()),
        "an empty list is written empty"
    );
    assert_eq!(field_of(&bare, 24), None, "None is not written");
    assert_eq!(field_of(&bare, 6), Some(Vec::new()), "a marker is empty");
    for tag in 31..=40 {
        assert_eq!(
            field_of(&bare, tag),
            None,
            "a job with no result has no field {tag}"
        );
    }
}

// Catches a time cut to fit: the largest reading of the store's clock is written as it is,
// and nanoseconds that make a whole second are refused.
#[test]
fn every_store_time_is_written_as_it_is() {
    let latest = StoreTime::from_duration(Duration::MAX);
    let job = JobRecord {
        submitted_at: latest,
        ended_at: Some(latest),
        ..full_job()
    };
    assert_eq!(decode_job(&job_row(&job)), Ok(job));
    let mut value = u64::MAX.to_be_bytes().to_vec();
    value.extend_from_slice(&999_999_999u32.to_be_bytes());
    assert_eq!(
        field_of(
            &job_row(&JobRecord {
                submitted_at: latest,
                ..full_job()
            }),
            23
        ),
        Some(value)
    );
    let mut whole_second = 0u64.to_be_bytes().to_vec();
    whole_second.extend_from_slice(&1_000_000_000u32.to_be_bytes());
    assert_eq!(
        decode_job(&with(&job_row(&full_job()), 23, &whole_second)),
        Err(DecodeError::BadValue { tag: 23 })
    );
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
        job_tag::RESULT_EXIT_CODE,
        job_tag::RESULT_STDOUT,
        job_tag::RESULT_STDERR,
        job_tag::RESULT_TRUNCATED,
        job_tag::RESULT_PLACED_AT,
        job_tag::RESULT_STARTED_AT,
        job_tag::RESULT_USAGE,
        job_tag::DEADLINE_FLOOR,
        job_tag::CLIENT,
    ] {
        row = without(&row, tag);
    }
    let job = decode_job(&row).expect("reads");
    assert_eq!(job.deadline_floor, None, "a row before floors has none");
    assert_eq!(job.client, None, "a row before clients were named has none");
    assert_eq!(
        job.result,
        Some(JobResult {
            exit_code: None,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            truncated: false,
            timing: JobTiming {
                placed_at: None,
                started_at: None,
                ..full_result().timing
            },
            usage: None,
            ..full_result()
        })
    );
    // A row without a reason, as every row before results were stored, has no result.
    let mut before_results = job_row(&full_job());
    for tag in 31..=40 {
        before_results = without(&before_results, tag);
    }
    assert_eq!(
        decode_job(&before_results).map(|job| (job.result, job.carried)),
        Ok((None, Carried::default()))
    );
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
        job_tag::RESULT_QUEUED_AT,
        job_tag::RESULT_ENDED_AT,
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
        (t::CLIENT, b"a b".to_vec(), BadValue { tag: t::CLIENT }),
        (t::CLIENT, not_utf8.to_vec(), NotUtf8 { tag: t::CLIENT }),
        (t::IMAGE, b"app:latest".to_vec(), BadValue { tag: t::IMAGE }),
        (t::WHOLE_HOST, vec![1], BadValue { tag: t::WHOLE_HOST }),
        (
            t::ENV,
            items(&[b"K", b"V", b"K", b"W"]),
            BadValue { tag: t::ENV },
        ),
        (
            t::FILES,
            items(&[b"f", b"1", b"f", b"2"]),
            BadValue { tag: t::FILES },
        ),
        (
            t::SUBMITTED_AT,
            vec![0; 8],
            BadLength {
                tag: t::SUBMITTED_AT,
            },
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
        (
            t::RESULT_REASON,
            vec![0, 0],
            BadLength {
                tag: t::RESULT_REASON,
            },
        ),
        (
            t::RESULT_EXIT_CODE,
            vec![0; 5],
            BadLength {
                tag: t::RESULT_EXIT_CODE,
            },
        ),
        (
            t::RESULT_STDOUT,
            not_utf8.to_vec(),
            NotUtf8 {
                tag: t::RESULT_STDOUT,
            },
        ),
        (
            t::RESULT_STDERR,
            not_utf8.to_vec(),
            NotUtf8 {
                tag: t::RESULT_STDERR,
            },
        ),
        (
            t::RESULT_TRUNCATED,
            vec![2],
            BadValue {
                tag: t::RESULT_TRUNCATED,
            },
        ),
        (
            t::RESULT_QUEUED_AT,
            vec![0; 11],
            BadLength {
                tag: t::RESULT_QUEUED_AT,
            },
        ),
        (
            t::RESULT_PLACED_AT,
            vec![0; 13],
            BadLength {
                tag: t::RESULT_PLACED_AT,
            },
        ),
        (
            t::RESULT_STARTED_AT,
            vec![0; 4],
            BadLength {
                tag: t::RESULT_STARTED_AT,
            },
        ),
        (
            t::RESULT_ENDED_AT,
            vec![0; 12 + 1],
            BadLength {
                tag: t::RESULT_ENDED_AT,
            },
        ),
        (
            t::RESULT_USAGE,
            vec![0; 11],
            BadLength {
                tag: t::RESULT_USAGE,
            },
        ),
        (
            t::RESULT_USAGE,
            vec![0; 19],
            BadLength {
                tag: t::RESULT_USAGE,
            },
        ),
        (
            t::RESULT_USAGE,
            [seconds(0), 4096u64.to_be_bytes().to_vec(), vec![0]].concat(),
            BadLength {
                tag: t::RESULT_USAGE,
            },
        ),
        (
            t::RESULT_USAGE,
            [
                0u64.to_be_bytes().to_vec(),
                1_000_000_000u32.to_be_bytes().to_vec(),
                0u64.to_be_bytes().to_vec(),
            ]
            .concat(),
            BadValue {
                tag: t::RESULT_USAGE,
            },
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
    assert_eq!(
        decode_allocation(&with(&allocation, alloc_tag::WHOLE_HOST, &[1])),
        Err(BadValue {
            tag: alloc_tag::WHOLE_HOST
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
            DecodeError::WrongKey,
            "the row names another job than its key",
        ),
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

// Catches host ports dropped or reordered by a rewrite, or written for an allocation that holds
// none, which would change every row written before ports were booked.
#[test]
fn a_ported_allocation_row_round_trips_with_a_later_field_beside_it() {
    let ported = AllocationRecord {
        host_ports: vec![1, 80, 8080, u16::MAX],
        ..allocation(shape())
    };
    let row = allocation_row(&ported);
    assert_eq!(decode_allocation(&row), Ok(ported.clone()));
    assert_eq!(
        field_of(&allocation_row(&allocation(shape())), 13),
        None,
        "an allocation that holds none writes no field 13"
    );
    let read = decode_allocation(&with(&row, 902, b"later")).expect("reads");
    assert_eq!(read.host_ports, ported.host_ports);
    let rewritten = allocation_row(&read);
    assert_eq!(field_of(&rewritten, 902), Some(b"later".to_vec()));
    assert_eq!(field_of(&rewritten, 13), field_of(&row, 13));
}

// Catches a row from before host ports that is refused, or read as holding some, when it
// should read as holding none.
#[test]
fn an_allocation_row_from_before_host_ports_reads_as_holding_none() {
    let row = allocation_row(&AllocationRecord {
        host_ports: vec![80],
        ..allocation(shape())
    });
    assert_eq!(
        decode_allocation(&without(&row, 13)),
        Ok(allocation(shape()))
    );
}

// Catches host ports that are read from a value that is not a whole number of ports, or that
// is not strictly ascending with no zero, which no writer produces.
#[test]
fn a_host_port_list_that_is_not_valid_is_refused_by_tag() {
    let row = allocation_row(&allocation(shape()));
    let cases: [(&[u8], DecodeError); 5] = [
        (&[0, 80, 0], DecodeError::BadLength { tag: 13 }),
        (&[0, 0], DecodeError::BadValue { tag: 13 }),
        (&[0, 80, 0, 0], DecodeError::BadValue { tag: 13 }),
        (&[0x1f, 0x90, 0, 80], DecodeError::BadValue { tag: 13 }),
        (&[0, 80, 0, 80], DecodeError::BadValue { tag: 13 }),
    ];
    for (value, error) in cases {
        assert_eq!(
            decode_allocation(&with(&row, 13, value)),
            Err(error),
            "{value:?}"
        );
    }
}
