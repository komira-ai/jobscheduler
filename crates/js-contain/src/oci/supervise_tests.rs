//! The supervise mode run in process with a stand-in `crun` script. The supervisor's core
//! (the exit record, the process identity, the handle and the handshake) is `js-supervise`,
//! tested there.

use std::os::unix::ffi::OsStringExt as _;

use super::*;
use crate::oci::StreamTail;

fn dir(name: &str) -> crate::test_dir::TestDir {
    crate::unit_dir(&format!("supervise-{name}"))
}

fn stream(text: &str, total: u64, file_cut: bool) -> StreamTail {
    StreamTail {
        bytes: text.as_bytes().to_vec(),
        total,
        file_cut,
    }
}

/// The file `js-supervise` writes a record to before it renames it into place.
fn staged_record_path(lease: &Path) -> PathBuf {
    lease.join(".exit.json.tmp")
}

/// A stand-in `crun` that runs `body` as a shell script.
fn crun(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("crun");
    js_testkit::spawn::install_script(&path, &format!("#!/bin/sh\n{body}\n"), "755");
    path
}

/// A lease directory with a bundle directory in it.
fn lease_in(dir: &Path) -> PathBuf {
    let lease = dir.join("lease");
    std::fs::create_dir_all(lease.join("bundle")).expect("lease");
    lease
}

/// The arguments of the supervise mode for the lease `lease`.
fn args(crun: &Path, lease: &Path, file_cap: &str, tail: &str) -> Vec<OsString> {
    vec![
        crun.into(),
        lease.into(),
        "job-1".into(),
        file_cap.into(),
        tail.into(),
        "1000".into(),
    ]
}

/// Runs the supervise mode in process; the exit status and what it wrote as its ready word.
fn supervise(arguments: &[OsString], runtime: RuntimeMaker) -> (u8, Vec<u8>) {
    let mut ready = Vec::new();
    let code = run_with(arguments, &mut ready, runtime);
    (code, ready)
}

/// Catches a supervisor that does not run the container as `Oci::start` does or does not
/// say how it ended: the stand-in `crun` runs over the lease, the ready word is written, and
/// the exit record has the status, both streams' tails and counts, and a time; the capture
/// files hold the output; no staged record is left; the exit status is 0.
#[test]
fn the_supervise_mode_runs_crun_and_records_how_it_ended() {
    let dir = dir("run");
    let lease = lease_in(&dir);
    let crun = crun(&dir, "echo hello; echo oops >&2; exit 3");
    let (code, ready) = supervise(&args(&crun, &lease, "1048576", "65536"), one_thread_runtime);
    assert_eq!((code, ready.as_slice()), (0, READY));
    let record = read_exit_record(&lease)
        .expect("readable")
        .expect("written");
    assert_eq!(record.end, RunEnd::Exited(3));
    assert_eq!(record.tail.stdout, stream("hello\n", 6, false));
    assert_eq!(record.tail.stderr, stream("oops\n", 5, false));
    assert!(record.finished_ms > 0);
    let file = std::fs::read(logs::stdout_path(&lease)).expect("file");
    assert_eq!(file, b"hello\n");
    assert!(init::report_path(&lease).is_file());
    assert!(!staged_record_path(&lease).exists());
}

/// Catches limits of the arguments not reaching the capture: a file cap of 4 and a tail of 3
/// keep four bytes in the file, the last three in the record, the whole count, and mark the
/// stream as cut.
#[test]
fn the_limits_in_the_arguments_bound_the_output_kept() {
    let dir = dir("limits");
    let lease = lease_in(&dir);
    let crun = crun(&dir, "echo hello");
    let (code, _) = supervise(&args(&crun, &lease, "4", "3"), one_thread_runtime);
    assert_eq!(code, 0);
    let record = read_exit_record(&lease)
        .expect("readable")
        .expect("written");
    assert_eq!(record.tail.stdout, stream("lo\n", 6, true));
    let file = std::fs::read(logs::stdout_path(&lease)).expect("file");
    assert_eq!(file, b"hell");
}

/// Catches a `crun` that cannot be run, or a runtime that cannot be made, ending without a
/// record: each is a run that failed, recorded with the reason.
#[test]
fn a_run_that_could_not_be_made_is_recorded_as_failed() {
    fn no_runtime() -> std::io::Result<tokio::runtime::Runtime> {
        Err(std::io::Error::other("no epoll"))
    }
    let dir = dir("failed");
    let lease = lease_in(&dir);
    let missing = dir.join("no-such-crun");
    let (code, _) = supervise(&args(&missing, &lease, "100", "100"), one_thread_runtime);
    assert_eq!(code, 0);
    let record = read_exit_record(&lease)
        .expect("readable")
        .expect("written");
    let RunEnd::Failed(why) = record.end else {
        panic!("the run did not fail");
    };
    assert!(why.contains("no-such-crun"), "{why}");

    let lease = dir.join("second");
    std::fs::create_dir_all(lease.join("bundle")).expect("lease");
    let (code, _) = supervise(&args(&missing, &lease, "100", "100"), no_runtime);
    assert_eq!(code, 0);
    let record = read_exit_record(&lease)
        .expect("readable")
        .expect("written");
    assert_eq!(
        record.end,
        RunEnd::Failed("the runtime of the supervisor: no epoll".to_owned())
    );
}

/// Catches arguments the mode cannot read being run: a wrong count, a number that is not
/// one (each of the three), and a name that is not text each exit 2 with no ready word and
/// no file made.
#[test]
fn arguments_that_cannot_be_read_exit_2_without_the_ready_word() {
    let dir = dir("usage");
    let lease = lease_in(&dir);
    let crun = crun(&dir, "exit 0");
    let good = args(&crun, &lease, "100", "100");
    let mut cases: Vec<Vec<OsString>> = vec![Vec::new(), good[..5].to_vec()];
    let mut long = good.clone();
    long.push("extra".into());
    cases.push(long);
    for at in [3, 4, 5] {
        let mut list = good.clone();
        list[at] = "many".into();
        cases.push(list);
    }
    let mut list = good.clone();
    list[2] = OsString::from_vec(vec![0xff, 0xfe]);
    cases.push(list);
    for case in cases {
        let outcome = supervise(&case, one_thread_runtime);
        assert_eq!(outcome, (2, Vec::new()), "{case:?}");
    }
    assert!(!logs::stdout_path(&lease).exists());
}

/// A writer that takes the ready word and cannot flush it.
struct FlushFails;

impl std::io::Write for FlushFails {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("closed"))
    }
}

/// Catches a supervisor that goes on when it could not set up or could not say it is ready:
/// capture files that exist already, a lease directory that is not there, and a parent that
/// has gone away each exit 1 with no ready word where there is none, and run nothing and
/// write no record.
#[test]
fn a_supervisor_that_cannot_make_its_files_or_say_ready_exits_1_and_runs_nothing() {
    let dir = dir("setup");
    let lease = lease_in(&dir);
    let crun = crun(&dir, &format!("touch '{}'", dir.join("ran").display()));
    let arguments = args(&crun, &lease, "100", "100");

    std::fs::write(logs::stdout_path(&lease), b"left").expect("old file");
    assert_eq!(supervise(&arguments, one_thread_runtime), (1, Vec::new()));
    std::fs::remove_file(logs::stdout_path(&lease)).expect("remove");
    std::fs::remove_file(init::report_path(&lease)).expect("remove");

    let absent = args(&crun, &dir.join("absent"), "100", "100");
    assert_eq!(supervise(&absent, one_thread_runtime), (1, Vec::new()));

    let code = run_with(&arguments, &mut FlushFails, one_thread_runtime);
    assert_eq!(code, 1);
    assert!(!dir.join("ran").exists());
    assert_eq!(read_exit_record(&lease), Ok(None));
}

/// Catches a supervisor that reports success when its record is not on disk: with something
/// in the record's place that cannot be replaced, it exits 1.
#[test]
fn a_record_that_cannot_be_written_exits_1() {
    let dir = dir("unwritable");
    let lease = lease_in(&dir);
    let crun = crun(&dir, "exit 0");
    std::fs::create_dir_all(exit_record_path(&lease).join("inner")).expect("blocker");
    let (code, ready) = supervise(&args(&crun, &lease, "100", "100"), one_thread_runtime);
    assert_eq!((code, ready.as_slice()), (1, READY));
}

/// Catches the signals a supervisor must not die of left open: after `run`, on the thread it
/// ran on, `SIGHUP`, `SIGINT` and `SIGTERM` are blocked and another signal is not. (Bad
/// arguments, so nothing else happens; a thread of its own keeps the test harness's mask
/// as it was.)
#[test]
fn the_supervise_mode_blocks_hangup_interrupt_and_terminate() {
    use nix::sys::signal::{SigSet, Signal};
    let (code, blocked, other) = std::thread::spawn(|| {
        let code = run(&[]);
        let mask = SigSet::thread_get_mask().expect("mask");
        let blocked = [Signal::SIGHUP, Signal::SIGINT, Signal::SIGTERM].map(|s| mask.contains(s));
        (code, blocked, mask.contains(Signal::SIGUSR1))
    })
    .join()
    .expect("thread");
    assert_eq!((code, blocked, other), (2, [true; 3], false));
}

/// Asserts that the start of a supervisor fails, with the reason, when its program cannot be
/// run and when it answers with something other than the ready word (the handshake, which
/// `js-supervise` tests in full, then kills and reaps it). It catches a start that returns a
/// handle on a supervisor that never said it is ready.
#[test]
fn a_supervisor_that_cannot_start_or_does_not_answer_fails_the_start() {
    let dir = dir("start");
    let missing = Command::new(dir.join("no-such-program"));
    let why = spawn(missing, dir.to_path_buf(), READY, READY_WAIT).expect_err("not run");
    assert!(why.contains("No such file"), "{why}");
    let mut wrong = Command::new("sh");
    wrong.args(["-c", "printf 'wrong-word!!'"]);
    let why = spawn(wrong, dir.to_path_buf(), READY, READY_WAIT).expect_err("not ready");
    assert_eq!(
        why,
        "the supervisor said something other than its ready word"
    );
}
