//! The supervisor's exit record, the handle that reads it, and the supervise mode run in
//! process with a stand-in `crun` script. No test depends on time: a child that must stay
//! is held on a fifo the test releases, and a wait is for an event (the record, the end of
//! the child).

use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::process::ExitStatusExt as _;

use super::*;

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

fn a_record() -> ExitRecord {
    ExitRecord {
        end: RunEnd::Exited(7),
        tail: LogTail {
            stdout: stream("out\n", 4, false),
            stderr: stream("err\n", 9, true),
        },
        finished_ms: 1234,
    }
}

/// The JSON of `a_record`, to break in a way the test names.
fn valid() -> Value {
    serde_json::from_str(&encode(&a_record())).expect("json")
}

fn text(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

/// Catches a record that is not read back as it was written: every kind of end, every byte
/// value in a tail, the counts and flags of both streams, and a record left whole (the
/// staged file gone, readable by its writer alone).
#[test]
fn a_record_is_written_whole_and_read_back_as_it_was() {
    let lease = dir("round-trip");
    assert_eq!(read_exit_record(&lease), Ok(None));
    let all_bytes: Vec<u8> = (0..=255).collect();
    let records = [
        a_record(),
        ExitRecord {
            end: RunEnd::Exited(-1),
            ..a_record()
        },
        ExitRecord {
            end: RunEnd::Failed("crun run: \"quoted\"\nsecond line \u{e9}".to_owned()),
            tail: LogTail {
                stdout: StreamTail {
                    bytes: all_bytes,
                    total: u64::MAX,
                    file_cut: true,
                },
                stderr: StreamTail::default(),
            },
            finished_ms: u64::MAX,
        },
    ];
    for record in records {
        write_record(&lease, &record).expect("written");
        assert_eq!(read_exit_record(&lease), Ok(Some(record)));
        assert!(!staged_record_path(&lease).exists());
    }
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(exit_record_path(&lease)).expect("meta");
    assert_eq!(mode.permissions().mode() & 0o777, 0o600);
}

/// Catches a reader that is strict where the form must stay open: members it does not know
/// are ignored, so a later form that adds a member is still read.
#[test]
fn members_a_record_adds_later_are_ignored() {
    let mut value = valid();
    value["added"] = json!({ "anything": [1, 2] });
    value["stdout"]["added"] = json!(true);
    assert_eq!(decode(&text(&value)), Ok(a_record()));
}

/// `valid()` with `change` applied, as bytes.
fn broken(change: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value = valid();
    change(&mut value);
    text(&value)
}

/// Takes the member `name` out of the object `value`.
fn remove(value: &mut Value, name: &str) {
    value.as_object_mut().expect("an object").remove(name);
}

/// Catches a document taken for a record that is not one, and a refusal that does not say
/// what is wrong: each damage to the form is its own error naming the member.
#[test]
fn a_document_that_is_not_a_record_is_refused_and_says_why() {
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (b"not json".to_vec(), "not JSON"),
        (b"{}".to_vec(), "the form version is None, not 1"),
        (
            broken(|v| v["version"] = json!(2)),
            "the form version is Some(2), not 1",
        ),
        (
            broken(|v| v["version"] = json!("1")),
            "the form version is None, not 1",
        ),
        (broken(|v| remove(v, "end")), "end is missing"),
        (
            broken(|v| v["end"] = json!({})),
            "end is neither exited nor failed",
        ),
        (
            broken(|v| v["end"] = json!({ "exited": "3" })),
            "end.exited is not an exit status",
        ),
        (
            broken(|v| v["end"] = json!({ "exited": 4_294_967_296_u64 })),
            "end.exited is not an exit status",
        ),
        (
            broken(|v| v["end"] = json!({ "failed": 3 })),
            "end.failed is not text",
        ),
        (
            broken(|v| v["finished_ms"] = json!("soon")),
            "finished_ms is not a number",
        ),
        (
            broken(|v| remove(v, "finished_ms")),
            "finished_ms is missing",
        ),
        (broken(|v| remove(v, "stderr")), "stderr is missing"),
        (
            broken(|v| v["stdout"]["tail"] = json!(5)),
            "stdout.tail is not text",
        ),
        (
            broken(|v| v["stdout"]["tail"] = json!("zz")),
            "lowercase hexadecimal",
        ),
        (
            broken(|v| v["stdout"]["tail"] = json!("AB")),
            "lowercase hexadecimal",
        ),
        (
            broken(|v| v["stdout"]["tail"] = json!("abc")),
            "odd number of digits",
        ),
        (
            broken(|v| v["stdout"]["total"] = json!(-1)),
            "total is not a number",
        ),
        (
            broken(|v| v["stderr"]["file_cut"] = json!("yes")),
            "stderr.file_cut is not true or false",
        ),
        (
            broken(|v| remove(&mut v["stderr"], "tail")),
            "tail is missing",
        ),
    ];
    for (bytes, says) in cases {
        let why = decode(&bytes).expect_err("not a record");
        assert!(why.contains(says), "{says:?} not in {why:?}");
    }
}

/// Catches the hexadecimal of a tail read leniently or written wrongly: digits and letters
/// of both halves of a byte come back, and nothing outside `0-9a-f` is a digit.
#[test]
fn a_tail_is_lowercase_hexadecimal() {
    assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    assert_eq!(unhex("000fa5ff"), Ok(vec![0x00, 0x0f, 0xa5, 0xff]));
    assert_eq!(unhex(""), Ok(Vec::new()));
    for bad in ["g0", "0G", "+f", " f", "0x"] {
        assert!(unhex(bad).is_err(), "{bad:?}");
    }
}

/// Catches a record read when it should be refused: one missing is none yet; a link (even to
/// a good record), a directory and a pipe are not regular files (and a pipe is not waited
/// on); a file over the cap is refused, one at the cap is read as far as the cap allows; each
/// error names the file.
#[test]
fn a_record_file_that_is_not_a_plain_small_file_is_refused() {
    let lease = dir("files");
    let path = exit_record_path(&lease);
    write_record(&lease, &a_record()).expect("written");
    let good = std::fs::read(&path).expect("read");
    std::fs::remove_file(&path).expect("remove");

    std::os::unix::fs::symlink(lease.join("elsewhere"), &path).expect("dangling link");
    let why = read_exit_record(&lease).expect_err("a link");
    assert!(why.starts_with(&format!("{}: ", path.display())), "{why}");
    std::fs::remove_file(&path).expect("remove");
    std::fs::write(lease.join("real"), &good).expect("real");
    std::os::unix::fs::symlink(lease.join("real"), &path).expect("link");
    assert!(read_exit_record(&lease).is_err());
    std::fs::remove_file(&path).expect("remove");

    std::fs::create_dir(&path).expect("directory");
    let why = read_exit_record(&lease).expect_err("a directory");
    assert!(why.contains("not a regular file"), "{why}");
    std::fs::remove_dir(&path).expect("remove");

    let fifo = rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    );
    fifo.expect("fifo");
    let why = read_exit_record(&lease).expect_err("a pipe");
    assert!(why.contains("not a regular file"), "{why}");
    std::fs::remove_file(&path).expect("remove");

    let cap = usize::try_from(MAX_RECORD).expect("cap");
    std::fs::write(&path, vec![b' '; cap + 1]).expect("long");
    let why = read_exit_record(&lease).expect_err("too long");
    assert!(why.contains("longer than 1 MiB"), "{why}");
    std::fs::write(&path, vec![b' '; cap]).expect("at the cap");
    let why = read_exit_record(&lease).expect_err("blank");
    assert!(why.contains("not JSON"), "{why}");
    std::fs::write(&path, &good).expect("good");
    assert_eq!(read_exit_record(&lease), Ok(Some(a_record())));
}

/// Catches the start time read from the wrong field, and a name with spaces or parentheses
/// throwing the count off: the fields are counted from the last closing parenthesis.
#[test]
fn the_state_and_start_time_are_fields_3_and_22_of_a_stat_line() {
    let middle = ["0"; 18].join(" ");
    let line = format!("42 (a b) c) S {middle} 777 8 9\n");
    assert_eq!(parse_stat(&line), Some(('S', 777)));
    for bad in [
        String::new(),
        "42 no parenthesis".to_owned(),
        "42 (a)".to_owned(),
        format!("42 (a) S {}", ["0"; 17].join(" ")),
        format!("42 (a) S {middle} later"),
    ] {
        assert_eq!(parse_stat(&bad), None, "{bad:?}");
    }
}

/// Catches a process taken for the supervisor when it is not: only the same pid with the
/// same start time that is not a zombie is alive; a pid that is gone, a reused pid (another
/// start time) and a zombie are not.
#[test]
fn only_the_process_that_started_then_and_still_runs_is_alive() {
    let me = SupervisorId::of(std::process::id()).expect("own entry");
    assert!(me.alive());
    let reused = SupervisorId {
        start: me.start + 1,
        ..me
    };
    assert!(!reused.alive());
    assert_eq!(SupervisorId::of(u32::MAX), None);
    let nobody = SupervisorId {
        pid: u32::MAX,
        ..me
    };
    assert!(!nobody.alive());

    assert!(names(Some(('S', 5)), 5));
    assert!(!names(Some(('S', 5)), 6));
    assert!(!names(Some(('Z', 5)), 5));
    assert!(!names(None, 5));

    let mut child = Command::new("true").spawn().expect("true");
    let gone = SupervisorId::of(child.id()).expect("entry until reaped");
    child.wait().expect("wait");
    assert!(!gone.alive());
}

/// Catches a kill that reaches the wrong process or none: a live supervisor's whole group is
/// sent `SIGKILL`; a gone id, a reused pid (another start time) and a pid that is no pid
/// signal nothing (this test would die with the group of its own pid otherwise).
#[test]
fn ending_a_supervisor_kills_its_group_and_only_when_it_is_the_same_process() {
    let me = SupervisorId::of(std::process::id()).expect("own entry");
    for other in [
        SupervisorId {
            start: me.start + 1,
            ..me
        },
        SupervisorId { pid: 0, ..me },
        SupervisorId {
            pid: u32::MAX,
            ..me
        },
    ] {
        end_supervisor(other);
    }

    let mut child = Command::new("sh")
        .args(["-c", "read nothing"])
        .stdin(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("sh");
    let id = SupervisorId::of(child.id()).expect("entry");
    assert!(id.alive());
    end_supervisor(id);
    assert_eq!(child.wait().expect("wait").signal(), Some(9));
    assert!(!id.alive());
    end_supervisor(id);
}

/// A supervisor stand-in: `sh` running `script`, with the fifo `hold` as its `$1`.
fn start(lease: &Path, hold: &Path, script: &str) -> Result<Supervised, String> {
    let mut command = Command::new("sh");
    command.args(["-c", script, "sh"]).arg(hold);
    Supervised::spawn(command, lease.to_owned(), READY)
}

/// A fifo in `dir`, for a stand-in to wait on.
fn fifo(dir: &Path) -> PathBuf {
    let path = dir.join("hold");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .expect("fifo");
    path
}

const SAYS_READY: &str = "printf 'supervising\\n'";
const HOLDS: &str = "printf 'supervising\\n'; read nothing < \"$1\"";

/// Catches a handle that reports an end it has not seen or hides one it has: while the
/// supervisor runs without a record there is no end, no tail and no time, and `run_end`
/// says so; with the record there (the supervisor still running, as it is for a moment
/// after it writes it) the end, tail and time are the record's, and a `wait` that was
/// waiting returns.
#[tokio::test]
async fn a_run_has_no_end_until_its_record_is_there() {
    let lease = dir("handle");
    let hold = fifo(&lease);
    let mut run = start(&lease, &hold, HOLDS).expect("started");
    assert!(run.id().alive());
    assert_eq!(run.lease(), &*lease);
    assert_eq!(run.try_end(), None);
    assert_eq!(
        run.run_end(),
        RunEnd::Failed("the run has not ended".to_owned())
    );
    assert_eq!(run.tail(), LogTail::default());
    assert_eq!(run.finished_ms(), None);

    // The record is written, and the stand-in released, while `wait` is already waiting: it
    // opens the fifo to read, which the open for writing meets.
    let release = async {
        write_record(&lease, &a_record()).expect("written");
        let release = std::fs::OpenOptions::new().write(true).open(&hold);
        drop(release.expect("release"));
    };
    tokio::join!(run.wait(), release);
    run.wait().await;
    assert_eq!(run.try_end(), Some(RunEnd::Exited(7)));
    assert_eq!(run.run_end(), RunEnd::Exited(7));
    assert_eq!(run.tail(), a_record().tail);
    assert_eq!(run.finished_ms(), Some(1234));
}

/// Catches a supervisor that died without a record taken for a run still going, and a wait
/// that never ends for it: the run failed, with the reason, and has no tail or time.
#[tokio::test]
async fn a_supervisor_that_is_gone_without_a_record_failed_the_run() {
    let lease = dir("gone");
    let mut run = start(&lease, &lease, SAYS_READY).expect("started");
    run.wait().await;
    assert_eq!(run.try_end(), Some(RunEnd::Failed(GONE.to_owned())));
    assert_eq!(run.tail(), LogTail::default());
    assert_eq!(run.finished_ms(), None);
}

/// Catches a forced end that leaves the supervisor running or does not wait for it: the
/// supervisor held on a fifo is killed, and the run has then failed.
#[tokio::test]
async fn forcing_a_run_ends_its_supervisor_and_waits_for_it() {
    let lease = dir("force");
    let hold = fifo(&lease);
    let mut run = start(&lease, &hold, HOLDS).expect("started");
    run.force().await;
    assert!(!run.id().alive());
    assert_eq!(run.try_end(), Some(RunEnd::Failed(GONE.to_owned())));
}

/// Catches a record that cannot be read taken for no record, or for a good one: the run
/// has ended, as a failure saying the record cannot be read, and has no tail or time.
#[tokio::test]
async fn a_record_that_cannot_be_read_ends_the_run_in_failure() {
    let lease = dir("unreadable");
    std::fs::write(exit_record_path(&lease), b"{").expect("garbage");
    let mut run = start(&lease, &lease, SAYS_READY).expect("started");
    run.wait().await;
    let Some(RunEnd::Failed(why)) = run.try_end() else {
        panic!("the run did not fail");
    };
    assert!(why.starts_with("the exit record cannot be read: "), "{why}");
    assert!(why.contains("not JSON"), "{why}");
    assert_eq!(run.tail(), LogTail::default());
    assert_eq!(run.finished_ms(), None);
}

/// Catches a start that succeeds without the supervisor's word: a child that says
/// something else, says too little and ends, says nothing and ends, or cannot be run, is
/// an error each (and the child that could not say is not left behind).
#[test]
fn a_supervisor_that_does_not_say_it_is_ready_fails_the_start() {
    let lease = dir("silent");
    for (script, says) in [
        ("printf 'wrong-word!!'", "something other than its ready word"),
        ("printf 'sup'", "did not say it is ready"),
        ("true", "did not say it is ready"),
    ] {
        let why = start(&lease, &lease, script).expect_err("not ready");
        assert!(why.contains(says), "{script}: {why}");
    }
    let missing = Command::new(lease.join("no-such-program"));
    let why = Supervised::spawn(missing, lease.to_path_buf(), READY).expect_err("not run");
    assert!(why.contains("No such file"), "{why}");
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
    let record = read_exit_record(&lease).expect("readable").expect("written");
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
    let record = read_exit_record(&lease).expect("readable").expect("written");
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
    let record = read_exit_record(&lease).expect("readable").expect("written");
    let RunEnd::Failed(why) = record.end else {
        panic!("the run did not fail");
    };
    assert!(why.starts_with("crun run: "), "{why}");

    let lease = dir.join("second");
    std::fs::create_dir_all(lease.join("bundle")).expect("lease");
    let (code, _) = supervise(&args(&missing, &lease, "100", "100"), no_runtime);
    assert_eq!(code, 0);
    let record = read_exit_record(&lease).expect("readable").expect("written");
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
