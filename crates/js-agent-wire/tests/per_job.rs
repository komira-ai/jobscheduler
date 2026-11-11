//! The messages keyed by job id: their wire form, and every rule `validate` holds them to.

mod common;

use common::{broken, expect, heartbeat, job, long, map, refused};
use js_agent_wire::limits::{
    MAX_DETAIL_LEN, MAX_GPUS, MAX_HELD, MAX_ID_LEN, MAX_LOG_STREAM_LEN, MAX_RESULTS, MAX_WALL_MS,
};
use js_agent_wire::{
    Ack, BookedResources, Command, Heartbeat, HeartbeatReply, HeldJob, HeldState, JobResult,
    JobResultAck, JobStartSpec, LogTail, Outcome, PER_JOB_CAPABILITY, Rule, RunFile,
    ValidationError, refusal,
};
use serde_json::{Value, json};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A job start with every field set.
fn start() -> JobStartSpec {
    JobStartSpec {
        job_id: "job-1".into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        image: format!("ubuntu:24.04@{DIGEST}"),
        command: vec!["make".into(), "all".into()],
        run: None,
        os: None,
        arch: None,
        env: map(&[("LANG", "C")]),
        files: vec![RunFile {
            name: "grant".into(),
            contents: "https://store.example/put?sig=x".into(),
        }],
        booked: BookedResources {
            cpu_millis: 2000,
            memory_bytes: 1 << 30,
            disk_bytes: Some(4 << 30),
            gpu_indices: vec![0],
        },
        wall_ms: 60_000,
        network: Default::default(),
        ports: Vec::new(),
        log_sink: None,
        kind: Default::default(),
        isolation: Default::default(),
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
    }
}

fn job_result() -> JobResult {
    JobResult {
        job_id: "job-1".into(),
        allocation_id: "0123456789abcdef0123456789abcdef".into(),
        outcome: Outcome::Failed,
        exit_code: Some(2),
        detail: Some("make failed".into()),
        log_tail: Some(LogTail {
            stdout: "out".into(),
            stderr: "err".into(),
            truncated: false,
        }),
        logs: None,
        input_failure: None,
    }
}

fn held_job() -> HeldJob {
    HeldJob {
        job_id: "job-1".into(),
        state: HeldState::ExitedUnacked,
        result: Some(job_result()),
    }
}

/// A heartbeat that also lists a job and a retired job id.
fn beat() -> Heartbeat {
    Heartbeat {
        jobs: vec![held_job()],
        retired_jobs: vec!["job-0".into()],
        ..heartbeat()
    }
}

fn validate_start(spec: &JobStartSpec) -> Result<(), ValidationError> {
    Command::JobStart(Box::new(spec.clone())).validate()
}

/// Asserts the literal JSON of a job start, a job stop, a heartbeat with `jobs` and
/// `retired_jobs`, and a reply with `job_acks` and `retired_acks`, and that each decodes
/// back to the value. It catches a renamed field or tag, a lost `snake_case`, and an empty
/// list or absent option that is sent anyway.
#[test]
fn job_messages_have_the_documented_wire_form() {
    let command = Command::JobStart(Box::new(start()));
    let expected = json!({
        "type": "job_start",
        "job_id": "job-1",
        "allocation_id": "0123456789abcdef0123456789abcdef",
        "image": format!("ubuntu:24.04@{DIGEST}"),
        "command": ["make", "all"],
        "env": {"LANG": "C"},
        "files": [{"name": "grant", "contents": "https://store.example/put?sig=x"}],
        "booked": {
            "cpu_millis": 2000,
            "memory_bytes": 1073741824u64,
            "disk_bytes": 4294967296u64,
            "gpu_indices": [0]
        },
        "wall_ms": 60000
    });
    assert_eq!(serde_json::to_value(&command).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<Command>(expected).unwrap(),
        command
    );

    let mut bare = start();
    bare.command.clear();
    bare.env.clear();
    bare.files.clear();
    bare.booked.disk_bytes = None;
    bare.booked.gpu_indices.clear();
    assert_eq!(
        serde_json::to_value(Command::JobStart(Box::new(bare))).unwrap(),
        json!({
            "type": "job_start",
            "job_id": "job-1",
            "allocation_id": "0123456789abcdef0123456789abcdef",
            "image": format!("ubuntu:24.04@{DIGEST}"),
            "booked": {"cpu_millis": 2000, "memory_bytes": 1073741824u64},
            "wall_ms": 60000
        })
    );

    let stop = Command::JobStop {
        job_id: "job-1".into(),
    };
    assert_eq!(
        serde_json::to_value(&stop).unwrap(),
        json!({"type": "job_stop", "job_id": "job-1"})
    );

    let beat = Heartbeat {
        held: Vec::new(),
        results: Vec::new(),
        taint: None,
        ..beat()
    };
    assert_eq!(
        serde_json::to_value(&beat).unwrap(),
        json!({
            "host_id": "mac-01",
            "epoch": 3,
            "session": "s3cret-0",
            "phase": "draining",
            "held": [],
            "results": [],
            "jobs": [{
                "job_id": "job-1",
                "state": "exited_unacked",
                "result": {
                    "job_id": "job-1",
                    "allocation_id": "0123456789abcdef0123456789abcdef",
                    "outcome": "failed",
                    "exit_code": 2,
                    "detail": "make failed",
                    "log_tail": {"stdout": "out", "stderr": "err", "truncated": false}
                }
            }],
            "retired_jobs": ["job-0"]
        })
    );
    let running = HeldJob {
        job_id: "job-2".into(),
        state: HeldState::Running,
        result: None,
    };
    assert_eq!(
        serde_json::to_value(&running).unwrap(),
        json!({"job_id": "job-2", "state": "running"})
    );

    let reply = HeartbeatReply {
        job_acks: vec![JobResultAck {
            job_id: "job-1".into(),
            ack: Ack::Accepted,
        }],
        retired_acks: vec!["job-0".into()],
        ..HeartbeatReply::default()
    };
    let wire = json!({
        "commands": [],
        "result_acks": [],
        "taint_ack": false,
        "job_acks": [{"job_id": "job-1", "ack": "ACCEPTED"}],
        "retired_acks": ["job-0"]
    });
    assert_eq!(serde_json::to_value(&reply).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<HeartbeatReply>(wire).unwrap(),
        reply
    );
    assert_eq!(PER_JOB_CAPABILITY, "per-job");
}

/// Asserts that a job start that names no image, whether the key is absent or empty, is
/// refused `invalid-job` on the field `image`, and that a malformed one is `invalid-image`.
/// It catches a job start an agent would try to run with no image, or one that fails to
/// decode (and so is silently skipped) instead of being refused.
#[test]
fn a_job_start_with_no_image_is_refused() {
    let mut wire = serde_json::to_value(Command::JobStart(Box::new(start()))).unwrap();
    wire.as_object_mut().unwrap().remove("image");
    let absent: Command = serde_json::from_value(wire).unwrap();
    let Command::JobStart(spec) = &absent else {
        panic!("a job start without an image still decodes: {absent:?}");
    };
    assert_eq!(spec.image, "");
    expect(
        absent.validate().unwrap_err(),
        "image",
        Rule::Empty,
        refusal::INVALID_JOB,
    );
    let err = broken(&start(), |s| s.image.clear(), validate_start);
    expect(err, "image", Rule::Empty, refusal::INVALID_JOB);
    let err = broken(&start(), |s| s.image = "oci:ubuntu".into(), validate_start);
    assert_eq!((err.field(), err.code()), ("image", refusal::INVALID_IMAGE));
}

/// Asserts that a wall limit of zero is refused `wall-time-not-positive` on `wall_ms`, that
/// one above the cap is `invalid-job`, and that the cap itself is accepted. It catches a job
/// start that would run unbounded, or one the agent kills at once.
#[test]
fn a_job_start_with_zero_wall_ms_is_refused() {
    let err = broken(&start(), |s| s.wall_ms = 0, validate_start);
    expect(
        err,
        "wall_ms",
        Rule::OutOfRange,
        refusal::WALL_TIME_NOT_POSITIVE,
    );
    let err = broken(&start(), |s| s.wall_ms = MAX_WALL_MS + 1, validate_start);
    expect(err, "wall_ms", Rule::OutOfRange, refusal::INVALID_JOB);
    let mut cap = start();
    cap.wall_ms = MAX_WALL_MS;
    assert_eq!(validate_start(&cap), Ok(()));
}

/// Asserts that a job start whose image is not pinned by a digest is refused
/// `image-digest-required` on `image` by the command's check (the spec's own `validate`
/// passes it, as for a run start), and that a pinned image with a tag passes. It catches a
/// host that pulls an image whose content can move.
#[test]
fn a_job_start_with_an_unpinned_image_is_refused() {
    let mut spec = start();
    spec.image = "ubuntu:24.04".into();
    assert_eq!(spec.validate(), Ok(()));
    expect(
        validate_start(&spec).unwrap_err(),
        "image",
        Rule::NotPinned,
        refusal::IMAGE_DIGEST_REQUIRED,
    );
    assert_eq!(spec.require_pinned().unwrap_err().field(), "image");
    assert_eq!(validate_start(&start()), Ok(()));
}

/// Asserts each rule of the rest of a job start: the ids, the command, the environment,
/// the files and the booked resources (each bound, and an absent disk). It catches a field
/// a job start carries that is not held to the rules its run-start counterpart is.
#[test]
fn every_field_of_a_job_start_is_checked() {
    let check = |edit: fn(&mut JobStartSpec), field: &str, rule: Rule, code: &str| {
        expect(broken(&start(), edit, validate_start), field, rule, code);
    };
    let job = refusal::INVALID_JOB;
    check(|s| s.job_id.clear(), "job_id", Rule::Empty, job);
    check(
        |s| s.job_id = "a".repeat(MAX_ID_LEN + 1),
        "job_id",
        Rule::TooLong,
        job,
    );
    check(
        |s| s.allocation_id = "-a".into(),
        "allocation_id",
        Rule::BadChars,
        job,
    );
    check(
        |s| s.command = vec!["a\0".into()],
        "command",
        Rule::BadChars,
        job,
    );
    check(
        |s| s.files[0].contents = "a\0".into(),
        "files",
        Rule::BadChars,
        refusal::INVALID_FILE,
    );
    check(
        |s| s.booked.cpu_millis = 0,
        "booked.cpu_millis",
        Rule::OutOfRange,
        job,
    );
    check(
        |s| s.booked.memory_bytes = 0,
        "booked.memory_bytes",
        Rule::OutOfRange,
        job,
    );
    check(
        |s| s.booked.disk_bytes = Some(0),
        "booked.disk_bytes",
        Rule::OutOfRange,
        job,
    );
    check(
        |s| s.booked.gpu_indices = vec![1, 1],
        "booked.gpu_indices",
        Rule::Duplicate,
        job,
    );
    check(
        |s| s.booked.gpu_indices = vec![MAX_GPUS as u32],
        "booked.gpu_indices",
        Rule::OutOfRange,
        job,
    );
    check(
        |s| s.booked.gpu_indices = (0..=MAX_GPUS as u32).collect(),
        "booked.gpu_indices",
        Rule::TooMany,
        job,
    );
    let mut no_disk = start();
    no_disk.booked.disk_bytes = None;
    assert_eq!(validate_start(&no_disk), Ok(()));
    expect(
        Command::JobStop {
            job_id: "-x".into(),
        }
        .validate()
        .unwrap_err(),
        "job_id",
        Rule::BadChars,
        refusal::INVALID_REPLY,
    );
}

/// Asserts that a job listing over [`MAX_HELD`] entries is refused `invalid-heartbeat` on
/// `jobs`, and a list of retired job ids over it on `retired_jobs`, while lists at the cap
/// pass; and the same for a reply's `job_acks` (over [`MAX_RESULTS`]) and `retired_acks`,
/// which leave the commands beside them alone. It catches a list a peer could grow without
/// bound.
#[test]
fn an_overlong_job_listing_is_refused() {
    let jobs = |n: usize| -> Vec<HeldJob> {
        (0..n)
            .map(|i| HeldJob {
                job_id: format!("j{i}"),
                state: HeldState::Running,
                result: None,
            })
            .collect()
    };
    let ids = |n: usize| -> Vec<String> { (0..n).map(|i| format!("j{i}")).collect() };
    let at_cap = Heartbeat {
        jobs: jobs(MAX_HELD),
        retired_jobs: ids(MAX_HELD),
        ..heartbeat()
    };
    assert_eq!(at_cap.validate(), Ok(()));
    let over = Heartbeat {
        jobs: jobs(MAX_HELD + 1),
        ..heartbeat()
    };
    expect(
        over.validate().unwrap_err(),
        "jobs",
        Rule::TooMany,
        refusal::INVALID_HEARTBEAT,
    );
    let over = Heartbeat {
        retired_jobs: ids(MAX_HELD + 1),
        ..heartbeat()
    };
    expect(
        over.validate().unwrap_err(),
        "retired_jobs",
        Rule::TooMany,
        refusal::INVALID_HEARTBEAT,
    );

    let acks = |n: usize| -> Vec<JobResultAck> {
        (0..n)
            .map(|i| JobResultAck {
                job_id: format!("j{i}"),
                ack: Ack::Accepted,
            })
            .collect()
    };
    let stop = Command::JobStop {
        job_id: "j1".into(),
    };
    let reply = |job_acks, retired_acks| HeartbeatReply {
        commands: vec![stop.clone()],
        job_acks,
        retired_acks,
        ..HeartbeatReply::default()
    };
    let fine = reply(acks(MAX_RESULTS), ids(MAX_HELD));
    let checked = fine.validate();
    assert_eq!((checked.job_acks, checked.retired_acks), (Ok(()), Ok(())));
    let long = reply(acks(MAX_RESULTS + 1), ids(MAX_HELD + 1));
    let checked = long.validate();
    expect(
        checked.job_acks.unwrap_err(),
        "job_acks",
        Rule::TooMany,
        refusal::INVALID_REPLY,
    );
    expect(
        checked.retired_acks.unwrap_err(),
        "retired_acks",
        Rule::TooMany,
        refusal::INVALID_REPLY,
    );
    assert_eq!(checked.commands, [&stop]);
}

/// Asserts the rules of a job listing besides its length: a bad or repeated job id in
/// `jobs` or `retired_jobs`, and a result that names another job than the one it is listed
/// under. It catches a listing a server would file under the wrong job.
#[test]
fn a_job_listing_names_each_job_once_and_its_own_result() {
    let fails = |edit: fn(&mut Heartbeat), field: &str, rule: Rule| {
        let err = broken(&beat(), edit, Heartbeat::validate);
        expect(err, field, rule, refusal::INVALID_HEARTBEAT);
    };
    fails(
        |b| b.jobs[0].job_id = "-x".into(),
        "jobs.job_id",
        Rule::BadChars,
    );
    fails(|b| b.jobs.push(b.jobs[0].clone()), "jobs", Rule::Duplicate);
    fails(
        |b| b.jobs[0].result.as_mut().unwrap().job_id = "job-2".into(),
        "jobs.result.job_id",
        Rule::Inconsistent,
    );
    fails(
        |b| b.retired_jobs = vec!["".into()],
        "retired_jobs",
        Rule::Empty,
    );
    fails(
        |b| b.retired_jobs = vec!["a".into(), "a".into()],
        "retired_jobs",
        Rule::Duplicate,
    );
    let listed = held_job();
    assert_eq!(listed.validate(), Ok(()));
    let bare = HeldJob {
        result: None,
        ..held_job()
    };
    assert_eq!(bare.validate(), Ok(()));
}

/// Asserts the rules of a job's result, which are a run result's: its ids, the detail, each
/// log stream's length, and that the exit status agrees with the outcome. It catches a job
/// result held to weaker rules than the run result it stands beside.
#[test]
fn a_job_result_is_held_to_the_rules_of_a_run_result() {
    let fails = |edit: fn(&mut JobResult), field: &str, rule: Rule| {
        let err = broken(&job_result(), edit, JobResult::validate);
        expect(err, field, rule, refusal::INVALID_HEARTBEAT);
    };
    fails(|r| r.job_id.clear(), "jobs.result.job_id", Rule::Empty);
    fails(
        |r| r.allocation_id.clear(),
        "jobs.result.allocation_id",
        Rule::Empty,
    );
    fails(
        |r| r.detail = Some(long(MAX_DETAIL_LEN + 1)),
        "jobs.result.detail",
        Rule::TooLong,
    );
    fails(
        |r| r.log_tail.as_mut().unwrap().stdout = long(MAX_LOG_STREAM_LEN + 1),
        "jobs.result.log_tail.stdout",
        Rule::TooLong,
    );
    fails(
        |r| r.log_tail.as_mut().unwrap().stderr = long(MAX_LOG_STREAM_LEN + 1),
        "jobs.result.log_tail.stderr",
        Rule::TooLong,
    );
    fails(
        |r| {
            r.outcome = Outcome::Succeeded;
            r.exit_code = Some(1);
        },
        "jobs.result.exit_code",
        Rule::Inconsistent,
    );
    fails(
        |r| r.exit_code = Some(0),
        "jobs.result.exit_code",
        Rule::Inconsistent,
    );
    fails(
        |r| {
            r.outcome = Outcome::FailedToStart;
            r.exit_code = Some(0);
        },
        "jobs.result.exit_code",
        Rule::Inconsistent,
    );
    let at_cap = JobResult {
        outcome: Outcome::Succeeded,
        exit_code: Some(0),
        detail: None,
        log_tail: Some(LogTail {
            stdout: long(MAX_LOG_STREAM_LEN),
            stderr: long(MAX_LOG_STREAM_LEN),
            truncated: true,
        }),
        ..job_result()
    };
    assert_eq!(at_cap.validate(), Ok(()));
    let killed = JobResult {
        outcome: Outcome::Killed,
        exit_code: None,
        log_tail: None,
        ..job_result()
    };
    assert_eq!(killed.validate(), Ok(()));
}

/// Asserts that a result's input failure is held to the rules of its detail: present, it is
/// not empty, at most `MAX_DETAIL_LEN` bytes (the bound passes), and has no control
/// character; a code it does not list (a later agent's) passes, beside any outcome, the exit
/// status kept. It catches an input failure that could fill a heartbeat or carry a forged
/// log line, and a server that would refuse a result for a code it does not know yet.
#[test]
fn a_job_results_input_failure_is_bounded_as_its_detail() {
    let fails = |code: String, rule: Rule| {
        let err = broken(
            &job_result(),
            |r| r.input_failure = Some(code),
            JobResult::validate,
        );
        expect(
            err,
            "jobs.result.input_failure",
            rule,
            refusal::INVALID_HEARTBEAT,
        );
    };
    fails(String::new(), Rule::Empty);
    fails(long(MAX_DETAIL_LEN + 1), Rule::TooLong);
    fails("tree-read-failed\nforged".into(), Rule::BadChars);
    for code in [
        refusal::TREE_READ_FAILED.to_owned(),
        refusal::TREE_STALLED.to_owned(),
        refusal::TREE_UNAVAILABLE.to_owned(),
        refusal::INPUT_UNREADABLE.to_owned(),
        refusal::HOST_PORT_LOST.to_owned(),
        "tree-later".to_owned(),
        long(MAX_DETAIL_LEN),
    ] {
        let succeeded = JobResult {
            outcome: Outcome::Succeeded,
            exit_code: Some(0),
            input_failure: Some(code.clone()),
            ..job_result()
        };
        assert_eq!(succeeded.validate(), Ok(()), "{code}");
        let failed = JobResult {
            input_failure: Some(code.clone()),
            ..job_result()
        };
        assert_eq!(failed.validate(), Ok(()), "{code}");
    }
}

/// Asserts that a reply refuses a second job start of one job id, and a job start that
/// names a GPU a run start of the same reply already named (or the other way round), while
/// a run start and a job start of one id pass the reply's check (the agent tells them
/// apart); and that bad `job_acks` and `retired_acks` are refused without costing a command.
/// It catches two starts of one job in one reply, and one device given to two jobs.
#[test]
fn a_reply_names_each_job_and_each_gpu_once() {
    let run = |id: &str, gpus: &[u32]| {
        let mut spec = job();
        spec.run_id = id.into();
        spec.gpu_indices = gpus.to_vec();
        Command::Start(Box::new(spec))
    };
    let start_of = |id: &str, gpus: &[u32]| {
        let mut spec = start();
        spec.job_id = id.into();
        spec.booked.gpu_indices = gpus.to_vec();
        Command::JobStart(Box::new(spec))
    };
    let refused_at = |commands: Vec<Command>| -> Vec<(usize, &'static str)> {
        let reply = HeartbeatReply {
            commands,
            ..HeartbeatReply::default()
        };
        reply
            .validate()
            .refused
            .iter()
            .map(|refused| (refused.index, refused.error.code()))
            .collect()
    };
    let none: Vec<(usize, &str)> = Vec::new();
    assert_eq!(
        refused_at(vec![start_of("j1", &[0]), start_of("j1", &[1])]),
        [(1, refusal::INVALID_REPLY)]
    );
    assert_eq!(
        refused_at(vec![run("r1", &[0]), start_of("j1", &[0])]),
        [(1, refusal::INVALID_REPLY)]
    );
    assert_eq!(
        refused_at(vec![start_of("j1", &[1]), run("r1", &[1])]),
        [(1, refusal::INVALID_REPLY)]
    );
    assert_eq!(refused_at(vec![run("x", &[0]), start_of("x", &[1])]), none);

    let reply = HeartbeatReply {
        commands: vec![start_of("j1", &[])],
        job_acks: vec![
            JobResultAck {
                job_id: "j1".into(),
                ack: Ack::Accepted,
            },
            JobResultAck {
                job_id: "j1".into(),
                ack: Ack::Stale,
            },
        ],
        retired_acks: vec!["-x".into()],
        ..HeartbeatReply::default()
    };
    let checked = reply.validate();
    expect(
        checked.job_acks.unwrap_err(),
        "job_acks",
        Rule::Duplicate,
        refusal::INVALID_REPLY,
    );
    expect(
        checked.retired_acks.unwrap_err(),
        "retired_acks",
        Rule::BadChars,
        refusal::INVALID_REPLY,
    );
    assert_eq!(checked.commands.len(), 1);
    let bad_id = HeartbeatReply {
        job_acks: vec![JobResultAck {
            job_id: "".into(),
            ack: Ack::Accepted,
        }],
        ..HeartbeatReply::default()
    };
    let err = refused(bad_id.validate().job_acks, refusal::INVALID_REPLY);
    assert_eq!(err.rule(), Rule::Empty);
}

/// Asserts that a job start or a job listing a newer peer extends with a field this
/// version does not know still decodes, and that a job start of a shape this version cannot
/// read is one unknown command that costs the reply nothing else. It catches a per-job
/// message that breaks the additive rule within the major.
#[test]
fn per_job_messages_ignore_what_they_do_not_know() {
    let mut command = serde_json::to_value(Command::JobStart(Box::new(start()))).unwrap();
    command["sandbox"] = json!({"mode": "from-the-future"});
    command["booked"]["accelerators"] = json!(4);
    let decoded: Command = serde_json::from_value(command).unwrap();
    assert_eq!(decoded, Command::JobStart(Box::new(start())));

    let mut body = serde_json::to_value(beat()).unwrap();
    body["jobs"][0]["field_from_the_future"] = json!(1);
    body["jobs"][0]["state"] = json!("hibernating");
    let decoded: Heartbeat = serde_json::from_value(body).unwrap();
    assert_eq!(decoded.jobs[0].state, HeldState::Unknown);

    let broken_start: Value = json!({"type": "job_start", "job_id": "j1"});
    let reply: HeartbeatReply = serde_json::from_value(json!({
        "commands": [broken_start, {"type": "job_stop", "job_id": "j2"}]
    }))
    .unwrap();
    assert_eq!(
        reply.commands,
        [
            Command::Unknown,
            Command::JobStop {
                job_id: "j2".into()
            }
        ]
    );
}

/// Asserts that a job start's environment is held to the rules of a native job's: a
/// reserved name, a malformed name, a value with a NUL and one entry too many are refused
/// `invalid-env` on `env`, and an empty environment passes. It catches an environment that
/// could set the agent's own variables or the loader's in a job.
#[test]
fn a_job_start_with_a_reserved_or_malformed_env_name_is_refused() {
    let refused_env = |pairs: &[(&str, &str)], rule: Rule| {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let err = broken(&start(), |s| s.env.extend(pairs), validate_start);
        expect(err, "env", rule, refusal::INVALID_ENV);
    };
    refused_env(&[("JS_SECRET", "x")], Rule::Reserved);
    refused_env(&[("LD_PRELOAD", "/x.so")], Rule::Reserved);
    refused_env(&[("1A", "x")], Rule::BadChars);
    refused_env(&[("A-B", "x")], Rule::BadChars);
    refused_env(&[("", "x")], Rule::Empty);
    refused_env(&[("A", "x\0y")], Rule::BadChars);
    let many: Vec<String> = (0..js_agent_wire::limits::MAX_ENV)
        .map(|n| format!("V{n}"))
        .collect();
    let many: Vec<(&str, &str)> = many.iter().map(|k| (k.as_str(), "x")).collect();
    refused_env(&many, Rule::TooMany);
    let mut none = start();
    none.env.clear();
    assert_eq!(validate_start(&none), Ok(()));
}

/// A run body for a job start: a program with an argument, nothing else.
fn run_body() -> js_agent_wire::RunSpec {
    js_agent_wire::RunSpec {
        program: "/usr/bin/make".into(),
        args: vec!["all".into()],
        env: Default::default(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: js_agent_wire::NetworkLevel::Off,
    }
}

/// Asserts the `run`, `os` and `arch` of a job start, the fields of the one job definition:
/// with an empty `command` a valid run passes and is sent as `run` with `os` and `arch`
/// beside it; a run beside a `command` is refused `invalid-job` naming `command`, and a bad
/// run body is refused on its own field; absent, none of the three is sent. It catches a job
/// start that carries two command lines, and a run body held to weaker rules than a run
/// start's.
#[test]
fn a_job_start_carries_the_run_os_and_arch_of_the_job() {
    let mut spec = start();
    spec.command.clear();
    spec.run = Some(run_body());
    spec.os = Some(js_types::Os::Linux);
    spec.arch = Some(js_types::Arch::Arm64);
    assert_eq!(validate_start(&spec), Ok(()));
    let wire = serde_json::to_value(Command::JobStart(Box::new(spec.clone()))).unwrap();
    assert_eq!(wire["run"]["program"], json!("/usr/bin/make"));
    assert_eq!(wire["os"], json!("linux"));
    assert_eq!(wire["arch"], json!("arm64"));
    assert!(wire.get("command").is_none());
    assert_eq!(
        serde_json::from_value::<Command>(wire).unwrap(),
        Command::JobStart(Box::new(spec.clone()))
    );

    let both = broken(&spec, |s| s.command = vec!["x".into()], validate_start);
    expect(both, "command", Rule::Inconsistent, refusal::INVALID_JOB);
    let bad = broken(
        &spec,
        |s| s.run.as_mut().unwrap().program.clear(),
        validate_start,
    );
    assert_eq!(
        (bad.field(), bad.code()),
        ("run.program", refusal::INVALID_PROGRAM)
    );

    let plain = serde_json::to_value(Command::JobStart(Box::new(start()))).unwrap();
    for key in ["run", "os", "arch"] {
        assert!(plain.get(key).is_none(), "{key} is sent when absent");
    }
}

/// Proves a job start carries `network` and `ports` as a run start does: a start that asks
/// for nothing encodes with neither key (the text of a start from before the fields), one
/// that asks for `open` with a port encodes and decodes both, a level this version does not
/// know is a decode error, and the ports are held to the run start's bounds naming the field.
/// Catches a field that is always written or required, a level that fails open, and a job
/// start whose ports skip the bounds of a run start's.
#[test]
fn a_job_start_carries_network_and_ports_additively() {
    use js_types::{JobNetwork, PortMap};

    let plain = serde_json::to_value(start()).unwrap();
    assert!(plain.get("network").is_none(), "{plain}");
    assert!(plain.get("ports").is_none(), "{plain}");
    let back: JobStartSpec = serde_json::from_value(plain.clone()).unwrap();
    assert_eq!((back.network, back.ports.len()), (JobNetwork::None, 0));

    let mut open = start();
    open.network = JobNetwork::Open;
    open.ports = vec![PortMap {
        host: 8080,
        container: 80,
    }];
    assert_eq!(open.validate(), Ok(()));
    let value = serde_json::to_value(&open).unwrap();
    assert_eq!(value["network"], "open");
    assert_eq!(
        value["ports"],
        serde_json::json!([{"host": 8080, "container": 80}])
    );
    let back: JobStartSpec = serde_json::from_value(value).unwrap();
    assert_eq!(back, open);

    let mut unknown = plain;
    unknown["network"] = serde_json::json!("wide");
    assert!(serde_json::from_value::<JobStartSpec>(unknown).is_err());

    let mut zero = start();
    zero.ports = vec![PortMap {
        host: 0,
        container: 80,
    }];
    let err = zero.validate().unwrap_err();
    assert_eq!(
        (err.field(), err.rule(), err.code()),
        (
            "ports[0].host",
            js_agent_wire::Rule::OutOfRange,
            "invalid-job"
        )
    );
    let mut many = start();
    many.ports = (1..=17)
        .map(|n| PortMap {
            host: 8000 + n,
            container: 80,
        })
        .collect();
    assert_eq!(many.validate().unwrap_err().field(), "ports");
}
