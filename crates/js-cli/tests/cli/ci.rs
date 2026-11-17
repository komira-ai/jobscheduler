//! `js ci run` against the in-memory job server of `fake_jobs`, with a waiter that returns at
//! once: the job it submits for a step, the status it exits with for each end, the outputs it
//! takes out of the job's stdout and unpacks, the lines it writes for GitHub Actions, and the
//! flags it refuses before anything is sent. No port, no clock.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::{Future, ready};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use js_client::jobs::Waiter;
use js_proto::v1::{
    Constraints, ExecMode, GetJobResponse, GetLogsRequest, GetLogsResponse, GetResultResponse, Job,
    JobFile, JobResult, JobSpec, JobState, LogStream, NetworkLevel, ResultReason, SubmitJobRequest,
};
use prost::Message as _;
use tonic::Code;

use crate::fake_jobs::{Answer, Fake};
use crate::support::{Outcome, Scratch, TOKEN};

/// A loopback origin over plain http; the in-memory dialer never dials it.
const PLAIN: &str = "http://localhost:7070";

/// A pinned image.
const IMAGE: &str =
    "registry.example/ci@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The repository the steps fetch.
const URL: &str = "https://github.com/komira-ai/jobscheduler";

/// A full commit id.
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A waiter that returns at once.
struct Now;

impl Waiter for Now {
    fn wait(&self, _: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(ready(()))
    }
}

/// Runs `js --server PLAIN --token-file <token> ci run <words>` against `fake`.
fn ci(scratch: &Scratch, fake: &Fake, words: &[&str]) -> Outcome {
    let token = scratch.write("token", TOKEN, 0o600);
    let mut args: Vec<OsString> = vec![
        "js".into(),
        "--server".into(),
        PLAIN.into(),
        "--token-file".into(),
        token.into(),
        "ci".into(),
        "run".into(),
    ];
    args.extend(words.iter().map(OsString::from));
    let services = js_cli::Services::new(Box::new(|_| Ok(())))
        .with_dialer(fake.dialer())
        .with_job_waiter(Arc::new(Now));
    crate::support::run(args, services)
}

/// The flags every step gives, with `more` after them; a flag of `more` that the base gives
/// takes the base's place.
fn step<'a>(more: &[&'a str]) -> Vec<&'a str> {
    let base = [
        "--os",
        "linux",
        "--image",
        IMAGE,
        "--step",
        "komira-ai/jobscheduler/beta/macos",
        "--run",
        "make test",
        "--source-url",
        URL,
        "--source-commit",
        COMMIT,
        "--id",
        "step-1",
    ];
    let mut words: Vec<&str> = Vec::new();
    for pair in base.chunks(2) {
        if !more.contains(&pair[0]) {
            words.extend(pair);
        }
    }
    words.extend(more.iter().copied());
    words
}

/// An answer of `GetJob`: the job in `state`.
fn job(state: JobState) -> Answer {
    let answer = GetJobResponse {
        job: Some(Job {
            id: "step-1".to_owned(),
            state: state.into(),
            ..Job::default()
        }),
    };
    Answer::Message(answer.encode_to_vec())
}

/// An answer of `GetResult`: a failed job's result for `reason`, with `exit_code`.
fn result(reason: ResultReason, exit_code: Option<i32>) -> Answer {
    let answer = GetResultResponse {
        result: Some(JobResult {
            state: JobState::Failed.into(),
            reason: reason.into(),
            exit_code,
            ..JobResult::default()
        }),
    };
    Answer::Message(answer.encode_to_vec())
}

/// The whole of each stream at once: `stdout` made from the submitted job (its marker is in
/// its arguments), and an empty stderr.
fn streams(fake: &Fake, stdout: impl Fn(&JobSpec) -> Vec<u8> + Send + Sync + 'static) {
    let seen = fake.clone();
    fake.respond("GetLogs", move |message| {
        let read = GetLogsRequest::decode(message).unwrap();
        let data = match LogStream::try_from(read.stream).unwrap() {
            LogStream::Stdout => stdout(&submitted(&seen)),
            _ => Vec::new(),
        };
        let page = GetLogsResponse {
            state: JobState::Succeeded.into(),
            next_offset: u64::try_from(data.len()).unwrap(),
            data,
            eof: true,
            ..GetLogsResponse::default()
        };
        Answer::Message(page.encode_to_vec())
    });
}

/// The job of the one submit `fake` read.
fn submitted(fake: &Fake) -> JobSpec {
    let path = "/jobscheduler.v1.JobService/SubmitJob";
    let seen = fake.seen().into_iter().find(|seen| seen.path == path);
    let request = SubmitJobRequest::decode(seen.unwrap().message.as_slice()).unwrap();
    request.spec.unwrap()
}

/// A job that writes `before`, then the outputs frame of `archive` for its marker.
fn framed(before: &'static [u8], archive: Vec<u8>) -> impl Fn(&JobSpec) -> Vec<u8> {
    move |spec| {
        let marker = &spec.args[6];
        let head = format!("\n{marker} {}\n", archive.len());
        [before, head.as_bytes(), &archive].concat()
    }
}

/// A tar archive of `files`, each a path and its contents.
fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, contents.as_bytes())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

/// The words of the outputs flags: `paths` and the directory `dir`.
fn outputs<'a>(paths: &'a str, dir: &'a str) -> [&'a str; 4] {
    ["--outputs", paths, "--outputs-dir", dir]
}

/// Asserts the job `ci run` submits for a macOS step: `/bin/sh` running the script with the
/// step, URL, commit and `-` (no outputs) after it; macOS on arm64 under sandbox-exec; the
/// wall limit given; open network; the token as the job file `source-token`; the size class
/// written from the step's name and the fallback `ci-macos`; the environment given; and no
/// CPU or memory. The drawn id is printed before the submit and the end after it. It catches
/// a step sent to the wrong platform, a token sent in the environment or the arguments, a
/// job sized by a CPU or memory it names, and a class that is not the step's.
#[test]
fn a_macos_step_is_a_job_on_a_mac_sized_by_the_steps_name() {
    let scratch = Scratch::new("ci_macos");
    let token = scratch.write("source", "gh-token-value\n", 0o600);
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Succeeded)]);
    streams(&fake, |_| b"done\n".to_vec());
    let words = [
        "--os",
        "macos",
        "--image",
        IMAGE,
        "--step",
        "komira-ai/jobscheduler/macos-probe/probe",
        "--run",
        "cargo test",
        "--source-url",
        URL,
        "--source-commit",
        COMMIT,
        "--source-token-file",
        token.to_str().unwrap(),
        "--env",
        "RUST_LOG=debug",
        "--wall-limit",
        "15m",
    ];
    let got = ci(&scratch, &fake, &words);
    let spec = submitted(&fake);
    let id = spec.job_id.clone();
    assert_eq!(id.len(), 32, "{id}");
    assert_eq!(
        (got.code, got.out.as_str(), got.err),
        (
            0,
            "done\n",
            format!("js: job id {id}\njob {id} succeeded\n")
        )
    );
    assert!(spec.args[1].starts_with("set -eu\n"), "{}", spec.args[1]);
    let want = JobSpec {
        job_id: id,
        image: IMAGE.to_owned(),
        command: "/bin/sh".to_owned(),
        args: ["-c", &spec.args[1], "js-ci", "cargo test", URL, COMMIT, "-"]
            .map(str::to_owned)
            .to_vec(),
        env: BTreeMap::from([("RUST_LOG".to_owned(), "debug".to_owned())]),
        constraints: Some(Constraints {
            os: "macos".to_owned(),
            arch: "arm64".to_owned(),
            exec: ExecMode::SandboxExec.into(),
            ..Constraints::default()
        }),
        wall_limit_seconds: Some(900),
        network: NetworkLevel::Open.into(),
        files: vec![JobFile {
            name: "source-token".to_owned(),
            contents: "gh-token-value".to_owned(),
        }],
        labels: BTreeMap::from([
            (
                "size-class".to_owned(),
                "komira-ai.jobscheduler.macos-probe.probe".to_owned(),
            ),
            ("size-fallback".to_owned(), "ci-macos".to_owned()),
        ]),
        ..JobSpec::default()
    };
    assert_eq!(spec, want);
}

/// Asserts a Linux step with every default it can change: a container on any architecture,
/// the network level given, a fallback class written from the name given, an hour's wall
/// limit, the id given (nothing drawn or printed for it), and no job file without a token.
/// It catches a Linux step held to one architecture and a fallback sent unwritten.
#[test]
fn a_linux_step_is_a_container_job_with_the_defaults() {
    let scratch = Scratch::new("ci_linux");
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Succeeded)]);
    streams(&fake, |_| Vec::new());
    let words = step(&["--network", "none", "--size-fallback", "Cargo Test"]);
    let got = ci(&scratch, &fake, &words);
    assert_eq!((got.code, got.err.as_str()), (0, "job step-1 succeeded\n"));
    let spec = submitted(&fake);
    assert_eq!(spec.job_id, "step-1");
    assert_eq!(
        spec.constraints.unwrap(),
        Constraints {
            os: "linux".to_owned(),
            exec: ExecMode::Container.into(),
            ..Constraints::default()
        }
    );
    assert_eq!(spec.network, i32::from(NetworkLevel::None));
    assert_eq!(spec.wall_limit_seconds, Some(3600));
    assert!(spec.files.is_empty());
    assert_eq!(spec.labels["size-fallback"], "cargo.test");
    assert_eq!(
        spec.labels["size-class"],
        "komira-ai.jobscheduler.beta.macos"
    );
    let allow = step(&["--network", "allow-list"]);
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Succeeded)]);
    streams(&fake, |_| Vec::new());
    assert_eq!(ci(&scratch, &fake, &allow).code, 0);
    assert_eq!(submitted(&fake).network, i32::from(NetworkLevel::AllowList));
}

/// Runs a step whose job ends in `state`, with `results` the answers of `GetResult` in
/// order, and gives the outcome and the lines written for GitHub Actions.
fn ended(name: &str, state: JobState, results: Vec<Answer>) -> (Outcome, String) {
    let scratch = Scratch::new(name);
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Running), job(state)]);
    fake.answer("GetResult", results);
    streams(&fake, |_| Vec::new());
    let file = scratch.path("github-output");
    let words = step(&["--github-output", file.to_str().unwrap()]);
    let got = ci(&scratch, &fake, &words);
    let lines = std::fs::read_to_string(file).unwrap();
    (got, lines)
}

/// Asserts that a step that failed with an exit code of its own exits with that code, and
/// writes it with the outcome `failed`; that one that failed with no exit code (stopped for
/// its memory), with one outside 1 to 255, or whose result cannot be read again is 10; and
/// the GitHub lines of each. It catches every failure collapsed to one status, and a code
/// outside a process's statuses passed on cut short.
#[test]
fn a_failed_step_exits_with_its_own_code() {
    let exit = ResultReason::Exit;
    let (got, lines) = ended("ci_code", JobState::Failed, vec![result(exit, Some(3))]);
    assert_eq!((got.code, got.err.as_str()), (3, "job step-1 failed\n"));
    assert_eq!(lines, "job-id=step-1\noutcome=failed\nexit-code=3\n");
    let memory = ResultReason::Memory;
    let (got, lines) = ended("ci_memory", JobState::Failed, vec![result(memory, None)]);
    assert_eq!(got.code, 10);
    assert_eq!(lines, "job-id=step-1\noutcome=failed\nexit-code=\n");
    let (got, lines) = ended("ci_big", JobState::Failed, vec![result(exit, Some(300))]);
    assert_eq!(got.code, 10);
    assert_eq!(lines, "job-id=step-1\noutcome=failed\nexit-code=300\n");
    let lost = vec![
        result(exit, Some(3)),
        Answer::Status(Code::NotFound, "gone"),
    ];
    let (got, lines) = ended("ci_reread", JobState::Failed, lost);
    assert_eq!(got.code, 10);
    assert_eq!(lines, "job-id=step-1\noutcome=failed\nexit-code=\n");
}

/// Asserts the status and the GitHub lines of every other end: succeeded 0 (exit code 0),
/// timed out 12 (with no exit code read for it), cancelled 11, lost 13, failed to start 14. It catches
/// an end given another end's status or outcome name.
#[test]
fn each_other_end_has_js_own_status() {
    let wall = vec![result(ResultReason::WallLimit, Some(137))];
    let ends = [
        (JobState::Succeeded, vec![], 0, "succeeded"),
        (JobState::Failed, wall, 12, "timed-out"),
        (JobState::Cancelled, vec![], 11, "cancelled"),
        (JobState::Lost, vec![], 13, "lost"),
        (JobState::FailedToStart, vec![], 14, "failed-to-start"),
    ];
    for (state, results, code, name) in ends {
        let (got, lines) = ended("ci_ends", state, results);
        assert_eq!(
            (got.code, got.err),
            (code, format!("job step-1 {name}\n")),
            "{name}"
        );
        let exit_code = match name {
            "succeeded" => "0",
            _ => "",
        };
        assert_eq!(
            lines,
            format!("job-id=step-1\noutcome={name}\nexit-code={exit_code}\n")
        );
    }
}

/// Asserts that the archive the job wrote after its output is taken out of stdout and
/// unpacked into the outputs directory, made with its parents, while the step's own output
/// is written whole; and that the job carries the marker and the declared paths in its
/// arguments, the marker being the job's id (so the same command run again with that id
/// submits the same job). It catches outputs left in the log, a frame shown to the user, a
/// path declared but not sent, and a marker that differs between two runs of one id.
#[test]
fn the_outputs_the_job_wrote_are_unpacked() {
    let scratch = Scratch::new("ci_outputs");
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Succeeded)]);
    let archive = tar_of(&[("out/report.json", "{}\n"), ("trace.sb", "trace")]);
    streams(&fake, framed(b"built\n", archive));
    let dir = scratch.path("deep/outputs");
    let words = step(&outputs("out\n\ntrace.sb\n", dir.to_str().unwrap()));
    let got = ci(&scratch, &fake, &words);
    assert_eq!(
        (got.code, got.out.as_str(), got.err.as_str()),
        (0, "built\n", "job step-1 succeeded\n")
    );
    let read = |path: &str| std::fs::read_to_string(dir.join(path)).unwrap();
    assert_eq!(
        (read("out/report.json"), read("trace.sb")),
        ("{}\n".to_owned(), "trace".to_owned())
    );
    let args = submitted(&fake).args;
    assert_eq!(args[6], "js-ci-outputs-step-1");
    assert_eq!(args[7..], ["out", "trace.sb"]);
}

/// Runs a step with outputs declared, whose job ends in `state` and writes `stdout`, and
/// gives the outcome.
fn with_outputs(
    name: &str,
    state: JobState,
    results: Vec<Answer>,
    stdout: impl Fn(&JobSpec) -> Vec<u8> + Send + Sync + 'static,
    dir: &Path,
) -> Outcome {
    let scratch = Scratch::new(name);
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(state)]);
    fake.answer("GetResult", results);
    streams(&fake, stdout);
    let words = step(&outputs("out", dir.to_str().unwrap()));
    ci(&scratch, &fake, &words)
}

/// Asserts what a step whose outputs cannot be read back gives: a successful job with no
/// frame, with a broken frame or with an archive that is not tar is 16 with the reason on
/// stderr; a job that failed keeps its own code and still says why; an outputs directory
/// that cannot be made is 16 too. It catches a successful step whose outputs were lost
/// passing, and a step's failure hidden behind its outputs'.
#[test]
fn outputs_that_cannot_be_read_back_are_said_and_fail_a_successful_step() {
    let scratch = Scratch::new("ci_outputs_lost");
    let dir = scratch.path("outputs");
    let none =
        "js: the job wrote no outputs: its script did not reach them (see the job's stderr)\n";
    let got = with_outputs(
        "ci_none",
        JobState::Succeeded,
        vec![],
        |_| b"x\n".to_vec(),
        &dir,
    );
    assert_eq!((got.code, got.out.as_str()), (16, "x\n"));
    assert_eq!(got.err, format!("job step-1 succeeded\n{none}"));
    let exit = vec![result(ResultReason::Exit, Some(2))];
    let got = with_outputs(
        "ci_none_failed",
        JobState::Failed,
        exit,
        |_| Vec::new(),
        &dir,
    );
    assert_eq!(
        (got.code, got.err),
        (2, format!("job step-1 failed\n{none}"))
    );
    let broken = |spec: &JobSpec| format!("\n{} 1x\n", spec.args[6]).into_bytes();
    let got = with_outputs("ci_broken", JobState::Succeeded, vec![], broken, &dir);
    assert_eq!(
        (got.code, got.err.as_str()),
        (
            16,
            "job step-1 succeeded\njs: the outputs frame is broken: its length is not a number\n"
        )
    );
    let junk = framed(b"", vec![b'z'; 700]);
    let got = with_outputs("ci_junk", JobState::Succeeded, vec![], junk, &dir);
    let prefix = format!(
        "job step-1 succeeded\njs: cannot unpack the outputs into {}: ",
        dir.display()
    );
    assert_eq!(got.code, 16);
    assert!(got.err.starts_with(&prefix), "{}", got.err);
    let file = scratch.write("a-file", "", 0o600);
    let archive = framed(b"", tar_of(&[("out", "x")]));
    let got = with_outputs(
        "ci_dir_file",
        JobState::Succeeded,
        vec![],
        archive,
        &file.join("sub"),
    );
    assert_eq!(got.code, 16);
    assert!(
        got.err.contains("cannot unpack the outputs into"),
        "{}",
        got.err
    );
}

/// Asserts that an archive of no bytes (the job made none of its outputs) leaves the outputs
/// directory made and empty, and the step's status as it is. It catches a step failed because
/// it made none of the outputs it may make.
#[test]
fn an_empty_archive_makes_an_empty_outputs_directory() {
    let scratch = Scratch::new("ci_outputs_empty");
    let dir = scratch.path("outputs");
    let empty = framed(b"ok\n", Vec::new());
    let got = with_outputs("ci_empty", JobState::Succeeded, vec![], empty, &dir);
    assert_eq!((got.code, got.out.as_str()), (0, "ok\n"));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

/// A writer that fails every write.
struct Closed;

impl std::io::Write for Closed {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("closed"))
    }
}

/// Asserts that when the id `js` drew cannot be printed (stderr is closed), nothing is sent
/// and the status is 1: the id is the only way to run the step again safely. It catches a
/// job submitted under an id nobody saw.
#[test]
fn a_drawn_id_that_cannot_be_printed_sends_nothing() {
    let scratch = Scratch::new("ci_unprinted");
    let fake = Fake::new();
    let token = scratch.write("token", TOKEN, 0o600);
    let words = step(&[]);
    let words: Vec<&str> = words
        .chunks(2)
        .filter(|pair| pair[0] != "--id")
        .flatten()
        .copied()
        .collect();
    let mut args: Vec<OsString> = vec!["js".into(), "--server".into(), PLAIN.into()];
    args.extend([
        "--token-file".into(),
        token.into(),
        "ci".into(),
        "run".into(),
    ]);
    args.extend(words.iter().map(OsString::from));
    let services = js_cli::Services::new(Box::new(|_| Ok(()))).with_dialer(fake.dialer());
    let mut out = Vec::new();
    let code = js_cli::run_with(args, &mut out, &mut Closed, services);
    assert_eq!((code, out), (1, Vec::new()));
    assert!(fake.seen().is_empty());
}

/// Asserts that a `--github-output` file that cannot be written (a directory) is said on
/// stderr and leaves the job's status as it is. It catches a successful step failed by
/// its bookkeeping.
#[test]
fn a_github_output_that_cannot_be_written_is_said_and_keeps_the_status() {
    let scratch = Scratch::new("ci_github_dir");
    let fake = Fake::new();
    fake.answer("GetJob", vec![job(JobState::Succeeded)]);
    streams(&fake, |_| Vec::new());
    let dir = scratch.path("");
    let words = step(&["--github-output", dir.to_str().unwrap()]);
    let got = ci(&scratch, &fake, &words);
    let why = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&dir)
        .unwrap_err();
    assert_eq!(
        (got.code, got.err),
        (
            0,
            format!(
                "job step-1 succeeded\njs: cannot write the step's outputs to {}: {why}\n",
                dir.display()
            )
        )
    );
}

/// Asserts that a submit the service refuses exits as `job submit` does (4 for an invalid
/// request), with nothing more sent, and that the GitHub lines name the failure's kind with
/// no exit code. It catches a refused step reported as a job's end.
#[test]
fn a_refused_submit_exits_as_job_submit_does() {
    let scratch = Scratch::new("ci_refused");
    let fake = Fake::new();
    fake.answer(
        "SubmitJob",
        vec![Answer::Status(
            Code::InvalidArgument,
            "wall-time-above-ceiling",
        )],
    );
    let file = scratch.path("github-output");
    let words = step(&["--github-output", file.to_str().unwrap()]);
    let got = ci(&scratch, &fake, &words);
    assert_eq!(
        (got.code, got.err.as_str()),
        (4, "js: rejected: wall-time-above-ceiling\n")
    );
    let calls: Vec<String> = fake.seen().into_iter().map(|seen| seen.path).collect();
    assert_eq!(calls, ["/jobscheduler.v1.JobService/SubmitJob"]);
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "job-id=step-1\noutcome=rejected\nexit-code=\n"
    );
}

/// Asserts each flag `ci run` refuses before anything is sent, by its whole message, with
/// status 2: a commit that is short, upper-case or not hex; a URL that is plain http, has
/// no host or carries a user; an output that is absolute, climbs out, or begins with `-`;
/// a step and a fallback with no letter or digit; an empty id; a token file others can read;
/// an environment entry with no key; and outputs with no directory. It catches a credential
/// put where every read of the job shows it, and a path that would leave the checkout.
#[test]
fn wrong_flags_are_refused_before_anything_is_sent() {
    let scratch = Scratch::new("ci_flags");
    let open = scratch.write("open-token", "t", 0o644);
    let open = open.to_str().unwrap().to_owned();
    let commit = |value: &str| {
        format!(
            "js: --source-commit: {value:?} is not a full commit id: 40 or 64 lower-case hex digits\n"
        )
    };
    let url = |value: &str| {
        format!(
            "js: --source-url: {value:?} is not an https:// URL with a host and no user or password\n"
        )
    };
    let path = |value: &str| {
        format!(
            "js: --outputs: {value:?} is not a path inside the checkout: it must be relative, hold no `..` and not begin with -\n"
        )
    };
    let upper = COMMIT.to_uppercase();
    let not_hex = "g".repeat(40);
    let cases: Vec<(Vec<&str>, String)> = vec![
        (vec!["--source-commit", "0123abc"], commit("0123abc")),
        (vec!["--source-commit", &upper], commit(&upper)),
        (vec!["--source-commit", &not_hex], commit(&not_hex)),
        (
            vec!["--source-url", "http://github.com/a/b"],
            url("http://github.com/a/b"),
        ),
        (vec!["--source-url", "https:///a"], url("https:///a")),
        (
            vec!["--source-url", "https://x:y@github.com/a"],
            url("https://x:y@github.com/a"),
        ),
        (
            vec!["--outputs", "/etc/passwd", "--outputs-dir", "d"],
            path("/etc/passwd"),
        ),
        (
            vec!["--outputs", "ok\na/../../b", "--outputs-dir", "d"],
            path("a/../../b"),
        ),
        (vec!["--outputs=-rf", "--outputs-dir", "d"], path("-rf")),
        (
            vec!["--step", "//:"],
            "js: --step: \"//:\" holds no letter or digit\n".to_owned(),
        ),
        (
            vec!["--size-fallback", "_._"],
            "js: --size-fallback: \"_._\" holds no letter or digit\n".to_owned(),
        ),
        (
            vec!["--id", ""],
            "js: --id: must not be empty: leave the flag out to have js draw an id\n".to_owned(),
        ),
        (
            vec!["--source-token-file", &open],
            format!(
                "js: --source-token-file: {open} must be a regular file owned by you and readable only by you (mode 0600 or 0400) that holds one token of printable ASCII\n"
            ),
        ),
    ];
    for (more, want) in cases {
        let fake = Fake::new();
        let got = ci(&scratch, &fake, &step(&more));
        assert_eq!(
            (got.code, got.out.as_str(), got.err),
            (2, "", want),
            "{more:?}"
        );
        assert!(fake.seen().is_empty(), "{more:?}");
    }
    let clap_refused = [
        vec!["--env", "=x"],
        vec!["--outputs", "a"],
        vec!["--wall-limit", "0"],
    ];
    for more in clap_refused {
        let fake = Fake::new();
        let got = ci(&scratch, &fake, &step(&more));
        assert_eq!((got.code, got.out.as_str()), (2, ""), "{more:?}");
        assert!(got.err.starts_with("error: "), "{more:?}: {}", got.err);
        assert!(fake.seen().is_empty(), "{more:?}");
    }
}
