//! The live-log fields: `log_sink` on a job start, `log_grant` on a reply, `log_grant_state`
//! on a heartbeat and `logs` on a job's result. Their wire form, the checks that set a bad
//! one aside without refusing the message around it, their sizes, and that no secret of a
//! grant reaches a `Debug` text.

mod common;

use common::{expect, heartbeat, round_trip};
use js_agent_wire::limits::{
    MAX_ID_LEN, MAX_LOG_BUCKET_LEN, MAX_LOG_CREDENTIAL_FIELD, MAX_LOG_ENDPOINT_LEN,
    MAX_LOG_JOB_ID_LEN, MAX_LOG_PREFIX_LEN, MAX_LOG_REGION_LEN, MAX_LOG_SESSION_TOKEN,
    REPLY_BODY_BUDGET,
};
use js_agent_wire::{
    Ack, BookedResources, Command, Heartbeat, HeartbeatReply, HeldJob, HeldState, JobResult,
    JobResultAck, JobStartSpec, LOG_UPLOAD_CAPABILITY, LogGrant, LogGrantState, LogReport, LogSink,
    Outcome, ResultAck, Rule, refusal,
};
use serde_json::{Value, json};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HEARTBEAT: &str = include_str!("fixtures/v0.1.135/heartbeat.json");
const REPLY: &str = include_str!("fixtures/v0.1.135/reply.json");

/// A valid job start of the job `job_id`, with no sink.
fn start_of(job_id: &str) -> JobStartSpec {
    JobStartSpec {
        job_id: job_id.into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        image: format!("ubuntu:24.04@{DIGEST}"),
        command: vec!["make".into()],
        run: None,
        os: None,
        arch: None,
        env: Default::default(),
        files: Vec::new(),
        booked: BookedResources {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
    }
}

/// The sink of the design's worked example: bucket `logs`, prefix `js`, job `build-42`.
fn sink() -> LogSink {
    LogSink {
        endpoint: "https://store.example".into(),
        region: "us-east-1".into(),
        bucket: "logs".into(),
        prefix: "js/build-42/".into(),
        path_style: true,
    }
}

/// The start of `build-42` with [`sink`].
fn start() -> JobStartSpec {
    JobStartSpec {
        log_sink: Some(sink()),
        ..start_of("build-42")
    }
}

/// A grant with a token.
fn grant() -> LogGrant {
    LogGrant {
        access_key_id: "testkeyid-0001".into(),
        secret_access_key: "test-secret/+=value".into(),
        session_token: Some("test-token/+=value".into()),
        expires_at_ms: 1_800_000_000_000,
    }
}

/// A stop of the run `run`.
fn stop(run: &str) -> Command {
    Command::Stop { run_id: run.into() }
}

/// What `log_sink_checked` says of the start of `build-42` with the sink `edit` makes.
fn sink_error(edit: impl FnOnce(&mut LogSink)) -> js_agent_wire::ValidationError {
    let mut spec = start();
    edit(spec.log_sink.as_mut().unwrap());
    spec.log_sink_checked().unwrap_err()
}

/// What the reply check says of the grant `edit` makes.
fn grant_check(edit: impl FnOnce(&mut LogGrant)) -> Result<(), js_agent_wire::ValidationError> {
    let mut value = grant();
    edit(&mut value);
    let reply = HeartbeatReply {
        log_grant: Some(value),
        ..HeartbeatReply::default()
    };
    reply.validate().log_grant
}

/// A job's result with the report `logs`.
fn result_with(logs: Option<LogReport>) -> JobResult {
    JobResult {
        job_id: "build-42".into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        outcome: Outcome::Succeeded,
        exit_code: Some(0),
        detail: None,
        log_tail: None,
        logs,
    }
}

/// A report of `bytes` and `dropped` on each stream.
fn report(stdout: (u64, u64), stderr: (u64, u64)) -> LogReport {
    LogReport {
        stdout_bytes: stdout.0,
        stdout_dropped: stdout.1,
        stderr_bytes: stderr.0,
        stderr_dropped: stderr.1,
    }
}

/// Asserts the capability's text. Catches a rename, which would make every server read an
/// uploading agent as one that cannot upload.
#[test]
fn the_log_upload_capability_is_named_log_upload() {
    assert_eq!(LOG_UPLOAD_CAPABILITY, "log-upload");
}

/// Asserts the exact text of a job start with no sink, byte for byte, and that the previous
/// release's heartbeat and reply decode with no log field and encode again to the same
/// bytes as the files hold them. Catches a `log_sink` (or any log field) sent when there is
/// none, which would move every existing golden and every deployed peer's view of a start.
#[test]
fn a_job_start_without_log_sink_encodes_as_before() {
    let text = serde_json::to_string(&Command::JobStart(Box::new(start_of("build-42")))).unwrap();
    assert_eq!(
        text,
        format!(
            "{{\"type\":\"job_start\",\"job_id\":\"build-42\",\
             \"allocation_id\":\"0123456789abcdef0123456789abcdef\",\
             \"image\":\"ubuntu:24.04@{DIGEST}\",\"command\":[\"make\"],\
             \"booked\":{{\"cpu_millis\":1000,\"memory_bytes\":1073741824}},\"wall_ms\":60000}}"
        )
    );
    assert!(!text.contains("log_sink"));

    let file = |text: &str| serde_json::from_str::<Value>(text).unwrap();
    let beat: Heartbeat = serde_json::from_str(HEARTBEAT).unwrap();
    assert_eq!(beat.log_grant_state, None);
    assert_eq!(serde_json::to_value(&beat).unwrap(), file(HEARTBEAT));
    let reply: HeartbeatReply = serde_json::from_str(REPLY).unwrap();
    assert_eq!(reply.log_grant, None);
    assert_eq!(serde_json::to_value(&reply).unwrap(), file(REPLY));
}

/// Asserts the exact JSON of a start with a sink, that it decodes to the same start (inside
/// a reply too), that the sink passes its check, and that `path_style` absent reads as
/// `false`. Catches a renamed sink field, a sink lost on decode, and a `path_style` that a
/// sender must always send.
#[test]
fn a_job_start_with_log_sink_round_trips() {
    let command = Command::JobStart(Box::new(start()));
    let wire = serde_json::to_value(&command).unwrap();
    assert_eq!(
        wire["log_sink"],
        json!({
            "endpoint": "https://store.example",
            "region": "us-east-1",
            "bucket": "logs",
            "prefix": "js/build-42/",
            "path_style": true
        })
    );
    assert_eq!(serde_json::from_value::<Command>(wire).unwrap(), command);
    round_trip(&start());
    assert_eq!(start().log_sink_checked(), Ok(Some(&sink())));
    assert_eq!(start_of("build-42").log_sink_checked(), Ok(None));

    let reply = HeartbeatReply {
        commands: vec![command.clone()],
        ..HeartbeatReply::default()
    };
    round_trip(&reply);
    let checked = reply.validate();
    assert_eq!(checked.commands, [&command]);

    let mut short = serde_json::to_value(start()).unwrap();
    short["log_sink"]
        .as_object_mut()
        .unwrap()
        .remove("path_style");
    let decoded: JobStartSpec = serde_json::from_value(short).unwrap();
    assert!(!decoded.log_sink.unwrap().path_style);
}

/// Asserts that a sink whose prefix is another job's, or one that merely ends in the same
/// letters, fails the sink check with `invalid-log-sink` naming `log_sink.prefix`, and that
/// a prefix of the job id alone (an empty configured prefix) passes. Catches a start that
/// points one job's logs at another job's objects.
#[test]
fn a_log_sink_for_another_jobs_prefix_fails_the_sink_check() {
    for other in ["js/build-43/", "js/xbuild-42/", "js/build-42/build-43/"] {
        let err = sink_error(|sink| sink.prefix = other.into());
        expect(
            err,
            "log_sink.prefix",
            Rule::Inconsistent,
            refusal::INVALID_LOG_SINK,
        );
    }
    let mut bare = start();
    bare.log_sink.as_mut().unwrap().prefix = "build-42/".into();
    assert!(bare.log_sink_checked().is_ok());
    let mut nested = start();
    nested.log_sink.as_mut().unwrap().prefix = "a/b.c/d_e-f/build-42/".into();
    assert!(nested.log_sink_checked().is_ok());
}

/// Asserts that a start whose sink fails its check, in each part, still passes `validate`
/// and `require_pinned`, and is accepted in a reply. Catches a sink folded into the start's
/// acceptance, which would make a logging defect fail a job.
#[test]
fn a_bad_log_sink_does_not_fail_the_start() {
    let bad = [
        LogSink {
            endpoint: "file:///var/log".into(),
            ..sink()
        },
        LogSink {
            region: String::new(),
            ..sink()
        },
        LogSink {
            bucket: "a b".into(),
            ..sink()
        },
        LogSink {
            prefix: "js/other/".into(),
            ..sink()
        },
    ];
    for sink in bad {
        let spec = JobStartSpec {
            log_sink: Some(sink),
            ..start_of("build-42")
        };
        assert!(spec.log_sink_checked().is_err());
        assert_eq!(spec.validate(), Ok(()));
        assert_eq!(spec.require_pinned(), Ok(()));
        let command = Command::JobStart(Box::new(spec));
        let reply = HeartbeatReply {
            commands: vec![command.clone()],
            ..HeartbeatReply::default()
        };
        let checked = reply.validate();
        assert!(checked.refused.is_empty(), "{:?}", checked.refused);
        assert_eq!(checked.commands, [&command]);
    }
}

/// Asserts the bound of every part of a sink, one case at a time: the endpoint's scheme,
/// user, path, query, fragment, host, port and length, `http` only to a loopback host; the
/// region and the bucket; the prefix's trailing `/`, segments and length. Each value at its
/// cap passes. Catches a check that lets a user part or a path into the URL, a plain-text
/// endpoint off the host, and a prefix that climbs out of the log prefix or widens a policy
/// resource with a wildcard.
#[test]
fn log_sink_bounds_are_enforced() {
    let code = refusal::INVALID_LOG_SINK;
    let at_cap = format!("https://{}", "a".repeat(MAX_LOG_ENDPOINT_LEN - 8));
    let over_cap = format!("https://{}", "a".repeat(MAX_LOG_ENDPOINT_LEN - 7));
    let good = [
        "https://store.example",
        "https://store.example/",
        "https://store.example:9000",
        "https://store-1.example",
        "https://192.0.2.10:443",
        "https://[::1]",
        "https://[2001:db8::1]:9000",
        "http://localhost:9000",
        "http://LocalHost",
        "http://127.0.0.1:9000/",
        "http://[::1]:9000",
        at_cap.as_str(),
    ];
    for endpoint in good {
        let mut spec = start();
        spec.log_sink.as_mut().unwrap().endpoint = endpoint.into();
        assert!(spec.log_sink_checked().is_ok(), "{endpoint}");
    }
    let bad = [
        ("", Rule::BadChars),
        ("file:///var/log", Rule::BadChars),
        ("ftp://store.example", Rule::BadChars),
        ("store.example", Rule::BadChars),
        ("https://user:pw@store.example", Rule::BadChars),
        ("https://store.example/logs", Rule::BadChars),
        ("https://store.example?x=1", Rule::BadChars),
        ("https://store.example#top", Rule::BadChars),
        ("https://", Rule::BadChars),
        ("https://:9000", Rule::BadChars),
        ("https://store.example:", Rule::BadChars),
        ("https://store.example:+80", Rule::BadChars),
        ("https://store.example:0", Rule::BadChars),
        ("https://store.example:65536", Rule::BadChars),
        ("https://store_example", Rule::BadChars),
        ("https://[zz]", Rule::BadChars),
        ("https://[::1", Rule::BadChars),
        ("http://store.example", Rule::Escapes),
        ("http://192.0.2.10", Rule::Escapes),
        (over_cap.as_str(), Rule::TooLong),
    ];
    for (endpoint, rule) in bad {
        let err = sink_error(|sink| sink.endpoint = endpoint.into());
        expect(err, "log_sink.endpoint", rule, code);
    }

    let names = [
        ("log_sink.region", MAX_LOG_REGION_LEN),
        ("log_sink.bucket", MAX_LOG_BUCKET_LEN),
    ];
    for (field, cap) in names {
        let set = |sink: &mut LogSink, value: String| {
            if field == "log_sink.region" {
                sink.region = value;
            } else {
                sink.bucket = value;
            }
        };
        let mut spec = start();
        set(spec.log_sink.as_mut().unwrap(), "a".repeat(cap));
        assert!(spec.log_sink_checked().is_ok(), "{field}");
        let cases = [
            (String::new(), Rule::Empty),
            ("a".repeat(cap + 1), Rule::TooLong),
            ("a b".into(), Rule::BadChars),
            ("a/b".into(), Rule::BadChars),
            ("..".into(), Rule::BadChars),
            ("-a".into(), Rule::BadChars),
        ];
        for (value, rule) in cases {
            let err = sink_error(|sink| set(sink, value));
            expect(err, field, rule, code);
        }
    }

    let job = "j".repeat(63);
    let longest = format!("{}/{job}/", "p".repeat(512));
    assert_eq!(longest.len(), MAX_LOG_PREFIX_LEN);
    let mut spec = JobStartSpec {
        log_sink: Some(sink()),
        ..start_of(&job)
    };
    spec.log_sink.as_mut().unwrap().prefix = longest;
    assert_eq!(spec.validate(), Ok(()));
    assert!(spec.log_sink_checked().is_ok());
    spec.log_sink.as_mut().unwrap().prefix = format!("{}/{job}/", "p".repeat(513));
    let err = spec.log_sink_checked().unwrap_err();
    expect(err, "log_sink.prefix", Rule::TooLong, code);

    let job = "j".repeat(MAX_LOG_JOB_ID_LEN + 1);
    let mut spec = JobStartSpec {
        log_sink: Some(sink()),
        ..start_of(&job)
    };
    spec.log_sink.as_mut().unwrap().prefix = format!("js/{job}/");
    assert_eq!(spec.validate(), Ok(()));
    let err = spec.log_sink_checked().unwrap_err();
    expect(err, "log_sink.prefix", Rule::TooLong, code);

    let prefixes = [
        "js/build-42",
        "/",
        "",
        "/js/build-42/",
        "js//build-42/",
        "js/../build-42/",
        "js/./build-42/",
        "../build-42/",
        "js/*/build-42/",
        "js/a?b/build-42/",
        "js/a b/build-42/",
    ];
    for prefix in prefixes {
        let err = sink_error(|sink| sink.prefix = prefix.into());
        expect(err, "log_sink.prefix", Rule::BadChars, code);
    }
}

/// Asserts the exact JSON of a grant, that it round-trips with and without a token, that
/// every field at its cap is accepted, that a field one byte over, an empty one, one with a
/// space or a control character and an expiry of zero are each refused with
/// `invalid-log-credential` naming the field, and that a refused grant costs the reply
/// nothing else: its commands and acknowledgements still pass. Catches an unbounded token,
/// a header-breaking byte let through, and a bad grant that drops a stop.
#[test]
fn a_log_grant_round_trips_and_is_bounded() {
    assert_eq!(
        serde_json::to_value(grant()).unwrap(),
        json!({
            "access_key_id": "testkeyid-0001",
            "secret_access_key": "test-secret/+=value",
            "session_token": "test-token/+=value",
            "expires_at_ms": 1_800_000_000_000u64
        })
    );
    round_trip(&grant());
    let tokenless = LogGrant {
        session_token: None,
        ..grant()
    };
    assert!(
        !serde_json::to_string(&tokenless)
            .unwrap()
            .contains("session_token")
    );
    round_trip(&tokenless);
    let reply = HeartbeatReply {
        log_grant: Some(grant()),
        ..HeartbeatReply::default()
    };
    round_trip(&reply);
    assert_eq!(reply.validate().log_grant, Ok(()));
    assert_eq!(HeartbeatReply::default().validate().log_grant, Ok(()));
    assert_eq!(grant_check(|g| g.session_token = None), Ok(()));

    let field = MAX_LOG_CREDENTIAL_FIELD;
    let maximal = |g: &mut LogGrant| {
        g.access_key_id = "A".repeat(field);
        g.secret_access_key = "+/=".repeat(field / 3) + &"k".repeat(field % 3);
        g.session_token = Some("t".repeat(MAX_LOG_SESSION_TOKEN));
        g.expires_at_ms = u64::MAX;
    };
    assert_eq!(grant_check(maximal), Ok(()));

    let code = refusal::INVALID_LOG_CREDENTIAL;
    let token = "log_grant.session_token";
    let key = "log_grant.access_key_id";
    let secret = "log_grant.secret_access_key";
    let cases: [(fn(&mut LogGrant), &str, Rule); 11] = [
        (
            |g| g.session_token = Some("t".repeat(MAX_LOG_SESSION_TOKEN + 1)),
            token,
            Rule::TooLong,
        ),
        (
            |g| g.session_token = Some(String::new()),
            token,
            Rule::Empty,
        ),
        (
            |g| g.session_token = Some("a\nb".into()),
            token,
            Rule::BadChars,
        ),
        (|g| g.access_key_id = "A".repeat(129), key, Rule::TooLong),
        (|g| g.access_key_id = String::new(), key, Rule::Empty),
        (|g| g.access_key_id = "AKIA KEY".into(), key, Rule::BadChars),
        (|g| g.access_key_id = "AKIAÄ".into(), key, Rule::BadChars),
        (
            |g| g.secret_access_key = "s".repeat(129),
            secret,
            Rule::TooLong,
        ),
        (|g| g.secret_access_key = String::new(), secret, Rule::Empty),
        (
            |g| g.secret_access_key = "s\u{7f}".into(),
            secret,
            Rule::BadChars,
        ),
        (
            |g| g.expires_at_ms = 0,
            "log_grant.expires_at_ms",
            Rule::OutOfRange,
        ),
    ];
    for (edit, field, rule) in cases {
        expect(grant_check(edit).unwrap_err(), field, rule, code);
    }

    let mut bad = grant();
    bad.secret_access_key = String::new();
    let reply = HeartbeatReply {
        commands: vec![stop("r1")],
        result_acks: vec![ResultAck {
            run_id: "r2".into(),
            ack: Ack::Accepted,
        }],
        job_acks: vec![JobResultAck {
            job_id: "build-41".into(),
            ack: Ack::Accepted,
        }],
        log_grant: Some(bad),
        ..HeartbeatReply::default()
    };
    let checked = reply.validate();
    assert_eq!(checked.commands, [&stop("r1")]);
    assert!(checked.refused.is_empty());
    assert_eq!((checked.acks, checked.job_acks), (Ok(()), Ok(())));
    expect(checked.log_grant.unwrap_err(), secret, Rule::Empty, code);
}

/// Asserts the exact JSON of a grant state, with and without a credential held, that each
/// round-trips inside a heartbeat that still validates, and that an empty object reads as
/// not stale and holding none. Catches a renamed member and a state that a heartbeat check
/// refuses.
#[test]
fn a_log_grant_state_round_trips() {
    let held = LogGrantState {
        stale: false,
        until_ms: Some(1_800_000_000_000),
    };
    let none = LogGrantState {
        stale: true,
        until_ms: None,
    };
    assert_eq!(
        serde_json::to_value(held).unwrap(),
        json!({"stale": false, "until_ms": 1_800_000_000_000u64})
    );
    assert_eq!(serde_json::to_value(none).unwrap(), json!({"stale": true}));
    for state in [held, none] {
        round_trip(&state);
        let beat = Heartbeat {
            log_grant_state: Some(state),
            ..heartbeat()
        };
        assert_eq!(beat.validate(), Ok(()));
        let back: Heartbeat = serde_json::from_value(serde_json::to_value(&beat).unwrap()).unwrap();
        assert_eq!(back, beat);
    }
    let empty: LogGrantState = serde_json::from_value(json!({})).unwrap();
    assert_eq!(empty, LogGrantState::default());
}

/// A reply as busy as a full host makes one: 96 stops and 96 job stops, 96 run, job and
/// retired acknowledgements, all at the longest ids, and 8 job starts each with a sink at
/// every cap.
fn busy_reply() -> HeartbeatReply {
    let wide = |i: usize| format!("{i:0>width$}", width = MAX_ID_LEN);
    let starts = (0..8).map(|i| {
        let job = format!("{i:0>63}");
        let sink = maximal_sink(&job);
        Command::JobStart(Box::new(JobStartSpec {
            log_sink: Some(sink),
            ..start_of(&job)
        }))
    });
    let stops = (0..96).map(|i| stop(&wide(i)));
    let job_stops = (100..196).map(|i| Command::JobStop { job_id: wide(i) });
    HeartbeatReply {
        commands: starts.chain(stops).chain(job_stops).collect(),
        result_acks: (0..96)
            .map(|i| ResultAck {
                run_id: wide(i),
                ack: Ack::Accepted,
            })
            .collect(),
        taint_ack: true,
        exclusive: None,
        job_acks: (200..296)
            .map(|i| JobResultAck {
                job_id: wide(i),
                ack: Ack::Accepted,
            })
            .collect(),
        retired_acks: (300..396).map(wide).collect(),
        log_grant: None,
    }
}

/// A sink of the job `job` (63 bytes) with every part at its cap.
fn maximal_sink(job: &str) -> LogSink {
    LogSink {
        endpoint: format!("https://{}", "e".repeat(MAX_LOG_ENDPOINT_LEN - 8)),
        region: "r".repeat(MAX_LOG_REGION_LEN),
        bucket: "b".repeat(MAX_LOG_BUCKET_LEN),
        prefix: format!("{}/{job}/", "p".repeat(512)),
        path_style: false,
    }
}

/// Asserts that a grant at every cap adds exactly 4,464 bytes of JSON to a reply and a sink
/// at every cap exactly 1,298 to a start (the figures of `docs/design/agent-wire.md`), and
/// that a busy reply with such a grant, every command valid, stays within a quarter of
/// `REPLY_BODY_BUDGET`, the body the agent reads. Catches a cap raised past what a reply can
/// carry and a figure in the design that the code no longer meets. Fix the caps, not this
/// threshold.
#[test]
fn a_reply_with_a_maximum_credential_fits_the_reply_bound() {
    let len = |value: &HeartbeatReply| serde_json::to_string(value).unwrap().len();
    let busy = busy_reply();
    let checked = busy.validate();
    assert!(checked.refused.is_empty(), "{:?}", checked.refused);
    assert_eq!(checked.commands.len(), 8 + 96 + 96);
    let mut maximal = busy.clone();
    maximal.log_grant = Some(LogGrant {
        access_key_id: "A".repeat(MAX_LOG_CREDENTIAL_FIELD),
        secret_access_key: "s".repeat(MAX_LOG_CREDENTIAL_FIELD),
        session_token: Some("t".repeat(MAX_LOG_SESSION_TOKEN)),
        expires_at_ms: u64::MAX,
    });
    assert_eq!(maximal.validate().log_grant, Ok(()));
    assert_eq!(len(&maximal) - len(&busy), 4_464);
    assert!(
        len(&maximal) * 4 <= REPLY_BODY_BUDGET,
        "{} bytes",
        len(&maximal)
    );
    assert_eq!(REPLY_BODY_BUDGET, 1_048_576);

    let job = "j".repeat(63);
    let bare = start_of(&job);
    let with = JobStartSpec {
        log_sink: Some(maximal_sink(&job)),
        ..bare.clone()
    };
    assert!(with.log_sink_checked().is_ok());
    let text = |spec: &JobStartSpec| serde_json::to_string(spec).unwrap().len();
    assert_eq!(text(&with) - text(&bare), 1_298);
}

/// Asserts that a heartbeat with no `log_grant_state`, a listing whose result has no
/// `logs`, a reply with no `log_grant` and a start with no `log_sink` carry no key of the
/// field at all. Catches an absent log field sent as `null`, which an older peer would read
/// as a value.
#[test]
fn log_fields_are_omitted_when_empty() {
    let beat = Heartbeat {
        jobs: vec![HeldJob {
            job_id: "build-42".into(),
            state: HeldState::ExitedUnacked,
            result: Some(result_with(None)),
        }],
        ..heartbeat()
    };
    let wire = serde_json::to_value(&beat).unwrap();
    assert!(wire.get("log_grant_state").is_none(), "{wire}");
    assert!(wire["jobs"][0]["result"].get("logs").is_none(), "{wire}");
    let reply = serde_json::to_value(HeartbeatReply::default()).unwrap();
    assert_eq!(
        reply,
        json!({"commands": [], "result_acks": [], "taint_ack": false})
    );
    let start = serde_json::to_value(start_of("build-42")).unwrap();
    assert!(start.get("log_sink").is_none(), "{start}");
}

/// Asserts that a listing whose report drops more than a stream had (on either stream)
/// still validates, alone and inside a heartbeat, and that `logs_report` then gives `None`;
/// a report that drops all or none of a stream is given back. Catches an inconsistent
/// report that refuses the heartbeat (and with it every result the beat carries) and one
/// that reaches the server's store.
#[test]
fn an_inconsistent_logs_report_does_not_refuse_the_heartbeat() {
    for bad in [report((10, 11), (0, 0)), report((0, 0), (5, 6))] {
        let result = result_with(Some(bad));
        assert_eq!(result.validate(), Ok(()));
        assert_eq!(result.logs_report(), None);
        let job = HeldJob {
            job_id: "build-42".into(),
            state: HeldState::ExitedUnacked,
            result: Some(result),
        };
        assert_eq!(job.validate(), Ok(()));
        let beat = Heartbeat {
            jobs: vec![job],
            ..heartbeat()
        };
        assert_eq!(beat.validate(), Ok(()));
    }
    for good in [report((10, 10), (5, 0)), report((0, 0), (0, 0))] {
        assert_eq!(result_with(Some(good)).logs_report(), Some(&good));
    }
    assert_eq!(result_with(None).logs_report(), None);
}

/// Asserts the exact JSON of a report and that it round-trips inside a result, a listing
/// and a heartbeat. Catches a renamed member, which a server would read as no report.
#[test]
fn logs_report_round_trips() {
    let value = LogReport {
        stdout_bytes: 2_621_440,
        stderr_bytes: 100,
        stdout_dropped: 0,
        stderr_dropped: 0,
    };
    assert_eq!(
        serde_json::to_value(value).unwrap(),
        json!({"stdout_bytes": 2621440, "stderr_bytes": 100, "stdout_dropped": 0, "stderr_dropped": 0})
    );
    round_trip(&value);
    let result = result_with(Some(value));
    round_trip(&result);
    let beat = Heartbeat {
        jobs: vec![HeldJob {
            job_id: "build-42".into(),
            state: HeldState::ExitedUnacked,
            result: Some(result),
        }],
        ..heartbeat()
    };
    let back: Heartbeat = serde_json::from_value(serde_json::to_value(&beat).unwrap()).unwrap();
    assert_eq!(back, beat);
    assert_eq!(
        back.jobs[0].result.as_ref().unwrap().logs_report(),
        Some(&value)
    );
}

/// Asserts that every log field that does not decode (a wrong type, a missing member, a
/// `null`, a negative count) reads as absent, and that the message around it decodes and
/// validates as before: the start is still a job start, the beat keeps its jobs, the reply
/// keeps its commands. Catches a log field whose decode failure costs the job start (it
/// would read as an unknown command and never run) or the whole heartbeat or reply.
#[test]
fn a_malformed_log_field_reads_as_absent() {
    for sink in [
        json!("nope"),
        json!({"endpoint": "https://store.example"}),
        Value::Null,
    ] {
        let mut wire = serde_json::to_value(Command::JobStart(Box::new(start()))).unwrap();
        wire["log_sink"] = sink;
        let reply: HeartbeatReply = serde_json::from_value(json!({"commands": [wire]})).unwrap();
        let Command::JobStart(spec) = &reply.commands[0] else {
            panic!("the job start is lost: {:?}", reply.commands);
        };
        assert_eq!(spec.log_sink, None);
        assert_eq!(spec.validate(), Ok(()));
    }

    for state in [json!(5), json!({"stale": "yes"}), json!({"until_ms": -1})] {
        let mut wire = serde_json::to_value(heartbeat()).unwrap();
        wire["log_grant_state"] = state;
        let beat: Heartbeat = serde_json::from_value(wire).unwrap();
        assert_eq!(beat.log_grant_state, None);
        assert_eq!(beat, heartbeat());
    }

    let grants = [
        json!({"access_key_id": 1}),
        json!({"access_key_id": "A", "secret_access_key": "s"}),
        json!([]),
    ];
    for grant in grants {
        let wire = json!({"commands": [{"type": "stop", "run_id": "r1"}], "log_grant": grant});
        let reply: HeartbeatReply = serde_json::from_value(wire).unwrap();
        assert_eq!(reply.log_grant, None);
        assert_eq!(reply.validate().commands, [&stop("r1")]);
    }

    let reports = [
        json!({"stdout_bytes": -1, "stderr_bytes": 0, "stdout_dropped": 0, "stderr_dropped": 0}),
        json!({"stdout_bytes": 1}),
        json!("all of it"),
    ];
    for logs in reports {
        let mut wire = serde_json::to_value(result_with(None)).unwrap();
        wire["logs"] = logs;
        let result: JobResult = serde_json::from_value(wire).unwrap();
        assert_eq!(result, result_with(None));
    }
}

/// Asserts the whole `Debug` text of a grant, by exact equality, with and without a token
/// (the key id cut at four characters, the secret and the token `***`); that a reply that
/// holds the grant prints none of the secret, the token or a fifth character of the key id;
/// that a key id of fewer than four characters, or of characters wider than a byte, is cut
/// by character; and that a start with a sink has no credential member at all. Catches a
/// derived `Debug` that would print the secret into a log line or a panic message.
#[test]
fn no_secret_is_printed() {
    assert_eq!(
        format!("{:?}", grant()),
        "LogGrant { access_key_id: \"test..\", secret_access_key: ***, \
         session_token: Some(***), expires_at_ms: 1800000000000 }"
    );
    let tokenless = LogGrant {
        session_token: None,
        ..grant()
    };
    assert_eq!(
        format!("{tokenless:?}"),
        "LogGrant { access_key_id: \"test..\", secret_access_key: ***, \
         session_token: None, expires_at_ms: 1800000000000 }"
    );
    let wide = LogGrant {
        access_key_id: "ÄÖÜßXYZ".into(),
        ..grant()
    };
    assert!(format!("{wide:?}").contains("access_key_id: \"ÄÖÜß..\""));
    let short = LogGrant {
        access_key_id: "AK".into(),
        ..grant()
    };
    assert!(format!("{short:?}").contains("access_key_id: \"AK..\""));

    let reply = HeartbeatReply {
        log_grant: Some(grant()),
        ..HeartbeatReply::default()
    };
    let text = format!("{reply:?} {reply:#?}");
    let secret = grant().secret_access_key;
    let token = grant().session_token.unwrap();
    for hidden in [secret.as_str(), token.as_str(), "testk", "test-s", "test-t"] {
        assert!(!text.contains(hidden), "{hidden} in {text}");
    }

    let wire = serde_json::to_value(start()).unwrap();
    let mut members: Vec<&str> = wire["log_sink"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    members.sort_unstable();
    assert_eq!(
        members,
        ["bucket", "endpoint", "path_style", "prefix", "region"]
    );
    let debug = format!("{:?}", start());
    for word in ["secret", "token", "access_key"] {
        assert!(!debug.contains(word), "{word} in {debug}");
    }
}
