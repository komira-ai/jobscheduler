//! Property tests over the frame decode: a valid frame survives an encode and a decode,
//! and bytes that are not a valid frame end in a typed error and never in a panic.
//!
//! There is no byte framing in this crate: a frame is a JSON body, decoded with
//! `serde_json::from_slice` and then checked with `validate`. Every test here is
//! deterministic: the generators run from a fixed seed and no case reads a clock.

mod common;

use std::collections::BTreeMap;
use std::fmt::Debug;

use common::{heartbeat, hello, job, welcome};
use js_agent_wire::limits::*;
use js_agent_wire::{
    Ack, Caps, Command, Heartbeat, HeartbeatReply, HeldRun, HeldState, Hello, HelloReply,
    Isolation, JobSpec, JobStartSpec, Outcome, Phase, ReadModel, Refused, ResultAck, Rule,
    RunResult, Welcome,
};
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn config(seed: u64) -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(seed),
        ..ProptestConfig::default()
    }
}

// ---- generators of valid values -------------------------------------------------------

/// An id in the grammar every id of the wire shares.
fn ident() -> impl Strategy<Value = String> {
    "[A-Za-z0-9][A-Za-z0-9._-]{0,40}"
}

/// A host id: lower case, in the grammar of `js_types::HostId`.
fn host_ident() -> impl Strategy<Value = String> {
    "[a-z0-9][a-z0-9._-]{0,40}"
}

/// Distinct ids, as a list that must hold none twice.
fn idents(most: usize) -> impl Strategy<Value = Vec<String>> {
    proptest::collection::btree_set(ident(), 0..=most).prop_map(|set| set.into_iter().collect())
}

/// Plain printable text, the kind `check::text` takes.
fn text() -> impl Strategy<Value = String> {
    "[ -~]{1,60}"
}

fn words() -> impl Strategy<Value = BTreeMap<String, String>> {
    proptest::collection::btree_map("[a-z][a-z0-9-]{0,12}", "[a-z0-9]{1,12}", 0..6)
}

fn phase() -> impl Strategy<Value = Phase> {
    prop_oneof![
        Just(Phase::Serving),
        Just(Phase::Draining),
        Just(Phase::Quarantined)
    ]
}

fn held_state() -> impl Strategy<Value = HeldState> {
    prop_oneof![
        Just(HeldState::Starting),
        Just(HeldState::Running),
        Just(HeldState::ExitedUnacked)
    ]
}

fn ack() -> impl Strategy<Value = Ack> {
    prop_oneof![
        Just(Ack::Accepted),
        Just(Ack::Duplicate),
        Just(Ack::Stale),
        Just(Ack::UnknownLease),
        Just(Ack::Mismatch),
        Just(Ack::Unknown)
    ]
}

fn held() -> impl Strategy<Value = Vec<HeldRun>> {
    proptest::collection::btree_map(ident(), held_state(), 0..8).prop_map(|map| {
        map.into_iter()
            .map(|(run_id, state)| HeldRun { run_id, state })
            .collect()
    })
}

fn run_result() -> impl Strategy<Value = RunResult> {
    (ident(), ident(), proptest::option::of(text()), any::<i32>()).prop_map(
        |(run_id, lease_id, detail, code)| RunResult {
            run_id,
            lease_id,
            outcome: Outcome::TimedOut,
            exit_code: Some(code),
            detail,
            log_tail: None,
        },
    )
}

fn hello_frame() -> impl Strategy<Value = Hello> {
    (
        host_ident(),
        ident(),
        text(),
        (1..=MAX_CPU_MILLIS, 1..=MAX_MEMORY_BYTES),
        (words(), words(), phase(), held(), any::<bool>()),
    )
        .prop_map(
            |(
                host_id,
                pool,
                agent_version,
                (cpu, memory),
                (capabilities, labels, phase, held, creds),
            )| {
                let mut frame = hello();
                frame.host_id = host_id;
                frame.pool = pool;
                frame.agent_version = agent_version;
                frame.shape.cpu_millis = cpu;
                frame.shape.memory_bytes = memory;
                frame.capabilities = capabilities;
                frame.labels = labels;
                frame.phase = phase;
                frame.held = held;
                frame.accepts_host_credential = creds;
                frame
            },
        )
}

fn heartbeat_frame() -> impl Strategy<Value = Heartbeat> {
    (
        host_ident(),
        any::<u64>(),
        "[A-Za-z0-9][A-Za-z0-9._-]{0,60}",
        phase(),
        held(),
        proptest::collection::vec(run_result(), 0..4),
    )
        .prop_map(|(host_id, epoch, session, phase, held, results)| {
            let mut results = results;
            results.sort_by(|a, b| a.run_id.cmp(&b.run_id));
            results.dedup_by(|a, b| a.run_id == b.run_id);
            Heartbeat {
                host_id,
                epoch,
                session,
                phase,
                held,
                results,
                taint: None,
                ..heartbeat()
            }
        })
}

fn welcome_frame() -> impl Strategy<Value = Welcome> {
    (
        1..=1_000_000_u64,
        3..=60_u64,
        any::<u64>(),
        ident(),
        "[A-Za-z0-9][A-Za-z0-9._-]{0,60}",
    )
        .prop_map(|(interval, factor, epoch, pool, session)| Welcome {
            epoch,
            heartbeat_interval_ms: interval + MIN_HEARTBEAT_INTERVAL_MS,
            grace_ms: ((interval + MIN_HEARTBEAT_INTERVAL_MS) * factor).min(MAX_GRACE_MS),
            pool,
            session,
            ..welcome()
        })
}

fn job_frame() -> impl Strategy<Value = JobSpec> {
    (
        ident(),
        ident(),
        proptest::collection::vec("[ -~]{0,30}", 0..6),
        (1..=MAX_CPU_MILLIS, 1..=MAX_MEMORY_BYTES, 1..=MAX_WALL_MS),
        (any::<u64>(), 0..=MAX_VALID_FOR_MS),
        prop_oneof![Just(Isolation::Contained), Just(Isolation::Host)],
    )
        .prop_map(
            |(run_id, lease_id, args, (cpu, memory, wall), (seq, valid_for), isolation)| {
                let mut spec = job();
                spec.run_id = run_id;
                spec.lease_id = lease_id;
                spec.args = args;
                spec.limits.cpu_millis = cpu;
                spec.limits.memory_bytes = memory;
                spec.limits.wall_ms = wall;
                spec.heartbeat_seq = seq;
                spec.valid_for_ms = valid_for;
                spec.isolation = isolation;
                spec.gpu_indices = Vec::new();
                spec
            },
        )
}

fn job_start_frame() -> impl Strategy<Value = JobStartSpec> {
    (
        ident(),
        ident(),
        proptest::collection::vec("[ -~]{0,30}", 0..6),
        (1..=MAX_CPU_MILLIS, 1..=MAX_MEMORY_BYTES, 1..=MAX_WALL_MS),
    )
        .prop_map(|(job_id, allocation_id, command, (cpu, memory, wall))| {
            let mut spec: JobStartSpec = serde_json::from_value(json!({
                "job_id": "j",
                "allocation_id": "a",
                "image": format!("ubuntu:24.04@sha256:{}", "0123456789abcdef".repeat(4)),
                "booked": {"cpu_millis": 1, "memory_bytes": 1},
                "wall_ms": 1,
            }))
            .expect("the base start decodes");
            spec.job_id = job_id;
            spec.allocation_id = allocation_id;
            spec.command = command;
            spec.booked.cpu_millis = cpu;
            spec.booked.memory_bytes = memory;
            spec.wall_ms = wall;
            spec
        })
}

fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        job_frame().prop_map(|spec| Command::Start(Box::new(spec))),
        ident().prop_map(|run_id| Command::Stop { run_id }),
        ident().prop_map(|run_id| Command::Cancel { run_id }),
        job_start_frame().prop_map(|spec| Command::JobStart(Box::new(spec))),
        ident().prop_map(|job_id| Command::JobStop { job_id }),
        Just(Command::Unknown),
    ]
}

fn reply_frame() -> impl Strategy<Value = HeartbeatReply> {
    (
        proptest::collection::vec(command(), 0..6),
        proptest::collection::btree_map(ident(), ack(), 0..6),
        any::<bool>(),
        proptest::option::of(ident()),
        idents(6),
    )
        .prop_map(
            |(commands, acks, taint_ack, exclusive, retired_acks)| HeartbeatReply {
                commands,
                result_acks: acks
                    .into_iter()
                    .map(|(run_id, ack)| ResultAck { run_id, ack })
                    .collect(),
                taint_ack,
                exclusive,
                retired_acks,
                ..HeartbeatReply::default()
            },
        )
}

fn caps_frame() -> impl Strategy<Value = Caps> {
    (
        proptest::collection::btree_set("[0-9a-f]{64}", 0..8),
        prop_oneof![Just(String::new()), "/[a-z]{1,10}(/[a-z]{1,10}){0,3}"],
        prop_oneof![Just(ReadModel::Open), Just(ReadModel::Allow)],
        proptest::collection::btree_set("[A-Za-z0-9][A-Za-z0-9._-]{0,20}", 0..=MAX_CAPS_XCODES),
    )
        .prop_map(|(bundles, bundle_root, read_model, xcode)| Caps {
            bundles: bundles
                .into_iter()
                .map(|hex| format!("sha256:{hex}"))
                .collect(),
            bundle_root,
            read_model,
            xcode: xcode.into_iter().collect(),
        })
}

fn hello_reply_frame() -> impl Strategy<Value = HelloReply> {
    prop_oneof![
        welcome_frame().prop_map(HelloReply::Welcome),
        "[a-z][a-z-]{0,30}".prop_map(|code| HelloReply::Refused(Refused {
            code,
            supported: None,
            field: None
        })),
    ]
}

// ---- helpers --------------------------------------------------------------------------

fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("a value encodes")
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Asserts that `value` survives an encode and a decode, and that the encoding of the
/// decoded value is the same bytes.
fn survives<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let bytes = encode(value);
    let back: T = decode(&bytes).expect("an encoded value decodes");
    assert_eq!(&back, value);
    assert_eq!(encode(&back), bytes);
}

// ---- generated valid frames -----------------------------------------------------------

proptest! {
    #![proptest_config(config(0x5eed_0001))]

    /// A generated hello is valid, and decoding its encoding gives it back.
    #[test]
    fn hello_round_trips_and_validates(frame in hello_frame()) {
        prop_assert_eq!(frame.validate(), Ok(()));
        survives(&frame);
    }

    /// A generated heartbeat is valid, and decoding its encoding gives it back.
    #[test]
    fn heartbeat_round_trips_and_validates(frame in heartbeat_frame()) {
        prop_assert_eq!(frame.validate(), Ok(()));
        survives(&frame);
    }

    /// A generated welcome, alone and as a hello reply, is valid and decodes to itself.
    #[test]
    fn welcome_and_hello_reply_round_trip_and_validate(frame in hello_reply_frame()) {
        if let HelloReply::Welcome(welcome) = &frame {
            prop_assert_eq!(welcome.validate(), Ok(()));
            survives(welcome);
        }
        prop_assert_eq!(frame.validate(), Ok(()));
        survives(&frame);
    }

    /// A generated reply decodes to itself, and every command of it passes, with the
    /// acknowledgements and the exclusive lease acceptable.
    #[test]
    fn heartbeat_reply_round_trips_and_validates(frame in reply_frame()) {
        survives(&frame);
        let checked = frame.validate();
        prop_assert_eq!(checked.refused.len(), 0);
        prop_assert_eq!(checked.commands.len(), frame.commands.len());
        prop_assert_eq!(checked.acks, Ok(()));
        prop_assert_eq!(checked.exclusive, Ok(()));
        prop_assert_eq!(checked.job_acks, Ok(()));
        prop_assert_eq!(checked.retired_acks, Ok(()));
        prop_assert_eq!(checked.log_grant, Ok(()));
    }

    /// A generated command, each kind of it, is valid and decodes to itself.
    #[test]
    fn command_round_trips_and_validates(frame in command()) {
        prop_assert_eq!(frame.validate(), Ok(()));
        survives(&frame);
    }

    /// A generated run start is valid and decodes to itself.
    #[test]
    fn job_spec_round_trips_and_validates(frame in job_frame()) {
        prop_assert_eq!(frame.validate(), Ok(()));
        survives(&frame);
    }

    /// A generated job start is valid, pinned, and decodes to itself.
    #[test]
    fn job_start_round_trips_and_validates(frame in job_start_frame()) {
        prop_assert_eq!(frame.validate(), Ok(()));
        prop_assert_eq!(frame.require_pinned(), Ok(()));
        survives(&frame);
    }

    /// A generated caps frame is valid and decodes to itself.
    #[test]
    fn caps_round_trips_and_validates(frame in caps_frame()) {
        prop_assert_eq!(frame.validate(), Ok(()));
        survives(&frame);
    }
}

// ---- arbitrary bytes ------------------------------------------------------------------

/// Decodes `bytes` as every frame type and validates what decodes. A decoded value must
/// encode and decode to an equal value.
fn feed(bytes: &[u8]) {
    fn each<T: Serialize + DeserializeOwned + PartialEq + Debug>(
        bytes: &[u8],
        validate: impl Fn(&T),
    ) {
        if let Ok(value) = decode::<T>(bytes) {
            validate(&value);
            survives(&value);
        }
    }
    each::<Hello>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
    each::<Heartbeat>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
    each::<Caps>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
    each::<Welcome>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
    each::<HelloReply>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
    each::<HeartbeatReply>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
    each::<Command>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
    each::<JobSpec>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
    each::<JobStartSpec>(bytes, |v| {
        let _ = std::hint::black_box(v.validate());
    });
}

/// A valid encoding of one frame of any type, for a generator that damages it.
fn encoded_frame() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        hello_frame().prop_map(|f| encode(&f)),
        heartbeat_frame().prop_map(|f| encode(&f)),
        reply_frame().prop_map(|f| encode(&f)),
        command().prop_map(|f| encode(&f)),
        job_start_frame().prop_map(|f| encode(&f)),
        caps_frame().prop_map(|f| encode(&f)),
        hello_reply_frame().prop_map(|f| encode(&f)),
    ]
}

proptest! {
    #![proptest_config(config(0x5eed_0002))]

    /// Arbitrary bytes, of a bounded length, fed to every decode and every validate never
    /// panic, and whatever decodes survives an encode and a decode.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        feed(&bytes);
    }

    /// Arbitrary text that looks like JSON (braces, quotes, the field names of the wire)
    /// reaches deeper into the decoders than random bytes do, and never panics.
    #[test]
    fn json_shaped_text_never_panics(
        text in r#"[{}\[\]":,0-9a-z_ ]{0,400}|\{"(type|commands|held|run_id|job_id)":[^{}]{0,60}\}"#
    ) {
        feed(text.as_bytes());
    }

    /// A valid frame with one byte replaced, one cut off the end, or one spliced in never
    /// panics a decode or a validate.
    #[test]
    fn damaged_frames_never_panic(
        frame in encoded_frame(),
        at in any::<prop::sample::Index>(),
        byte in any::<u8>(),
    ) {
        let at = at.index(frame.len());
        let mut replaced = frame.clone();
        replaced[at] = byte;
        feed(&replaced);
        feed(&frame[..at]);
        let mut spliced = frame;
        spliced.insert(at, byte);
        feed(&spliced);
    }

    /// Every proper prefix of a valid frame is a truncated body: the decode ends in an end
    /// of input error and no frame comes out of it.
    #[test]
    fn every_proper_prefix_is_an_eof_error(frame in encoded_frame(), at in any::<prop::sample::Index>()) {
        let cut = at.index(frame.len());
        let err = decode::<Value>(&frame[..cut]).unwrap_err();
        prop_assert!(err.is_eof(), "{err}");
    }
}

// ---- hand-written malformed input -----------------------------------------------------

fn hello_bytes() -> Vec<u8> {
    encode(&hello())
}

/// Asserts the category and a piece of the message of a failed decode.
fn refuses<T: DeserializeOwned + Debug>(bytes: &[u8], eof: bool, syntax: bool, text: &str) {
    let err = decode::<T>(bytes).expect_err("the body must not decode");
    assert_eq!(
        (err.is_eof(), err.is_syntax(), err.is_data()),
        (eof, syntax, !eof && !syntax)
    );
    assert!(err.to_string().contains(text), "{err}");
}

/// A body cut short, at any byte before its end, is an end of input error for every type
/// that reads a body: the decoder never returns a half-read frame.
#[test]
fn truncated_body_is_eof_for_every_type() {
    let full = hello_bytes();
    for cut in 0..full.len() {
        let part = &full[..cut];
        assert!(decode::<Hello>(part).unwrap_err().is_eof(), "cut {cut}");
        assert!(
            decode::<HelloReply>(part).unwrap_err().is_eof(),
            "cut {cut}"
        );
        assert!(decode::<Command>(part).unwrap_err().is_eof(), "cut {cut}");
    }
    refuses::<Heartbeat>(
        br#"{"host_id":"h","epoch":"#,
        true,
        false,
        "EOF while parsing",
    );
    refuses::<HeartbeatReply>(
        br#"{"commands":[{"type":"stop""#,
        true,
        false,
        "EOF while parsing",
    );
    refuses::<Welcome>(b"", true, false, "EOF while parsing a value");
}

/// Bytes that are not UTF-8, inside a string or outside any, are a syntax error and not
/// a frame with a replacement character in it.
#[test]
fn invalid_utf8_is_a_syntax_error() {
    let mut inside = hello_bytes();
    let at = inside
        .windows(6)
        .position(|w| w == b"mac-01")
        .expect("the host id is in the body");
    inside[at] = 0xff;
    refuses::<Hello>(&inside, false, true, "invalid unicode code point");
    refuses::<Hello>(b"\xff\xfe{}", false, true, "expected value");
    refuses::<Command>(
        b"{\"type\":\"stop\",\"run_id\":\"\xc3\x28\"}",
        false,
        true,
        "invalid unicode code point",
    );
    refuses::<HeartbeatReply>(
        b"{\"exclusive\":\"\xed\xa0\x80\"}",
        false,
        true,
        "invalid unicode code point",
    );
}

/// Bytes after the end of the body are a syntax error that names them; white space after
/// it is not.
#[test]
fn trailing_bytes_after_the_body_are_refused() {
    let mut body = hello_bytes();
    body.extend_from_slice(b" \n\t");
    assert_eq!(
        decode::<Hello>(&body).expect("white space may trail"),
        hello()
    );
    let mut body = hello_bytes();
    body.extend_from_slice(b"x");
    refuses::<Hello>(&body, false, true, "trailing characters");
    let mut body = encode(&HeartbeatReply::default());
    body.extend_from_slice(&encode(&HeartbeatReply::default()));
    refuses::<HeartbeatReply>(&body, false, true, "trailing characters");
    let mut body = encode(&Command::Unknown);
    body.push(0);
    refuses::<Command>(&body, false, true, "trailing characters");
}

/// A tag the type does not know: refused with the name of the tag where the type refuses
/// unknown values, and the documented lenient value where it does not.
#[test]
fn unknown_tag_is_refused_or_reads_as_the_unknown_value() {
    refuses::<HelloReply>(
        br#"{"type":"promoted"}"#,
        false,
        false,
        "unknown variant `promoted`",
    );
    refuses::<Command>(br#"{"stop":"r"}"#, false, false, "missing field `type`");
    let mut spec = serde_json::to_value(job()).expect("a job encodes");
    spec["isolation"] = json!("vm");
    let body = serde_json::to_vec(&spec).expect("a value encodes");
    refuses::<JobSpec>(&body, false, false, "unknown variant `vm`");

    assert_eq!(
        decode::<Command>(br#"{"type":"launch","run_id":"r"}"#).expect("lenient"),
        Command::Unknown
    );
    let reply: HeartbeatReply =
        decode(br#"{"commands":[{"type":"launch"},{"type":"stop","run_id":"r"}]}"#)
            .expect("lenient");
    assert_eq!(
        reply.commands,
        vec![Command::Unknown, Command::Stop { run_id: "r".into() }]
    );
    let ack: ResultAck = decode(br#"{"run_id":"r","ack":"EXPLODED"}"#).expect("lenient");
    assert_eq!(ack.ack, Ack::Unknown);
    let result: RunResult =
        decode(br#"{"run_id":"r","lease_id":"l","outcome":"exploded"}"#).expect("lenient");
    assert_eq!(result.outcome, Outcome::Unknown);
    let mut frame = serde_json::to_value(hello()).expect("a hello encodes");
    frame["phase"] = json!("hovering");
    let frame: Hello = serde_json::from_value(frame).expect("an unknown phase fails closed");
    assert_eq!(frame.phase, Phase::Quarantined);
}

/// A field of the wrong JSON type is a data error that names the type it found.
#[test]
fn wrong_field_type_is_a_data_error() {
    let with = |key: &str, value: Value| {
        let mut frame = serde_json::to_value(heartbeat()).expect("a heartbeat encodes");
        frame[key] = value;
        serde_json::to_vec(&frame).expect("a value encodes")
    };
    refuses::<Heartbeat>(
        &with("epoch", json!("three")),
        false,
        false,
        "invalid type: string \"three\"",
    );
    refuses::<Heartbeat>(
        &with("epoch", json!(-1)),
        false,
        false,
        "invalid value: integer `-1`",
    );
    refuses::<Heartbeat>(
        &with("epoch", json!(1.5)),
        false,
        false,
        "invalid type: floating point `1.5`",
    );
    refuses::<Heartbeat>(
        &with("held", json!({"run_id": "r"})),
        false,
        false,
        "invalid type: map",
    );
    refuses::<Heartbeat>(
        &with("host_id", json!(7)),
        false,
        false,
        "invalid type: integer `7`",
    );
    refuses::<Heartbeat>(
        &with("phase", json!(["serving"])),
        false,
        false,
        "invalid type: sequence",
    );
    refuses::<Welcome>(
        br#"{"epoch":1,"heartbeat_interval_ms":null,"grace_ms":1,"pool":"p","session":"s"}"#,
        false,
        false,
        "invalid type: null",
    );
    refuses::<Heartbeat>(br#"[]"#, false, false, "invalid length 0");
    refuses::<Heartbeat>(br#"{"host_id":"h"}"#, false, false, "missing field `epoch`");
}

/// Applies `edit` to the JSON of `value` and returns the body.
fn edited<T: Serialize>(value: &T, edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut json = serde_json::to_value(value).expect("a value encodes");
    edit(&mut json);
    serde_json::to_vec(&json).expect("a value encodes")
}

/// Asserts the field and the rule a decoded frame is refused with.
fn refused_for<T: DeserializeOwned>(
    bytes: &[u8],
    validate: impl Fn(&T) -> Result<(), js_agent_wire::ValidationError>,
    field: &str,
    rule: Rule,
) {
    let frame = decode::<T>(bytes).expect("the body decodes; only validate refuses it");
    let err = validate(&frame).expect_err("the frame must be refused");
    assert_eq!((err.field(), err.rule()), (field, rule), "{err}");
}

/// A field over its cap in `limits` decodes, and `validate` refuses it with the rule that
/// names the cap and the field that broke it.
#[test]
fn oversized_fields_are_refused_by_validate() {
    let many = |n: usize| {
        (0..n)
            .map(|i| json!({"run_id": format!("r{i}"), "state": "starting"}))
            .collect::<Vec<_>>()
    };
    refused_for::<Heartbeat>(
        &edited(&heartbeat(), |f| f["held"] = json!(many(MAX_HELD + 1))),
        Heartbeat::validate,
        "held",
        Rule::TooMany,
    );
    refused_for::<Heartbeat>(
        &edited(&heartbeat(), |f| {
            f["session"] = json!("a".repeat(MAX_SESSION_LEN + 1))
        }),
        Heartbeat::validate,
        "session",
        Rule::TooLong,
    );
    refused_for::<Hello>(
        &edited(&hello(), |f| {
            f["host_id"] = json!("a".repeat(MAX_ID_LEN + 1))
        }),
        Hello::validate,
        "host_id",
        Rule::TooLong,
    );
    let labels: BTreeMap<String, String> = (0..=MAX_LABELS)
        .map(|i| (format!("k{i}"), "v".into()))
        .collect();
    refused_for::<Hello>(
        &edited(&hello(), |f| f["labels"] = json!(labels)),
        Hello::validate,
        "labels",
        Rule::TooMany,
    );
    let bundles: Vec<String> = (0..=MAX_CAPS_BUNDLES)
        .map(|i| format!("sha256:{i:064x}"))
        .collect();
    refused_for::<Caps>(
        &encode(&json!({"bundles": bundles})),
        Caps::validate,
        "caps.bundles",
        Rule::TooMany,
    );
    let xcode: Vec<String> = (0..=MAX_CAPS_XCODES).map(|i| format!("b{i}")).collect();
    refused_for::<Caps>(
        &encode(&json!({"xcode": xcode})),
        Caps::validate,
        "caps.xcode",
        Rule::TooMany,
    );
    refused_for::<JobSpec>(
        &edited(&job(), |f| f["args"] = json!(vec!["x"; MAX_ARGS + 1])),
        JobSpec::validate,
        "args",
        Rule::TooMany,
    );
    refused_for::<JobSpec>(
        &edited(&job(), |f| {
            f["image"] = json!("a".repeat(MAX_IMAGE_LEN + 1))
        }),
        JobSpec::validate,
        "image",
        Rule::TooLong,
    );
    refused_for::<Welcome>(
        &edited(&welcome(), |f| {
            f["pool"] = json!("a".repeat(MAX_ID_LEN + 1))
        }),
        Welcome::validate,
        "pool",
        Rule::TooLong,
    );

    // A reply refuses by command, never as a whole: the stops beside an oversized start
    // and past the command cap are still read.
    let stops = (0..=MAX_COMMANDS)
        .map(|i| json!({"type": "stop", "run_id": format!("r{i}")}))
        .collect::<Vec<_>>();
    let reply: HeartbeatReply =
        decode(&encode(&json!({"commands": stops}))).expect("the reply decodes");
    let checked = reply.validate();
    assert_eq!(checked.commands.len(), MAX_COMMANDS);
    assert_eq!(checked.refused.len(), 1);
    assert_eq!(checked.refused[0].index, MAX_COMMANDS);
    assert_eq!(
        (
            checked.refused[0].error.field(),
            checked.refused[0].error.rule()
        ),
        ("commands", Rule::TooMany)
    );
    let acks = (0..=MAX_RESULTS)
        .map(|i| json!({"run_id": format!("r{i}"), "ack": "ACCEPTED"}))
        .collect::<Vec<_>>();
    let reply: HeartbeatReply =
        decode(&encode(&json!({"result_acks": acks}))).expect("the reply decodes");
    let err = reply
        .validate()
        .acks
        .expect_err("too many acknowledgements");
    assert_eq!((err.field(), err.rule()), ("result_acks", Rule::TooMany));
}
