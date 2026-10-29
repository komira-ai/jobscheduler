//! The log tail of the command-line runtime, against a stub program the test writes and
//! the bounded system runner starts as a real child process: the stub prints what a
//! `logs` call would, on its two streams.

use std::time::Duration;

use js_agent::cli_runtime::{CliRuntime, Settings, system_runner};
use js_agent::lifecycle::{RunHandle, Runtime};
use js_agent_wire::LogTail;
use js_agent_wire::limits::MAX_LOG_STREAM_LEN;

use super::stub::{ID, settle};
use super::{HOST, made_at, platform};
use crate::support::Scratch;

/// A runtime over a stub whose `logs` call runs `logs_body` (shell) and whose `--version`
/// answers like Docker.
fn over(name: &str, logs_body: &str) -> (CliRuntime, Scratch) {
    let dir = Scratch::new(name);
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'Docker version 27.3.1, build x' ;;\nlogs)\n{logs_body}\n;;\nesac\n"
    );
    let bin = dir.write("docker", &script, 0o755);
    settle(&bin);
    let settings = Settings::new(&bin, None, None).unwrap();
    let runner = Box::new(system_runner(Duration::from_secs(10)));
    let rt = CliRuntime::open(settings, HOST, &platform("linux"), made_at, runner);
    (rt.ok().unwrap(), dir)
}

fn tail_of(rt: &mut CliRuntime) -> Result<Option<LogTail>, String> {
    rt.log_tail(&RunHandle(ID.to_owned()))
        .map_err(|err| err.to_string())
}

/// Asserts that standard output and standard error of the job come back as two strings,
/// each as written, and that a short log is not marked truncated. It catches the two
/// streams merged into one, swapped, or read from one pipe only.
#[test]
fn stdout_and_stderr_are_kept_apart() {
    let (mut rt, _dir) = over(
        "logs-apart",
        "printf 'to stdout\\n'; printf 'to stderr\\n' >&2",
    );
    let tail = tail_of(&mut rt).unwrap().unwrap();
    assert_eq!(
        tail,
        LogTail {
            stdout: "to stdout\n".to_owned(),
            stderr: "to stderr\n".to_owned(),
            truncated: false,
        }
    );
}

/// Asserts that a stream longer than the bound is cut to its last bytes (the end is kept,
/// the beginning is dropped), exactly at the bound, that the other stream is untouched,
/// and that `truncated` is set. It catches a head kept in place of the tail, a bound that
/// is not applied, and a flag that is not raised.
#[test]
fn a_long_stream_is_cut_to_its_tail_and_marked() {
    let body = "head -c 100000 /dev/zero | tr '\\0' 'a'; printf 'THE-END'; printf 'err' >&2";
    let (mut rt, _dir) = over("logs-long", body);
    let tail = tail_of(&mut rt).unwrap().unwrap();
    assert_eq!(tail.stdout.len(), MAX_LOG_STREAM_LEN);
    assert!(tail.stdout.ends_with("aaaTHE-END"), "the end is kept");
    assert!(tail.stdout.starts_with('a'));
    assert_eq!(tail.stderr, "err");
    assert!(tail.truncated);
}

/// Asserts that a multi-byte character at the cut is not split: a log of three-byte
/// characters is cut in the middle of one (the bound is not a multiple of three), and the
/// tail holds whole characters only, no replacement character, and fits the bound. It
/// catches a cut by bytes that leaves a broken character at the start (shown as U+FFFD, and
/// longer than the byte it replaced, so over the bound).
#[test]
fn a_multibyte_character_at_the_cut_is_not_split() {
    let body = "i=0; while [ $i -lt 8192 ]; do printf '\\342\\202\\254'; i=$((i+1)); done";
    let (mut rt, _dir) = over("logs-multibyte", body);
    let tail = tail_of(&mut rt).unwrap().unwrap();
    assert!(tail.truncated);
    assert!(!tail.stdout.contains('\u{FFFD}'), "no broken character");
    assert!(tail.stdout.chars().all(|c| c == '\u{20AC}'));
    assert!(tail.stdout.len() <= MAX_LOG_STREAM_LEN);
    assert!(
        tail.stdout.len() >= MAX_LOG_STREAM_LEN - 2,
        "only the cut one is lost"
    );
}

/// Asserts that a bad byte in the log is shown as the replacement character and that the
/// text still fits the bound when every byte is bad (one byte becomes three). It catches a
/// lossy conversion after the cut that pushes the tail over the wire's limit, which the
/// server would refuse with the whole heartbeat.
#[test]
fn invalid_bytes_are_replaced_and_the_text_still_fits() {
    let body = "head -c 20000 /dev/zero | tr '\\0' '\\377'";
    let (mut rt, _dir) = over("logs-invalid", body);
    let tail = tail_of(&mut rt).unwrap().unwrap();
    assert!(tail.stdout.starts_with('\u{FFFD}'));
    assert!(tail.stdout.len() <= MAX_LOG_STREAM_LEN);
    assert!(tail.truncated);
}

/// Asserts that a `logs` call that exits non-zero is an error that names the call, and
/// that it carries nothing the program printed. It catches a failure read as an empty log
/// (a run reported as having printed nothing) and a log reaching an error text.
#[test]
fn a_failing_logs_call_is_an_error_naming_the_call() {
    let (mut rt, _dir) = over(
        "logs-fail",
        "echo secret-output; echo secret-err >&2; exit 1",
    );
    let err = tail_of(&mut rt).unwrap_err();
    assert!(err.starts_with("logs"), "{err}");
    assert!(!err.contains("secret"), "{err}");
}

/// Asserts the boundary of the byte cap: one invalid byte and `cap - 2` valid ones become
/// `cap + 1` bytes of text (the bad byte turns into a three-byte U+FFFD), exactly one over,
/// and the tail is cut to fit while the log that is exactly at the cap is not. It catches
/// an off-by-one in the cap check (`>` for `>=`, or a slack of one byte), which would send
/// a tail the wire refuses.
#[test]
fn text_one_byte_over_the_cap_is_cut_and_at_the_cap_is_kept() {
    let body = format!(
        "printf '\\377'; head -c {} /dev/zero | tr '\\0' 'a'",
        MAX_LOG_STREAM_LEN - 2
    );
    let (mut rt, _dir) = over("logs-one-over", &body);
    let tail = tail_of(&mut rt).unwrap().unwrap();
    assert!(
        tail.stdout.len() <= MAX_LOG_STREAM_LEN,
        "{}",
        tail.stdout.len()
    );
    assert_eq!(tail.stdout, "a".repeat(MAX_LOG_STREAM_LEN - 2));
    assert!(tail.truncated);

    let body = format!("head -c {MAX_LOG_STREAM_LEN} /dev/zero | tr '\\0' 'a'");
    let (mut rt, _dir) = over("logs-at-cap", &body);
    let tail = tail_of(&mut rt).unwrap().unwrap();
    assert_eq!(tail.stdout.len(), MAX_LOG_STREAM_LEN);
    assert!(!tail.truncated, "exactly the cap is not cut");
}

/// Asserts that a `logs` call that never exits is ended at its limit: the call is an error
/// that says it timed out (so the lifecycle notes `logs-failed` and sends the result with
/// no tail, as `a_failed_logs_call_still_yields_the_result_without_a_tail` shows), and the
/// stub process is gone, not left running. The limit is passed in, so the test does not
/// wait the production 30 s and asserts nothing about elapsed time. It catches a stuck
/// engine holding up a finished job's result, and a child left behind by the kill.
#[test]
fn a_logs_call_that_never_exits_is_killed_at_its_limit() {
    let (rt, dir) = over("logs-stuck", "echo $$ > \"$0.pid\"; exec sleep 600");
    let mut rt = rt.with_logs_limit(Duration::from_millis(500));
    let err = tail_of(&mut rt).unwrap_err();
    assert!(err.starts_with("logs"), "{err}");
    assert!(err.contains("timed out"), "{err}");
    let pid = std::fs::read_to_string(dir.path("docker.pid")).unwrap();
    let pid = pid.trim();
    assert!(!pid.is_empty());
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the stub (pid {pid}) is still there"
    );
}
