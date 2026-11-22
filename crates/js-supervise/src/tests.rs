//! The supervisor's exit record, the process identity, and the handle that reads the record,
//! with stand-in supervisors started the way `js-contain` starts its own. No test depends on
//! time: a child that must stay is held on a fifo the test releases, and a wait is for an
//! event (the record, the end of the child).

use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Command, Stdio};

use super::*;

fn dir(name: &str) -> js_testkit::dir::TestDir {
    let exe = std::env::current_exe().expect("test binary");
    let root = exe
        .parent()
        .expect("deps directory")
        .join("js-supervise-unit");
    js_testkit::dir::TestDir::under(root, &format!("supervise-{name}"))
}

impl Supervised {
    /// Starts `command` as a supervisor the way `js-contain`'s start does (stdin and stderr
    /// null, stdout a pipe, a process group of its own) and hands it to
    /// [`Supervised::handshake`]: the error is the text the start reports.
    fn spawn(
        mut command: Command,
        lease: PathBuf,
        ready: &[u8],
        limit: Duration,
    ) -> Result<Self, String> {
        let (from_child, to_child) = std::io::pipe().map_err(|e| e.to_string())?;
        command
            .stdin(Stdio::null())
            .stdout(to_child)
            .stderr(Stdio::null())
            .process_group(0);
        let spawned = command.spawn();
        // The command holds the write end of the pipe: closed, the child's exit ends the
        // read.
        drop(command);
        let child = spawned.map_err(|e| e.to_string())?;
        Self::handshake(child, from_child, lease, ready, limit).map_err(|e| e.to_string())
    }
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
    serde_json::from_str(&encode(&a_record(), MAX_TAIL_RECORDED)).expect("json")
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

// rustix has no `mknodat` on macOS: the tests that make a fifo run on Linux.
#[cfg(target_os = "linux")]
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
    assert!(why.contains("longer than an exit record can be"), "{why}");
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
    let lease = dir("end-no-record");
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
        end_supervisor(other, &lease);
    }

    let mut child = Command::new("sh")
        .args(["-c", "read nothing"])
        .stdin(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("sh");
    let id = SupervisorId::of(child.id()).expect("entry");
    assert!(id.alive());
    end_supervisor(id, &lease);
    assert_eq!(child.wait().expect("wait").signal(), Some(9));
    assert!(!id.alive());
    end_supervisor(id, &lease);
}

/// A live process in a group of its own, standing in for a supervisor that is still exiting:
/// `cat`, which exits 0 once its stdin (the returned child's) is closed.
fn exiting_stand_in() -> (std::process::Child, SupervisorId) {
    let child = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("cat");
    let id = SupervisorId::of(child.id()).expect("entry");
    (child, id)
}

/// How the stand-in ended once its stdin is closed: `Ok(code)` if it exited, `Err(signal)`
/// if a signal ended it.
fn close_and_reap(mut child: std::process::Child) -> Result<Option<i32>, i32> {
    drop(child.stdin.take());
    let status = child.wait().expect("wait");
    status.signal().map_or(Ok(status.code()), Err)
}

/// Catches a teardown that kills a supervisor that has finished its run: once the lease holds
/// the exit record the supervisor is only exiting (in a coverage build, writing the profile a
/// kill would cut short), so the live process is not signalled and exits by itself; the next
/// call, once it is gone, signals nothing either.
#[test]
fn a_supervisor_that_wrote_its_exit_record_is_left_to_exit() {
    let lease = dir("end-recorded");
    write_record(&lease, &a_record()).expect("written");
    let (child, id) = exiting_stand_in();
    end_supervisor(id, &lease);
    assert!(id.alive(), "signalled although its record is written");
    assert_eq!(close_and_reap(child), Ok(Some(0)));
    end_supervisor(id, &lease);
}

/// Catches an exit record that cannot be read sparing the supervisor: `exit.json` that is a
/// directory, or a link to a good record, is not a record the reader accepts, so the live
/// supervisor is ended as one with no record.
#[test]
fn an_exit_record_that_cannot_be_read_does_not_spare_the_supervisor() {
    let good = dir("end-good-record");
    write_record(&good, &a_record()).expect("written");
    let as_dir = dir("end-record-dir");
    std::fs::create_dir(exit_record_path(&as_dir)).expect("a directory");
    let as_link = dir("end-record-link");
    std::os::unix::fs::symlink(exit_record_path(&good), exit_record_path(&as_link))
        .expect("a link");
    for lease in [&as_dir, &as_link] {
        let (child, id) = exiting_stand_in();
        end_supervisor(id, lease);
        assert_eq!(close_and_reap(child), Err(9), "{}", lease.display());
    }
}

/// A supervisor stand-in: `sh` running `script`, with the fifo `hold` as its `$1`.
fn start(lease: &Path, hold: &Path, script: &str) -> Result<Supervised, String> {
    let mut command = Command::new("sh");
    command.args(["-c", script, "sh"]).arg(hold);
    Supervised::spawn(command, lease.to_owned(), READY, READY_WAIT)
}

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
const HOLDS: &str = "printf 'supervising\\n'; read nothing < \"$1\"";

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
        (
            "printf 'wrong-word!!'",
            "something other than its ready word",
        ),
        ("printf 'sup'", "did not say it is ready"),
        ("true", "did not say it is ready"),
    ] {
        let why = start(&lease, &lease, script).expect_err("not ready");
        assert!(why.contains(says), "{script}: {why}");
    }
    let missing = Command::new(lease.join("no-such-program"));
    let why =
        Supervised::spawn(missing, lease.to_path_buf(), READY, READY_WAIT).expect_err("not run");
    assert!(why.contains("No such file"), "{why}");
}

/// Catches a supervisor stuck before its ready word blocking the start for good: with no
/// time allowed, a child that never answers fails the start naming the limit, and the child
/// is killed and reaped (the `exec`ed `sleep` would otherwise run for ten minutes).
#[test]
fn a_supervisor_silent_past_the_limit_is_killed_and_the_start_fails() {
    let lease = dir("stuck");
    let mut command = Command::new("sh");
    command.args(["-c", "exec sleep 600"]);
    let why =
        Supervised::spawn(command, lease.to_path_buf(), READY, Duration::ZERO).expect_err("silent");
    assert!(why.contains("not say it is ready within 0ns"), "{why}");
}

/// Catches a tail longer than the bound being recorded whole, which would make a record
/// longer than the reader takes: the record keeps the last bytes of each stream, and a tail
/// within the bound is recorded as it is.
#[test]
fn a_tail_longer_than_the_bound_is_recorded_as_its_last_bytes() {
    let mut record = a_record();
    record.tail.stdout = stream("abcdef", 6, false);
    record.tail.stderr = stream("xyz", 3, true);
    let value: Value = serde_json::from_str(&encode(&record, 4)).expect("json");
    assert_eq!(value["stdout"]["tail"], hex(b"cdef"));
    assert_eq!(value["stderr"]["tail"], hex(b"xyz"));
    assert_eq!(value["stdout"]["total"], 6);
    let whole: Value = serde_json::from_str(&encode(&record, 6)).expect("json");
    assert_eq!(whole["stdout"]["tail"], hex(b"abcdef"));
}

/// A lease directory with a bundle directory in it.
fn lease_in(dir: &Path) -> PathBuf {
    let lease = dir.join("lease");
    std::fs::create_dir_all(lease.join("bundle")).expect("lease");
    lease
}

#[cfg(target_os = "linux")]
/// Catches a lease whose supervisor cannot be named without the ledger, and a supervisor file
/// read through a link, a pipe or a directory: what `note_supervisor` wrote is read back (over
/// what was there), and a file that does not hold a pid and a start time, a link, a pipe and a
/// directory name none. Also catches a process taken to run as a user it does not.
#[test]
fn a_lease_names_its_supervisor_in_a_file_of_its_own() {
    let dir = dir("names");
    let lease = lease_in(&dir);
    let path = supervisor_path(&lease);
    assert_eq!(recorded_supervisor(&lease), None);
    let me = SupervisorId::of(std::process::id()).expect("this process");
    note_supervisor(&lease, me);
    assert_eq!(recorded_supervisor(&lease), Some(me));
    let other = SupervisorId { pid: 1, start: 2 };
    note_supervisor(&lease, other);
    assert_eq!(recorded_supervisor(&lease), Some(other));
    for junk in ["", "12", "x 1", "1 y"] {
        std::fs::write(&path, junk).expect("junk");
        assert_eq!(recorded_supervisor(&lease), None, "{junk:?}");
    }
    std::fs::remove_file(&path).expect("remove");
    let target = dir.join("target");
    std::fs::write(&target, "3 4").expect("target");
    std::os::unix::fs::symlink(&target, &path).expect("link");
    assert_eq!(recorded_supervisor(&lease), None);
    std::fs::remove_file(&path).expect("remove");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .expect("fifo");
    assert_eq!(recorded_supervisor(&lease), None);
    std::fs::remove_file(&path).expect("remove");
    std::fs::create_dir(&path).expect("directory");
    assert_eq!(recorded_supervisor(&lease), None);

    let uid = rustix::process::getuid().as_raw();
    assert!(me.runs_as(uid));
    assert!(!me.runs_as(uid.wrapping_add(1)));
    let gone = SupervisorId {
        pid: u32::MAX,
        start: 0,
    };
    assert!(!gone.runs_as(uid));
}

/// Catches a supervisor taken for another lease's: a process whose command line names the
/// lease where the supervisor's names it is that lease's, and is not another lease's; this
/// process (whose command line names no lease) and a process that is gone supervise none.
#[test]
fn a_supervisor_is_known_by_the_lease_its_command_line_names() {
    let dir = dir("supervises");
    let lease = lease_in(&dir);
    // `sh -c <script> <$0>`: the command line is `sh -c <script> <lease>`, the lease fourth.
    // Right after the spawn the kernel may not have set the new program's arguments yet: the
    // shell's word says it runs, and its command line is set by then.
    let mut child = Command::new("sh")
        .args(["-c", "echo up; read line"])
        .arg(&lease)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("sh");
    let mut word = [0; 3];
    let out = child.stdout.as_mut().expect("piped");
    std::io::Read::read_exact(out, &mut word).expect("the shell runs");
    assert_eq!(&word, b"up\n");
    let id = SupervisorId::of(child.id()).expect("the child");
    assert!(id.supervises(&lease));
    assert!(!id.supervises(&dir.join("another-lease")));
    let me = SupervisorId::of(std::process::id()).expect("this process");
    assert!(!me.supervises(&lease));
    drop(child.stdin.take());
    child.wait().expect("the child ends");
    assert!(!id.supervises(&lease));
}

/// Asserts that each process status of macOS is read as the letter `ps` shows for it, so that
/// a zombie (`SZOMB`, 5) is `Z` as on Linux and no other status is. It catches a macOS
/// identity that takes a zombie for a live supervisor, or a live one for a zombie.
#[test]
fn a_macos_process_status_is_read_as_the_letter_ps_shows() {
    let letters: Vec<char> = (0..=6).map(state_letter).collect();
    assert_eq!(letters, ['?', 'I', 'R', 'S', 'T', 'Z', '?']);
    assert_eq!(state_letter(u32::MAX), '?');
    assert!(!names(Some((state_letter(5), 7)), 7));
    assert!(names(Some((state_letter(2), 7)), 7));
}

/// A `KERN_PROCARGS2` area: the count, the path, `pad` NUL bytes after the path's own, then
/// `words` each ended by a NUL.
fn argument_area(count: i32, path: &str, pad: usize, words: &[&str]) -> Vec<u8> {
    let mut area = count.to_ne_bytes().to_vec();
    area.extend_from_slice(path.as_bytes());
    area.extend(std::iter::repeat_n(0, pad + 1));
    for word in words {
        area.extend_from_slice(word.as_bytes());
        area.push(0);
    }
    area
}

/// The words `arguments` reads from `area`, as text.
fn words_of(area: &[u8]) -> Option<Vec<String>> {
    arguments(area).map(|words| {
        words
            .map(|word| String::from_utf8_lossy(word).into_owned())
            .collect()
    })
}

/// Asserts that the command line is read from a macOS argument area: the count's words after
/// the path and its padding, and not the environment after them; a count larger than the
/// words there yields what is there; a count of 0 yields none. An area too short for the
/// count, one whose path has no end, and a negative count are refused. It catches the
/// executable's path or an environment variable taken for an argument, which would make
/// `supervises` compare the wrong word.
#[test]
fn the_words_of_a_command_line_are_read_from_a_macos_argument_area() {
    let line = [
        "js-agent",
        "__supervise-run",
        "/usr/bin/crun",
        "/lease",
        "name",
    ];
    let mut area = argument_area(5, "/usr/local/bin/js-agent", 3, &line);
    area.extend_from_slice(b"HOME=/var/root\0PATH=/usr/bin\0");
    assert_eq!(words_of(&area), Some(line.map(str::to_owned).to_vec()));
    assert_eq!(
        words_of(&argument_area(0, "/bin/sh", 0, &["sh"])),
        Some(Vec::new())
    );
    assert_eq!(
        words_of(&argument_area(4, "/bin/sh", 0, &["sh", "-c"])),
        Some(vec!["sh".to_owned(), "-c".to_owned(), String::new()])
    );
    assert_eq!(
        words_of(&argument_area(2, "/bin/sh", 0, &[])),
        Some(vec![String::new()])
    );
    assert_eq!(words_of(&[1, 0, 0]), None);
    assert_eq!(words_of(&argument_area(1, "/bin/sh", 0, &[])[..11]), None);
    assert_eq!(words_of(&argument_area(-1, "/bin/sh", 0, &["sh"])), None);
}

/// Asserts that a lease is the fourth word of a supervisor's command line: the same path
/// there names it; the path as another word, another path, and a line of three words do not.
/// It catches a supervisor of another lease, or another program, taken for this lease's.
#[test]
fn a_lease_is_the_fourth_word_of_a_command_line() {
    let lease = Path::new("/state/lease-1");
    let words = |line: &[&'static str]| line.iter().map(|word| word.as_bytes()).collect::<Vec<_>>();
    let named = |line: &[&'static str]| names_lease(words(line).into_iter(), lease);
    assert!(named(&["agent", "mode", "crun", "/state/lease-1", "x"]));
    assert!(named(&["agent", "mode", "crun", "/state/lease-1"]));
    assert!(!named(&["agent", "mode", "/state/lease-1", "crun"]));
    assert!(!named(&["agent", "mode", "crun", "/state/lease-2"]));
    assert!(!named(&["agent", "mode", "crun", "/state/lease-1/"]));
    assert!(!named(&["agent", "mode", "crun"]));
    assert!(!named(&[]));
}

/// Asserts that this process runs as its own user and as no other, and that a pid that is
/// gone runs as nobody. Linux and macOS each read the user their own way, and this is the
/// test of it that runs on both. It catches a teardown that would signal a process of
/// another user.
#[test]
fn a_process_runs_as_its_own_user_and_no_other() {
    let me = SupervisorId::of(std::process::id()).expect("this process");
    let uid = rustix::process::getuid().as_raw();
    assert!(me.runs_as(uid));
    assert!(!me.runs_as(uid.wrapping_add(1)));
    let gone = SupervisorId {
        pid: u32::MAX,
        start: 0,
    };
    assert!(!gone.runs_as(uid));
    assert!(!SupervisorId { pid: 0, ..me }.runs_as(uid));
}

/// Asserts that a supervisor that has ended and is not yet collected (a zombie) is not alive,
/// though its identity was taken while it ran: on Linux `/proc` still shows it, in the state
/// `Z`; macOS's `proc_pidinfo` gives no answer for a zombie, so there it is gone. Once it is
/// collected, no process has its pid. It catches a supervisor that is gone taken for one that
/// runs, which would leave its run waiting for an end that never comes.
#[test]
fn an_ended_supervisor_not_yet_collected_is_not_alive() {
    let (mut child, id) = exiting_stand_in();
    assert!(id.alive());
    drop(child.stdin.take());
    let pid = rustix::process::Pid::from_child(&child);
    let options = rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT;
    rustix::process::waitid(rustix::process::WaitId::Pid(pid), options).expect("it ended");
    assert!(!id.alive());
    let zombie = if cfg!(target_os = "linux") {
        Some(('Z', id.start))
    } else {
        None
    };
    assert_eq!(stat_of(id.pid), zombie);
    child.wait().expect("collected");
    assert_eq!(SupervisorId::of(id.pid), None);
}

/// Asserts that an adopted run is the handle of the supervisor and lease it was given, and
/// that it starts nothing: a supervisor that is gone with no record is a failed run.
#[test]
fn an_adopted_run_reads_the_record_of_the_lease_it_was_given() {
    let lease = dir("adopt");
    let gone = SupervisorId {
        pid: u32::MAX,
        start: 0,
    };
    let run = Supervised::adopt(&lease, gone);
    assert_eq!((run.id(), run.lease()), (gone, &*lease));
    assert_eq!(run.try_end(), Some(RunEnd::Failed(GONE.to_owned())));
    write_record(&lease, &a_record()).expect("written");
    assert_eq!(run.try_end(), Some(RunEnd::Exited(7)));
}

/// Asserts the whole text of each way a handshake fails, which is what a start reports. It
/// catches a reason that changes when the handshake moves or is rewritten.
#[test]
fn each_failed_handshake_says_why_in_full() {
    let cases = [
        (
            HandshakeError::Unnamed,
            "the start time of the supervisor cannot be read",
        ),
        (
            HandshakeError::Silent(Duration::from_secs(30)),
            "the supervisor did not say it is ready within 30s",
        ),
        (
            HandshakeError::Unread(std::io::Error::other("closed early")),
            "the supervisor did not say it is ready: closed early",
        ),
        (
            HandshakeError::WrongWord,
            "the supervisor said something other than its ready word",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}

/// Asserts that after the call, on its thread, `SIGHUP`, `SIGINT` and `SIGTERM` are blocked
/// and another signal is not (a thread of its own keeps the harness's mask as it was). It
/// catches a supervisor that a hangup or a stop of the agent's unit would end.
#[test]
fn the_stop_signals_are_blocked_on_the_calling_thread() {
    use nix::sys::signal::{SigSet, Signal};
    let (blocked, other) = std::thread::spawn(|| {
        block_stop_signals();
        let mask = SigSet::thread_get_mask().expect("mask");
        let blocked = [Signal::SIGHUP, Signal::SIGINT, Signal::SIGTERM].map(|s| mask.contains(s));
        (blocked, mask.contains(Signal::SIGUSR1))
    })
    .join()
    .expect("thread");
    assert_eq!((blocked, other), ([true; 3], false));
}
