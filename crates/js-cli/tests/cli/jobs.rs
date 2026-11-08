//! The job commands of `js`, built from the generated table, against the in-memory job
//! server of `fake_jobs` (no port, no clock): every command and its flags, the requests
//! they send, the drawn job id, the exit statuses of each kind of failure, and the text and
//! JSON of the answers.

use std::ffi::OsString;
use std::sync::Arc;

use js_cli::jobs::{Kind, surface};
use js_client::jobs::Dialer;
use js_proto::v1::{
    GetJobResponse, GetLogsResponse, GetUsageRequest, Job, JobSpec, JobState, JobTimes,
    ListJobsRequest, Needs, PortForward, RefusalCode, SubmitJobRequest,
};
use prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tonic::Code;

use crate::fake_jobs::{Answer, Fake, refusing};
use crate::support::{Never, Outcome, Scratch, TOKEN, run, services};

/// A loopback origin over plain http; the in-memory dialer never dials it.
const PLAIN: &str = "http://localhost:7070";

/// The path of a call of the job service.
fn call(rpc: &str) -> String {
    format!("/jobscheduler.v1.JobService/{rpc}")
}

/// Runs `js --server PLAIN --token-file <token> <words>` through `dialer`.
pub(crate) fn js(scratch: &Scratch, dialer: Dialer, words: &[&str]) -> Outcome {
    let token = scratch.write("token", TOKEN, 0o600);
    let mut args: Vec<OsString> = vec![
        "js".into(),
        "--server".into(),
        PLAIN.into(),
        "--token-file".into(),
    ];
    args.push(token.into());
    args.extend(words.iter().map(OsString::from));
    run(args, services(Arc::new(Never)).with_dialer(dialer))
}

/// The one request `fake` read, decoded as `T`, and its call.
fn only<T: prost::Message + Default>(fake: &Fake) -> (String, T) {
    let seen = fake.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    (
        seen[0].path.clone(),
        T::decode(seen[0].message.as_slice()).unwrap(),
    )
}

/// Whether the field at `path` is set in `message`, through nested messages.
fn set(message: &DynamicMessage, path: &[&str]) -> bool {
    match path.split_first() {
        Some((last, [])) => message.has_field_by_name(last),
        Some((first, rest)) => message
            .get_field_by_name(first)
            .and_then(|value| value.as_message().cloned())
            .is_some_and(|inner| set(&inner, rest)),
        None => false,
    }
}

/// A sample value for a flag of `kind`, never the field's default.
fn sample(kind: &Kind, scratch: &Scratch) -> String {
    match kind {
        Kind::Bool => "true".to_owned(),
        Kind::I32 | Kind::I64 | Kind::U32 | Kind::U64 => "7".to_owned(),
        Kind::F32 | Kind::F64 => "1.5".to_owned(),
        Kind::Text => "x".to_owned(),
        Kind::Seconds => "2m".to_owned(),
        Kind::Enum(choices) => choices[choices.len() - 1].0.to_owned(),
        Kind::Pair(..) => "k=v".to_owned(),
        Kind::File => scratch
            .write("list.json", "[]", 0o600)
            .display()
            .to_string(),
    }
}

/// Asserts that every command of the table, given a value for each of its flags, exits 0
/// having sent one request of its own call; and that the requests of `job submit`,
/// `job list` and `usage` are what their flags say: repeated, map, dotted, duration and
/// enum flags, and a `-file` flag. It catches a flag that is parsed and dropped, a value put
/// in the wrong field, and a command whose call the dispatcher does not reach.
#[test]
fn every_job_command_parses_its_flags() {
    let scratch = Scratch::new("jobs_every");
    let pool = DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET).unwrap();
    for command in surface() {
        let fake = Fake::new();
        let mut words: Vec<String> = command.path.iter().map(|word| (*word).to_owned()).collect();
        for flag in command.flags {
            words.push(format!("--{}", flag.name));
            words.push(sample(&flag.kind, &scratch));
        }
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        let got = js(&scratch, fake.dialer(), &words);
        assert_eq!((got.code, got.err.as_str()), (0, ""), "{words:?}");
        assert_eq!(fake.seen().len(), 1, "{words:?}");
        assert_eq!(fake.seen()[0].path, call(command.rpc), "{words:?}");
        let request = pool.get_message_by_name(command.request).unwrap();
        let sent = DynamicMessage::decode(request, fake.seen()[0].message.as_slice()).unwrap();
        for flag in command
            .flags
            .iter()
            .filter(|flag| !matches!(flag.kind, Kind::File))
        {
            assert!(
                set(&sent, flag.field),
                "{words:?}: {} is not in the request",
                flag.name
            );
        }
    }

    let fake = Fake::new();
    let ports = scratch.write(
        "ports.json",
        r#"[{"hostPort": 8080, "containerPort": 80}]"#,
        0o600,
    );
    let got = js(
        &scratch,
        fake.dialer(),
        &[
            "job",
            "submit",
            "--id",
            "nightly-42",
            "--spec.image",
            "img@sha256:00",
            "--spec.args",
            "--fast",
            "--spec.args",
            "input",
            "--spec.env",
            "A=1",
            "--spec.env",
            "B=x=y",
            "--spec.needs.cpu-millis",
            "1500",
            "--spec.needs.whole-host",
            "true",
            "--spec.constraints.exec",
            "sandbox-exec",
            "--spec.wall-limit-seconds",
            "15m",
            "--spec.network",
            "allow-list",
            "--spec.ports-file",
            ports.to_str().unwrap(),
            "--spec.priority",
            "-3",
        ],
    );
    assert_eq!((got.code, got.err.as_str()), (0, ""));
    let (path, request) = only::<SubmitJobRequest>(&fake);
    assert_eq!(path, call("SubmitJob"));
    let want = SubmitJobRequest {
        spec: Some(JobSpec {
            job_id: "nightly-42".to_owned(),
            image: "img@sha256:00".to_owned(),
            args: vec!["--fast".to_owned(), "input".to_owned()],
            env: [("A", "1"), ("B", "x=y")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            needs: Some(Needs {
                cpu_millis: 1500,
                whole_host: true,
                ..Needs::default()
            }),
            constraints: Some(js_proto::v1::Constraints {
                exec: js_proto::v1::ExecMode::SandboxExec.into(),
                ..Default::default()
            }),
            wall_limit_seconds: Some(900),
            network: js_proto::v1::NetworkLevel::AllowList.into(),
            ports: vec![PortForward {
                host_port: 8080,
                container_port: 80,
            }],
            priority: -3,
            ..JobSpec::default()
        }),
    };
    assert_eq!(request, want);

    let fake = Fake::new();
    let got = js(
        &scratch,
        fake.dialer(),
        &[
            "job",
            "list",
            "--states",
            "running",
            "--states",
            "failed-to-start",
            "--labels",
            "team=core",
            "--queued-after-ms",
            "5",
        ],
    );
    assert_eq!((got.code, got.err.as_str()), (0, ""));
    let (_, request) = only::<ListJobsRequest>(&fake);
    assert_eq!(
        (
            request.states,
            request.labels.get("team").cloned(),
            request.queued_after_ms
        ),
        (
            vec![JobState::Running.into(), JobState::FailedToStart.into()],
            Some("core".to_owned()),
            Some(5)
        )
    );

    let fake = Fake::new();
    let got = js(
        &scratch,
        fake.dialer(),
        &["usage", "--host-id", "h-1", "--page-size", "10"],
    );
    assert_eq!((got.code, got.err.as_str()), (0, ""));
    let (path, request) = only::<GetUsageRequest>(&fake);
    assert_eq!(
        (path, request.host_id, request.page_size),
        (call("GetUsage"), "h-1".to_owned(), 10)
    );
}

/// Asserts that `job get` without its required `--job-id` is a usage error, exit 2, that
/// names the flag, and sends nothing.
#[test]
fn a_missing_required_flag_is_usage_exit_2() {
    let scratch = Scratch::new("jobs_required");
    let fake = Fake::new();
    let got = js(&scratch, fake.dialer(), &["job", "get"]);
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(got.err.contains("--job-id <TEXT>"), "{}", got.err);
    assert!(fake.seen().is_empty());
}

/// Asserts that an enum value that is not one of the enum's short names is exit 2 naming
/// the values there are, and that a value of the wrong type (a word for a number) is too.
#[test]
fn an_unknown_enum_name_is_usage_exit_2() {
    let scratch = Scratch::new("jobs_enum");
    let fake = Fake::new();
    let got = js(
        &scratch,
        fake.dialer(),
        &["job", "list", "--states", "sleeping"],
    );
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(
        got.err
            .contains("[possible values: unspecified, queued, placed, starting, running"),
        "{}",
        got.err
    );
    let got = js(
        &scratch,
        fake.dialer(),
        &["job", "list", "--page-size", "many"],
    );
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(
        got.err
            .contains("\"many\" is not a whole number below 2^32"),
        "{}",
        got.err
    );
    assert!(fake.seen().is_empty());
}

/// Asserts that a duration that is not one, zero, or one too long for its field (a cordon
/// is 32 bits of seconds) is exit 2 naming the flag.
#[test]
fn a_bad_duration_is_usage_exit_2() {
    let scratch = Scratch::new("jobs_duration");
    let fake = Fake::new();
    for value in ["soon", "0", "5x"] {
        let got = js(
            &scratch,
            fake.dialer(),
            &["host", "cordon", "--host-id", "h-1", "--cordon-secs", value],
        );
        assert_eq!(got.code, 2, "{value}: {}", got.err);
        assert!(
            got.err
                .contains("is not a positive duration such as 90, 90s, 15m or 2h"),
            "{}",
            got.err
        );
    }
    let got = js(
        &scratch,
        fake.dialer(),
        &[
            "host",
            "cordon",
            "--host-id",
            "h-1",
            "--cordon-secs",
            "9999999h",
        ],
    );
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(got.err.starts_with("js: --cordon-secs: "), "{}", got.err);
    assert!(fake.seen().is_empty());
}

/// Asserts that a `-file` flag whose file is missing, is not JSON, or holds the wrong
/// shape for its field is exit 2 naming the flag, before anything is sent.
#[test]
fn a_bad_file_flag_is_usage_exit_2() {
    let scratch = Scratch::new("jobs_file");
    let fake = Fake::new();
    let missing = scratch.path("missing.json");
    let junk = scratch.write("junk.json", "{", 0o600);
    let shape = scratch.write("shape.json", r#"{"hostPort": 1}"#, 0o600);
    for path in [&missing, &junk, &shape] {
        let got = js(
            &scratch,
            fake.dialer(),
            &["job", "submit", "--spec.ports-file", path.to_str().unwrap()],
        );
        assert_eq!(got.code, 2, "{}", got.err);
        assert!(
            got.err.starts_with("js: --spec.ports-file: "),
            "{}",
            got.err
        );
    }
    assert!(fake.seen().is_empty());
}

/// The id `js job submit` printed on stderr: the text after `js: job id ` on the first line.
fn printed_id(err: &str) -> String {
    let first = err.lines().next().unwrap_or_default();
    first
        .strip_prefix("js: job id ")
        .unwrap_or_default()
        .to_owned()
}

/// Asserts that `job submit` without `--id` draws one, 32 hexadecimal characters, prints
/// it on stderr, and sends it as the job's id, on each of two runs.
#[test]
fn job_submit_draws_an_id_and_prints_it_before_sending() {
    let scratch = Scratch::new("jobs_draw");
    for _ in 0..2 {
        let fake = Fake::new();
        let got = js(
            &scratch,
            fake.dialer(),
            &["job", "submit", "--spec.image", "img"],
        );
        assert_eq!(got.code, 0, "{}", got.err);
        let id = printed_id(&got.err);
        assert_eq!(got.err, format!("js: job id {id}\n"));
        assert!(
            id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "{id}"
        );
        let (_, request) = only::<SubmitJobRequest>(&fake);
        assert_eq!(request.spec.unwrap().job_id, id);
    }
}

/// Asserts that `job submit --id` sends that id and prints nothing.
#[test]
fn job_submit_with_id_sends_that_id() {
    let scratch = Scratch::new("jobs_given");
    let fake = Fake::new();
    let got = js(
        &scratch,
        fake.dialer(),
        &["job", "submit", "--id", "mine-1"],
    );
    assert_eq!((got.code, got.err.as_str()), (0, ""));
    let (_, request) = only::<SubmitJobRequest>(&fake);
    assert_eq!(request.spec.unwrap().job_id, "mine-1");
}

/// Asserts that a submit whose request the server read and whose connection then dropped
/// exits 9 naming the id and the command to run again, as text and as JSON
/// (`unconfirmed`), the drawn id printed first.
#[test]
fn a_lost_submit_reply_exits_9_and_names_the_id() {
    let scratch = Scratch::new("jobs_lost");
    let fake = Fake::new();
    fake.answer("SubmitJob", vec![Answer::Hang]);
    let got = js(
        &scratch,
        fake.dialer(),
        &["job", "submit", "--spec.image", "img"],
    );
    let id = printed_id(&got.err);
    assert_eq!(
        (got.code, got.err.clone()),
        (
            9,
            format!(
                "js: job id {id}\njs: the job may exist; run the same command again with --id {id}\n"
            )
        )
    );
    assert_eq!(fake.seen().len(), 1);
    let fake = Fake::new();
    fake.answer("SubmitJob", vec![Answer::Hang]);
    let got = js(
        &scratch,
        fake.dialer(),
        &["--json", "job", "submit", "--id", "mine-1"],
    );
    let lines: Vec<Value> = got
        .err
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        (got.code, lines),
        (
            9,
            vec![json!({
                "error": "unconfirmed",
                "message": "the job may exist; run the same command again with --id mine-1",
            })]
        )
    );
}

/// Asserts that a connection refused before anything was written exits 1 (`not-sent`) and
/// says nothing of a job that may exist, as text and as JSON; so does a refused token.
#[test]
fn a_refused_connection_exits_1_not_sent() {
    let scratch = Scratch::new("jobs_refused");
    let got = js(&scratch, refusing(), &["job", "submit", "--id", "mine-1"]);
    assert_eq!(
        (got.code, got.err.as_str()),
        (
            1,
            "js: cannot reach http://localhost:7070: nothing listens\n"
        )
    );
    let got = js(
        &scratch,
        refusing(),
        &["--json", "job", "get", "--job-id", "j-1"],
    );
    let error: Value = serde_json::from_str(&got.err).unwrap();
    assert_eq!(
        (got.code, error),
        (
            1,
            json!({ "error": "not-sent", "message": "cannot reach http://localhost:7070: nothing listens" })
        )
    );
    let fake = Fake::new();
    fake.answer(
        "GetJob",
        vec![Answer::Status(Code::Unauthenticated, "who?")],
    );
    let got = js(&scratch, fake.dialer(), &["job", "get", "--job-id", "j-1"]);
    assert_eq!(
        (got.code, got.err.as_str()),
        (1, "js: the server refused the token (unauthenticated)\n")
    );
}

/// Asserts that a call other than a keyed submit whose answer was lost exits 9 too, saying
/// it may have taken effect.
#[test]
fn a_lost_answer_to_another_call_exits_9() {
    let scratch = Scratch::new("jobs_lost_cancel");
    let fake = Fake::new();
    fake.answer("CancelJob", vec![Answer::Status(Code::Unavailable, "busy")]);
    let got = js(
        &scratch,
        fake.dialer(),
        &["job", "cancel", "--job-id", "j-1"],
    );
    assert_eq!(
        (got.code, got.err.as_str()),
        (
            9,
            "js: no answer from http://localhost:7070 after the request was sent: busy; the call may have taken effect\n"
        )
    );
}

/// Asserts the exit status of each definite refusal: `NOT_FOUND`, `FAILED_PRECONDITION` and
/// the refused submit `no-capable-host` are 3 (`refused`), `INVALID_ARGUMENT` is 4
/// (`rejected`), each with its reason.
#[test]
fn rejected_statuses_map_to_exits_3_and_4() {
    let scratch = Scratch::new("jobs_rejected");
    let cases = [
        (
            Answer::Status(Code::NotFound, "no job j-1"),
            3,
            "js: refused: no job j-1\n",
        ),
        (
            Answer::Status(Code::FailedPrecondition, "not ended"),
            3,
            "js: refused: not ended\n",
        ),
        (
            Answer::Refused(RefusalCode::NoCapableHost, "nothing has 64 GPUs"),
            3,
            "js: refused: no-capable-host: nothing has 64 GPUs\n",
        ),
        (
            Answer::Refused(RefusalCode::WallTimeNotPositive, ""),
            4,
            "js: rejected: wall-time-not-positive\n",
        ),
    ];
    for (answer, code, text) in cases {
        let fake = Fake::new();
        fake.answer("SubmitJob", vec![answer]);
        let got = js(
            &scratch,
            fake.dialer(),
            &["job", "submit", "--id", "mine-1"],
        );
        assert_eq!((got.code, got.err.as_str()), (code, text));
    }
}

/// Asserts the text of an answer (one line per field set, nested paths, list indexes, map
/// entries in key order, an enum by its short name) and its JSON (the message in proto3
/// JSON); and that a log tail prints the standard output tail, then the standard error
/// tail, each under its label without control characters, and says it was cut.
#[test]
fn answers_print_as_lines_or_proto3_json() {
    let scratch = Scratch::new("jobs_answers");
    let job = GetJobResponse {
        job: Some(Job {
            id: "j-1".to_owned(),
            spec: Some(JobSpec {
                image: "img".to_owned(),
                args: vec!["a".to_owned(), "b".to_owned()],
                labels: [("team", "core"), ("app", "x\u{7}y")]
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
                ..JobSpec::default()
            }),
            state: JobState::FailedToStart.into(),
            host_id: "h-1".to_owned(),
            times: Some(JobTimes {
                queued_at_ms: 5,
                started_at_ms: Some(7),
                ..JobTimes::default()
            }),
            ..Job::default()
        }),
    };
    let fake = Fake::new();
    fake.answer("GetJob", vec![Answer::Message(job.encode_to_vec())]);
    let got = js(&scratch, fake.dialer(), &["job", "get", "--job-id", "j-1"]);
    assert_eq!(
        (got.code, got.out.as_str()),
        (
            0,
            "job.id: j-1\njob.spec.image: img\njob.spec.args[0]: a\njob.spec.args[1]: b\njob.spec.labels.app: xy\njob.spec.labels.team: core\njob.state: failed-to-start\njob.host_id: h-1\njob.times.queued_at_ms: 5\njob.times.started_at_ms: 7\n"
        )
    );
    let got = js(
        &scratch,
        fake.dialer(),
        &["job", "get", "--job-id", "j-1", "--json"],
    );
    let printed: Value = serde_json::from_str(&got.out).unwrap();
    assert_eq!(
        printed,
        json!({ "job": {
            "id": "j-1",
            "spec": { "image": "img", "args": ["a", "b"], "labels": { "team": "core", "app": "x\u{7}y" } },
            "state": "JOB_STATE_FAILED_TO_START",
            "hostId": "h-1",
            "times": { "queuedAtMs": "5", "startedAtMs": "7" },
        }})
    );
    assert_eq!(got.out.lines().count(), 1);

    let logs = GetLogsResponse {
        state: JobState::Running.into(),
        stdout_tail: b"line one\nline two\n".to_vec(),
        stderr_tail: b"oo\x1bps".to_vec(),
        truncated: true,
        ..Default::default()
    };
    let fake = Fake::new();
    fake.answer("GetLogs", vec![Answer::Message(logs.encode_to_vec())]);
    let got = js(&scratch, fake.dialer(), &["job", "logs", "--job-id", "j-1"]);
    assert_eq!(
        (got.code, got.out.as_str()),
        (
            0,
            "state: running\nstdout_tail:\nline one\nline two\nstderr_tail:\noops\ntruncated: true\n"
        )
    );
}

/// Asserts that the global flags may follow a job command, and that without `--server` or
/// `--token-file` a job command is exit 2 naming the flag, as for the lease commands.
#[test]
fn the_global_flags_mean_what_they_mean_for_leases() {
    let scratch = Scratch::new("jobs_globals");
    let token = scratch.write("token", TOKEN, 0o600);
    let fake = Fake::new();
    let args: Vec<OsString> = [
        "js",
        "host",
        "list",
        "--server",
        PLAIN,
        "--token-file",
        token.to_str().unwrap(),
    ]
    .map(OsString::from)
    .to_vec();
    let got = run(args, services(Arc::new(Never)).with_dialer(fake.dialer()));
    assert_eq!((got.code, got.err.as_str()), (0, ""));
    assert_eq!(fake.seen()[0].path, call("ListHosts"));
    let no_server = run(
        [
            "js",
            "--token-file",
            token.to_str().unwrap(),
            "pool",
            "list",
        ]
        .map(OsString::from)
        .to_vec(),
        services(Arc::new(Never)),
    );
    assert_eq!(
        (no_server.code, no_server.err.as_str()),
        (2, "js: --server: is required\n")
    );
    let no_token = run(
        ["js", "--server", PLAIN, "pool", "list"]
            .map(OsString::from)
            .to_vec(),
        services(Arc::new(Never)),
    );
    assert_eq!(
        (no_token.code, no_token.err.as_str()),
        (2, "js: --token-file: is required\n")
    );
    let loose = scratch.write("loose", TOKEN, 0o644);
    let got = run(
        [
            "js",
            "--server",
            PLAIN,
            "--token-file",
            loose.to_str().unwrap(),
            "pool",
            "list",
        ]
        .map(OsString::from)
        .to_vec(),
        services(Arc::new(Never)).with_dialer(fake.dialer()),
    );
    assert_eq!(got.code, 2, "{}", got.err);
    assert!(got.err.starts_with("js: --token-file: "), "{}", got.err);
}

/// Asserts that the table is the golden `goldens/job_surface.json`: every command's words,
/// call, documentation, idempotency, request and answer, retry key and flags (name, field,
/// kind, repeated, required, `oneof`, documentation).
/// A change to the definition or to the generator's rules moves it.
#[test]
fn the_job_surface_matches_its_golden() {
    let table: Vec<Value> = surface()
        .iter()
        .map(|command| {
            json!({
                "path": command.path,
                "rpc": command.rpc,
                "request": command.request,
                "response": command.response,
                "streaming": command.streaming,
                "retry_key": command.retry_key,
                "flags": command.flags.iter().map(|flag| json!({
                    "name": flag.name,
                    "field": flag.field,
                    "kind": format!("{:?}", flag.kind),
                    "repeated": flag.repeated,
                    "required": flag.required,
                    "oneof": flag.oneof,
                    "doc": flag.doc,
                })).collect::<Vec<_>>(),
                "doc": command.doc,
                "idempotency": format!("{:?}", command.idempotency),
            })
        })
        .collect();
    let got = serde_json::to_string_pretty(&table).unwrap() + "\n";
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/cli/goldens/job_surface.json"
    );
    let want = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        got == want,
        "{path}: {}",
        serde_json::to_string(&got).unwrap()
    );
}

/// Asserts that an empty `--id` is a usage error that sends nothing: the service would mint
/// an id for it, and a run repeated after a lost answer would make a second job.
#[test]
fn an_empty_id_is_usage_exit_2() {
    let scratch = Scratch::new("jobs_empty_id");
    let fake = Fake::new();
    let got = js(&scratch, fake.dialer(), &["job", "submit", "--id", ""]);
    assert_eq!(
        (got.code, got.err.as_str()),
        (
            2,
            "js: --id: must not be empty: leave the flag out to have js draw an id\n"
        )
    );
    assert!(fake.seen().is_empty());
}

/// A writer that records what it is given in a shared log and refuses everything from its
/// `fail_after`-th write on.
struct Logged {
    log: Arc<std::sync::Mutex<Vec<String>>>,
    fail_after: usize,
    writes: usize,
}

impl std::io::Write for Logged {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        match self.writes > self.fail_after {
            true => Err(std::io::Error::other("the stream is closed")),
            false => {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("print {}", String::from_utf8_lossy(buf)));
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `js <words>` against `fake` with `out` and `err` as its streams; the dialer adds
/// `dial` to `log` when the command connects.
fn logged(
    scratch: &Scratch,
    fake: &Fake,
    log: &Arc<std::sync::Mutex<Vec<String>>>,
    words: &[&str],
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> u8 {
    let token = scratch.write("token", TOKEN, 0o600);
    let mut args: Vec<OsString> = vec![
        "js".into(),
        "--server".into(),
        PLAIN.into(),
        "--token-file".into(),
        token.into(),
    ];
    args.extend(words.iter().map(OsString::from));
    let seen = Arc::clone(log);
    let dialer = fake.dialer_with(move || seen.lock().unwrap().push("dial".to_owned()));
    js_cli::run_with(
        args,
        out,
        err,
        services(Arc::new(Never)).with_dialer(dialer),
    )
}

/// Asserts that the drawn id is printed before anything is dialled, as text and as JSON, and
/// that when it cannot be printed nothing is dialled and the command fails: without the id
/// the command could not be run again safely.
#[test]
fn the_drawn_id_is_printed_before_anything_is_sent() {
    let scratch = Scratch::new("jobs_id_first");
    for json in [false, true] {
        let fake = Fake::new();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut err = Logged {
            log: Arc::clone(&log),
            fail_after: usize::MAX,
            writes: 0,
        };
        let mut words = vec!["job", "submit"];
        words.extend(json.then_some("--json"));
        let code = logged(&scratch, &fake, &log, &words, &mut Vec::new(), &mut err);
        let log = log.lock().unwrap().clone();
        assert_eq!(
            (code, log.len(), log[1].as_str()),
            (0, 2, "dial"),
            "{log:?}"
        );
        let printed = log[0].strip_prefix("print ").unwrap();
        let id = match json {
            true => serde_json::from_str::<Value>(printed).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned(),
            false => printed
                .strip_prefix("js: job id ")
                .unwrap()
                .trim_end()
                .to_owned(),
        };
        let (_, request) = only::<SubmitJobRequest>(&fake);
        assert_eq!(request.spec.unwrap().job_id, id);
    }
    let fake = Fake::new();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut err = Logged {
        log: Arc::clone(&log),
        fail_after: 0,
        writes: 0,
    };
    let code = logged(
        &scratch,
        &fake,
        &log,
        &["job", "submit"],
        &mut Vec::new(),
        &mut err,
    );
    assert_eq!(code, 1);
    assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
    assert!(fake.seen().is_empty());
}

/// Asserts that an answer that cannot be written is a failure (exit 1), not a success with
/// the output lost.
#[test]
fn an_answer_that_cannot_be_written_fails() {
    let scratch = Scratch::new("jobs_out");
    let fake = Fake::new();
    let job = GetJobResponse {
        job: Some(Job {
            id: "j-1".to_owned(),
            ..Job::default()
        }),
    };
    fake.answer("GetJob", vec![Answer::Message(job.encode_to_vec())]);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut out = Logged {
        log: Arc::clone(&log),
        fail_after: 0,
        writes: 0,
    };
    let mut err = Vec::new();
    let code = logged(
        &scratch,
        &fake,
        &log,
        &["job", "get", "--job-id", "j-1"],
        &mut out,
        &mut err,
    );
    assert_eq!(
        (code, String::from_utf8(err).unwrap()),
        (
            1,
            "js: cannot write the answer: the stream is closed\n".to_owned()
        )
    );
}

/// Asserts that a log tail keeps its label when it is empty, and that bytes that are not
/// UTF-8 print with the replacement character; and that the JSON keeps the bytes (base64).
#[test]
fn empty_and_binary_log_tails_keep_their_labels() {
    let scratch = Scratch::new("jobs_tails");
    let logs = GetLogsResponse {
        state: JobState::Running.into(),
        stdout_tail: Vec::new(),
        stderr_tail: vec![b'o', 0xff, b'k'],
        truncated: false,
        ..Default::default()
    };
    let fake = Fake::new();
    fake.answer("GetLogs", vec![Answer::Message(logs.encode_to_vec())]);
    let got = js(&scratch, fake.dialer(), &["job", "logs", "--job-id", "j-1"]);
    assert_eq!(
        (got.code, got.out.as_str()),
        (
            0,
            "state: running\nstdout_tail:\n\nstderr_tail:\no\u{fffd}k\n"
        )
    );
    let got = js(
        &scratch,
        fake.dialer(),
        &["job", "logs", "--job-id", "j-1", "--json"],
    );
    let printed: Value = serde_json::from_str(&got.out).unwrap();
    assert_eq!(
        printed,
        json!({ "state": "JOB_STATE_RUNNING", "stderrTail": "b/9r" })
    );
}
