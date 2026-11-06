//! The row encoding under generated input: every record reads back as written, a row cut,
//! flipped or spliced either fails with an error or reads as a record that writes and reads
//! back unchanged, arbitrary bytes never panic, and a table of malformed rows each name the
//! error they raise.
//!
//! These are unit tests of the codec rather than a file in `tests/` because they reach the
//! codec's private framing (`Fields`, `Writer`) to build rows no encoder writes.

use std::collections::BTreeMap;
use std::time::Duration;

use proptest::collection::{btree_map, vec};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;

use super::tests::{allocation, at, bare_job, full_job, full_result, shape};
use super::*;
use crate::result::{JobResult, JobTiming, ResourceUsage, ResultReason};
use crate::rows;
use crate::suite::{digest, full_spec};

/// The same generated cases on every run.
fn config(seed: u64) -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(seed),
        ..ProptestConfig::default()
    }
}

fn store_time() -> impl Strategy<Value = StoreTime> {
    (any::<u64>(), 0..NANOS_PER_SEC)
        .prop_map(|(secs, nanos)| StoreTime::from_duration(Duration::new(secs, nanos)))
}

fn ident() -> impl Strategy<Value = String> {
    "[a-z0-9._-]{1,20}"
}

fn any_text() -> impl Strategy<Value = String> {
    "\\PC{0,12}"
}

fn text_pairs() -> impl Strategy<Value = BTreeMap<String, String>> {
    btree_map(any_text(), any_text(), 0..4)
}

fn needs() -> impl Strategy<Value = Needs> {
    prop_oneof![
        Just(Needs::WholeHost),
        (
            any::<u64>(),
            any::<u64>(),
            0..4u64,
            any::<u64>(),
            any_text()
        )
            .prop_map(|(cpu, memory, gpus, disk, class)| {
                let class = (gpus > 0).then_some(class);
                let resources = Resources::new(cpu, memory, gpus, class).expect("a matching class");
                Needs::Shape(resources.with_disk_bytes(disk))
            }),
    ]
}

fn constraints() -> impl Strategy<Value = Constraints> {
    (
        proptest::option::of(any_text()),
        proptest::option::of(any_text()),
        proptest::option::of(prop_oneof![
            Just(ExecMode::Container),
            Just(ExecMode::SandboxExec)
        ]),
        text_pairs(),
    )
        .prop_map(|(os, arch, exec, capabilities)| Constraints {
            os,
            arch,
            exec,
            capabilities,
        })
}

fn labels() -> impl Strategy<Value = Labels> {
    (
        btree_map("[a-z]{1,8}", "[a-z0-9]{1,8}", 0..4),
        proptest::option::of("[a-z0-9]{1,8}"),
        proptest::option::of("[a-z0-9]{1,8}"),
    )
        .prop_map(|(caller, run, tenant)| {
            let mut labels = Labels::from_caller(&caller).expect("caller labels");
            for (key, value) in [(ReservedKey::Run, run), (ReservedKey::Tenant, tenant)] {
                if let Some(value) = value {
                    labels = labels.with_reserved(key, &value).expect("a reserved label");
                }
            }
            labels
        })
}

fn spec() -> impl Strategy<Value = JobSpec> {
    (
        (
            prop::sample::select(vec!['a', 'b', '0']),
            vec(any_text(), 0..3),
            vec(any_text(), 0..3),
            text_pairs(),
            needs(),
            constraints(),
        ),
        (
            any::<u64>(),
            prop_oneof![
                Just(NetworkPolicy::None),
                Just(NetworkPolicy::AllowList),
                Just(NetworkPolicy::Open)
            ],
            vec((any::<u16>(), any::<u16>()), 0..4),
            btree_map(any_text(), vec(any::<u8>(), 0..8), 0..3),
            labels(),
        ),
    )
        .prop_map(
            |(
                (digit, command, args, env, needs, constraints),
                (wall, network, ports, files, labels),
            )| {
                let image = format!("team/app:1@{}", digest(digit));
                JobSpec {
                    image: ImageDigestRef::parse(&image).expect("an image"),
                    command,
                    args,
                    env,
                    needs,
                    constraints,
                    wall_limit_secs: wall,
                    network,
                    ports: ports
                        .into_iter()
                        .map(|(host, container)| DeclaredPort { host, container })
                        .collect(),
                    files,
                    labels,
                }
            },
        )
}

fn state() -> impl Strategy<Value = JobState> {
    prop::sample::select(vec![
        JobState::Queued,
        JobState::Placed,
        JobState::Starting,
        JobState::Running,
        JobState::Succeeded,
        JobState::Failed,
        JobState::Cancelled,
        JobState::Lost,
        JobState::FailedToStart(StartFailure::NoCapacity),
        JobState::FailedToStart(StartFailure::StartTimeout),
        JobState::FailedToStart(StartFailure::ExclusiveNotReady),
        JobState::FailedToStart(StartFailure::AgentRefused),
    ])
}

fn job_result() -> impl Strategy<Value = JobResult> {
    (
        (
            proptest::option::of(any::<i32>()),
            prop::sample::select(vec![
                ResultReason::Exit,
                ResultReason::WallLimit,
                ResultReason::Memory,
                ResultReason::Cancelled,
            ]),
            any_text(),
            any_text(),
            any::<bool>(),
        ),
        (
            store_time(),
            proptest::option::of(store_time()),
            proptest::option::of(store_time()),
            store_time(),
            proptest::option::of((0..u64::MAX, 0..NANOS_PER_SEC, any::<u64>())),
        ),
    )
        .prop_map(
            |((exit_code, reason, stdout_tail, stderr_tail, truncated), (q, p, s, e, usage))| {
                JobResult {
                    exit_code,
                    reason,
                    stdout_tail,
                    stderr_tail,
                    truncated,
                    timing: JobTiming {
                        queued_at: q,
                        placed_at: p,
                        started_at: s,
                        ended_at: e,
                    },
                    usage: usage.map(|(secs, nanos, peak)| ResourceUsage {
                        cpu_time: Duration::new(secs, nanos),
                        peak_memory_bytes: peak,
                    }),
                }
            },
        )
}

fn job() -> impl Strategy<Value = JobRecord> {
    (
        (ident(), spec(), any::<i32>(), state(), store_time()),
        (
            proptest::option::of(store_time()),
            proptest::option::of(store_time()),
            proptest::option::of(store_time()),
            proptest::option::of(ident()),
            any::<bool>(),
            proptest::option::of(store_time()),
        ),
        (
            proptest::option::of(job_result()),
            proptest::option::of(store_time()),
            proptest::option::of(store_time()),
            proptest::option::of(ident()),
        ),
    )
        .prop_map(
            |(
                (id, spec, priority, state, submitted_at),
                (placed_at, started_at, ended_at, host, cancel_requested, cancel_requested_at),
                (result, retention_ends, deadline_floor, client),
            )| JobRecord {
                id: JobId::parse(&id).expect("an id"),
                spec,
                priority,
                state,
                submitted_at,
                placed_at,
                started_at,
                ended_at,
                host: host.map(|host| HostId::new(&host).expect("a host")),
                cancel_requested,
                cancel_requested_at,
                result,
                retention_ends,
                deadline_floor,
                client: client.map(|name| ClientName::parse(&name).expect("a client name")),
                carried: Carried::default(),
            },
        )
}

fn booked() -> impl Strategy<Value = Booked> {
    prop_oneof![
        Just(Booked::WholeHost),
        (any::<u64>(), any::<u64>(), any::<u64>(), any::<u64>()).prop_map(
            |(cpu_millis, memory_bytes, disk_bytes, accelerators)| Booked::Shape {
                cpu_millis,
                memory_bytes,
                disk_bytes,
                accelerators,
            }
        ),
    ]
}

fn allocation_record() -> impl Strategy<Value = AllocationRecord> {
    (
        (ident(), any::<[u8; 16]>(), ident(), booked()),
        // Ascending and each once, as the codec requires: a set, read in order.
        proptest::collection::btree_set(1..=u16::MAX, 0..6),
        (
            prop::sample::select(vec![
                AllocationStep::Booked,
                AllocationStep::Releasing,
                AllocationStep::Freed,
            ]),
            store_time(),
            proptest::option::of(store_time()),
            proptest::option::of(prop::sample::select(vec![
                EndReason::Exited,
                EndReason::WallLimit,
                EndReason::Memory,
                EndReason::Cancelled,
                EndReason::HostAbsent,
                EndReason::FailedPrepare,
                EndReason::StartTimeout,
                EndReason::Refused,
            ])),
        ),
    )
        .prop_map(
            |((job, bits, host, booked), ports, (step, created_at, freed_at, end_reason))| {
                AllocationRecord {
                    job: JobId::parse(&job).expect("an id"),
                    allocation_id: AllocationId::from_bits(bits),
                    host: HostId::new(&host).expect("a host"),
                    booked,
                    host_ports: ports.into_iter().collect(),
                    step,
                    created_at,
                    freed_at,
                    end_reason,
                    carried: Carried::default(),
                }
            },
        )
}

/// One change to a row's bytes.
#[derive(Debug, Clone)]
enum Edit {
    /// Keep only the first `at` (taken modulo the length) bytes.
    Truncate(usize),
    /// Xor the byte at `at` with `mask`.
    Flip(usize, u8),
    /// Put `bytes` in at `at`.
    Insert(usize, Vec<u8>),
    /// Remove `len` bytes from `at`.
    Remove(usize, usize),
}

fn edits() -> impl Strategy<Value = Vec<Edit>> {
    vec(
        prop_oneof![
            any::<usize>().prop_map(Edit::Truncate),
            (any::<usize>(), 1..=u8::MAX).prop_map(|(at, mask)| Edit::Flip(at, mask)),
            (any::<usize>(), vec(any::<u8>(), 0..8))
                .prop_map(|(at, bytes)| Edit::Insert(at, bytes)),
            (any::<usize>(), 0..8usize).prop_map(|(at, len)| Edit::Remove(at, len)),
        ],
        1..4,
    )
}

/// `row` with `edits` applied in turn. An empty row stays empty under every edit.
fn edited(row: &[u8], edits: &[Edit]) -> Vec<u8> {
    let mut row = row.to_vec();
    for edit in edits {
        let at = |n: usize| n % (row.len() + 1);
        match edit {
            Edit::Truncate(n) => row.truncate(at(*n)),
            Edit::Flip(n, mask) => {
                let i = at(*n);
                for byte in row.iter_mut().skip(i).take(1) {
                    *byte ^= mask;
                }
            }
            Edit::Insert(n, bytes) => {
                let i = at(*n);
                row.splice(i..i, bytes.iter().copied());
            }
            Edit::Remove(n, len) => {
                let i = at(*n);
                let end = (i + len).min(row.len());
                row.drain(i..end);
            }
        }
    }
    row
}

/// A row whose framing is sound: the format byte and `fields` each with a valid tag, length
/// and value, in the order given (a tag may repeat).
fn framed_row(fields: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut row = vec![FORMAT];
    for (tag, value) in fields {
        row.extend_from_slice(&tag.to_be_bytes());
        row.extend_from_slice(&(value.len() as u32).to_be_bytes());
        row.extend_from_slice(value);
    }
    row
}

fn field_soup() -> impl Strategy<Value = Vec<u8>> {
    vec((1..=45u16, vec(any::<u8>(), 0..14)), 0..10).prop_map(|fields| framed_row(&fields))
}

proptest! {
    #![proptest_config(config(0x6a6f_6273_0001))]

    #[test]
    fn a_generated_job_reads_back_as_written(job in job()) {
        let row = encode_job(&job).expect("encodes");
        assert_eq!(decode_job(&row), Ok(job));
    }

    #[test]
    fn a_generated_allocation_reads_back_as_written(allocation in allocation_record()) {
        let row = encode_allocation(&allocation).expect("encodes");
        assert_eq!(decode_allocation(&row), Ok(allocation));
    }

    #[test]
    fn an_edited_job_row_fails_or_reads_back_unchanged_once_written_again(
        job in job(),
        edits in edits(),
    ) {
        let row = edited(&encode_job(&job).expect("encodes"), &edits);
        if let Ok(read) = decode_job(&row) {
            let again = encode_job(&read).expect("encodes");
            assert_eq!(decode_job(&again), Ok(read));
        }
    }

    #[test]
    fn an_edited_allocation_row_fails_or_reads_back_unchanged_once_written_again(
        allocation in allocation_record(),
        edits in edits(),
    ) {
        let row = edited(&encode_allocation(&allocation).expect("encodes"), &edits);
        if let Ok(read) = decode_allocation(&row) {
            let again = encode_allocation(&read).expect("encodes");
            assert_eq!(decode_allocation(&again), Ok(read));
        }
    }

    #[test]
    fn arbitrary_bytes_are_a_record_or_an_error_for_a_job(bytes in vec(any::<u8>(), 0..64)) {
        if let Ok(read) = decode_job(&bytes) {
            assert_eq!(decode_job(&encode_job(&read).expect("encodes")), Ok(read));
        }
    }

    #[test]
    fn arbitrary_bytes_are_a_record_or_an_error_for_an_allocation(bytes in vec(any::<u8>(), 0..64)) {
        if let Ok(read) = decode_allocation(&bytes) {
            assert_eq!(decode_allocation(&encode_allocation(&read).expect("encodes")), Ok(read));
        }
    }

    #[test]
    fn well_framed_fields_with_arbitrary_values_are_a_record_or_an_error_for_a_job(
        row in field_soup(),
    ) {
        if let Ok(read) = decode_job(&row) {
            assert_eq!(decode_job(&encode_job(&read).expect("encodes")), Ok(read));
        }
    }

    #[test]
    fn well_framed_fields_with_arbitrary_values_are_a_record_or_an_error_for_an_allocation(
        row in field_soup(),
    ) {
        if let Ok(read) = decode_allocation(&row) {
            assert_eq!(decode_allocation(&encode_allocation(&read).expect("encodes")), Ok(read));
        }
    }
}

/// `row` with its fields changed by `change`.
fn refielded(row: &[u8], change: impl FnOnce(&mut BTreeMap<u16, Vec<u8>>)) -> Vec<u8> {
    let mut fields = Fields::of(row).expect("the row parses");
    change(&mut fields.0);
    Writer(fields.0)
        .finish(&Carried::default())
        .expect("encodes")
}

fn job_row() -> Vec<u8> {
    encode_job(&full_job()).expect("encodes")
}

fn allocation_row() -> Vec<u8> {
    encode_allocation(&allocation(shape())).expect("encodes")
}

#[test]
fn a_malformed_job_row_raises_the_error_that_names_what_is_wrong() {
    use job_tag as t;
    let mut trailing_byte = job_row();
    trailing_byte.push(0);
    let mut partial_field = job_row();
    partial_field.extend_from_slice(&[0, 99, 0, 0]);
    let mut short_value = job_row();
    short_value.extend_from_slice(&[0, 99, 0, 0, 0, 5, 1]);
    let rows: Vec<(&str, Vec<u8>, DecodeError)> = vec![
        ("empty", vec![], DecodeError::Empty),
        ("format byte 0", vec![0], DecodeError::BadFormat),
        ("a newer format", vec![FORMAT + 1], DecodeError::Newer),
        ("the highest format", vec![u8::MAX], DecodeError::Newer),
        ("half a tag", vec![FORMAT, 0], DecodeError::Truncated),
        (
            "a tag and no length",
            vec![FORMAT, 0, 1],
            DecodeError::Truncated,
        ),
        (
            "half a length",
            vec![FORMAT, 0, 1, 0, 0],
            DecodeError::Truncated,
        ),
        (
            "a value cut short",
            vec![FORMAT, 0, 1, 0, 0, 0, 5, b'a'],
            DecodeError::Truncated,
        ),
        (
            "a length of u32::MAX",
            vec![FORMAT, 0, 1, 0xff, 0xff, 0xff, 0xff, b'a'],
            DecodeError::Truncated,
        ),
        (
            "a length of u32::MAX and no value",
            vec![FORMAT, 0, 1, 0xff, 0xff, 0xff, 0xff],
            DecodeError::Truncated,
        ),
        (
            "one byte after the last field",
            trailing_byte,
            DecodeError::Truncated,
        ),
        (
            "a tag and no length after the last field",
            partial_field,
            DecodeError::Truncated,
        ),
        (
            "a value cut short after the last field",
            short_value,
            DecodeError::Truncated,
        ),
        (
            "a tag twice",
            framed_row(&[(t::ID, b"a".to_vec()), (t::ID, b"a".to_vec())]),
            DecodeError::DuplicateTag { tag: t::ID },
        ),
        (
            "no fields",
            vec![FORMAT],
            DecodeError::Missing { tag: t::ID },
        ),
        (
            "no image",
            refielded(&job_row(), |f| drop(f.remove(&t::IMAGE))),
            DecodeError::Missing { tag: t::IMAGE },
        ),
        (
            "no state",
            refielded(&job_row(), |f| drop(f.remove(&t::STATE))),
            DecodeError::Missing { tag: t::STATE },
        ),
        (
            "an unknown state",
            refielded(&job_row(), |f| drop(f.insert(t::STATE, vec![200]))),
            DecodeError::Newer,
        ),
        (
            "an unknown network policy",
            refielded(&job_row(), |f| drop(f.insert(t::NETWORK, vec![9]))),
            DecodeError::Newer,
        ),
        (
            "a priority of 3 bytes",
            refielded(&job_row(), |f| drop(f.insert(t::PRIORITY, vec![0; 3]))),
            DecodeError::BadLength { tag: t::PRIORITY },
        ),
        (
            "a wall limit of 4 bytes",
            refielded(&job_row(), |f| {
                drop(f.insert(t::WALL_LIMIT_SECS, vec![0; 4]))
            }),
            DecodeError::BadLength {
                tag: t::WALL_LIMIT_SECS,
            },
        ),
        (
            "a state of 2 bytes",
            refielded(&job_row(), |f| drop(f.insert(t::STATE, vec![0, 0]))),
            DecodeError::BadLength { tag: t::STATE },
        ),
        (
            "an operating system that is not UTF-8",
            refielded(&job_row(), |f| drop(f.insert(t::OS, vec![0xff, 0xfe]))),
            DecodeError::NotUtf8 { tag: t::OS },
        ),
        (
            "a stdout tail that is not UTF-8",
            refielded(&job_row(), |f| {
                drop(f.insert(t::RESULT_STDOUT, vec![0xc3, 0x28]))
            }),
            DecodeError::NotUtf8 {
                tag: t::RESULT_STDOUT,
            },
        ),
        (
            "an id with a space",
            refielded(&job_row(), |f| drop(f.insert(t::ID, b"a b".to_vec()))),
            DecodeError::BadValue { tag: t::ID },
        ),
    ];
    for (why, row, error) in rows {
        assert_eq!(decode_job(&row), Err(error), "{why}");
    }
}

#[test]
fn a_malformed_allocation_row_raises_the_error_that_names_what_is_wrong() {
    use alloc_tag as t;
    let mut trailing_byte = allocation_row();
    trailing_byte.push(0);
    let rows: Vec<(&str, Vec<u8>, DecodeError)> = vec![
        ("empty", vec![], DecodeError::Empty),
        ("format byte 0", vec![0], DecodeError::BadFormat),
        ("a newer format", vec![FORMAT + 1, 0, 1], DecodeError::Newer),
        (
            "a length of u32::MAX",
            vec![FORMAT, 0, 1, 0xff, 0xff, 0xff, 0xff],
            DecodeError::Truncated,
        ),
        (
            "one byte after the last field",
            trailing_byte,
            DecodeError::Truncated,
        ),
        (
            "a tag twice",
            framed_row(&[(t::HOST, b"a".to_vec()), (t::HOST, b"a".to_vec())]),
            DecodeError::DuplicateTag { tag: t::HOST },
        ),
        (
            "no host",
            refielded(&allocation_row(), |f| drop(f.remove(&t::HOST))),
            DecodeError::Missing { tag: t::HOST },
        ),
        (
            "no step",
            refielded(&allocation_row(), |f| drop(f.remove(&t::STEP))),
            DecodeError::Missing { tag: t::STEP },
        ),
        (
            "an unknown step",
            refielded(&allocation_row(), |f| drop(f.insert(t::STEP, vec![77]))),
            DecodeError::Newer,
        ),
        (
            "an unknown end reason",
            refielded(&allocation_row(), |f| {
                drop(f.insert(t::END_REASON, vec![77]))
            }),
            DecodeError::Newer,
        ),
        (
            "an allocation id of 15 bytes",
            refielded(&allocation_row(), |f| {
                drop(f.insert(t::ALLOCATION_ID, vec![0; 15]))
            }),
            DecodeError::BadLength {
                tag: t::ALLOCATION_ID,
            },
        ),
        (
            "a host that is not UTF-8",
            refielded(&allocation_row(), |f| drop(f.insert(t::HOST, vec![0xff]))),
            DecodeError::NotUtf8 { tag: t::HOST },
        ),
        (
            "host ports that repeat",
            refielded(&allocation_row(), |f| {
                drop(f.insert(t::HOST_PORTS, vec![0, 5, 0, 5]))
            }),
            DecodeError::BadValue { tag: t::HOST_PORTS },
        ),
    ];
    for (why, row, error) in rows {
        assert_eq!(decode_allocation(&row), Err(error), "{why}");
    }
}

#[test]
fn the_rows_module_reads_and_writes_what_the_codec_does() {
    let job = full_job();
    let row = rows::encode_job_row(&job).expect("encodes");
    assert_eq!(row, encode_job(&job).expect("encodes"));
    assert_eq!(rows::decode_job_row(&row), Ok(job));
    assert_eq!(rows::decode_job_row(&[]), Err(DecodeError::Empty));

    let allocation = allocation(shape());
    let row = rows::encode_allocation_row(&allocation).expect("encodes");
    assert_eq!(row, encode_allocation(&allocation).expect("encodes"));
    assert_eq!(rows::decode_allocation_row(&row), Ok(allocation));
    assert_eq!(
        rows::decode_allocation_row(&[0]),
        Err(DecodeError::BadFormat)
    );
}

#[test]
fn the_fixtures_the_generators_share_read_back() {
    // The hand-written fixtures hold the shapes the generators vary; they stay readable here.
    let bare = bare_job();
    assert_eq!(decode_job(&encode_job(&bare).expect("encodes")), Ok(bare));
    let mut full = full_job();
    full.spec = full_spec();
    full.result = Some(full_result());
    full.retention_ends = Some(at(9));
    assert_eq!(decode_job(&encode_job(&full).expect("encodes")), Ok(full));
}
