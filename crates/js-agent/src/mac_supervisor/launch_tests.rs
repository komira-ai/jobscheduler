//! The agent's side: waiting for the ready record or the end over a supervisor that is a fake,
//! with counted pauses (never timed), and the real launcher's start of its program.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use js_testkit::dir::TestDir;

use super::{LaunchRequest, Launcher, Supervised, Supervision, SupervisorChild, SystemLauncher};
use crate::mac_supervisor::record::{EXIT, Ended, ExitRecord, READY, Ready, SPEC, Step};
use crate::mac_supervisor::spec::tests::spec;

/// A supervisor that answers each look with the next of its answers (running, when it has
/// none left), and may write a file as it reports its end: the record written between the
/// agent's look at the file and its look at the process.
struct FakeChild {
    answers: VecDeque<io::Result<Option<ExitStatus>>>,
    at_end: Option<(PathBuf, String)>,
}

impl FakeChild {
    /// A supervisor that runs until it has given `answers`.
    fn new(answers: impl IntoIterator<Item = io::Result<Option<ExitStatus>>>) -> Self {
        Self {
            answers: answers.into_iter().collect(),
            at_end: None,
        }
    }
}

impl SupervisorChild for FakeChild {
    fn pid(&self) -> u32 {
        999
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let answer = self.answers.pop_front().unwrap_or(Ok(None));
        if matches!(answer, Ok(Some(_)))
            && let Some((path, text)) = self.at_end.take()
        {
            fs::write(path, text).unwrap();
        }
        answer
    }
}

/// An exit status that exited with `code`.
fn exited(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

/// A record directory with the two empty files the supervisor makes first.
fn records(name: &str) -> TestDir {
    let dir = TestDir::under(
        std::env::temp_dir(),
        &format!("mac-supervisor-launch-{name}"),
    );
    for file in [READY, EXIT] {
        fs::write(dir.join(file), "").unwrap();
    }
    dir
}

/// A supervision of `child` over `dir`, with a pause that counts itself and, at the pause
/// numbered `write_at`, writes the ready record of job 4321 in session 77.
fn watched(
    dir: &Path,
    child: FakeChild,
    write_at: Option<usize>,
) -> (Supervision, Arc<AtomicUsize>) {
    let pauses = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&pauses);
    let ready = dir.join(READY);
    let pause = move || {
        let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
        if Some(n) == write_at {
            fs::write(&ready, r#"{"job_pid":4321,"asid":77}"#).unwrap();
        }
    };
    let supervision = Supervision::new(Box::new(child), dir.to_path_buf(), Box::new(pause));
    (supervision, pauses)
}

/// The exit record text of a start that failed at the profile.
fn profile_refused() -> (ExitRecord, String) {
    let record = ExitRecord {
        ended: Ended::SetupFailed {
            step: Step::Profile,
            why: "refused".to_owned(),
        },
        job_pid: None,
        asid: None,
    };
    let text = serde_json::to_string(&record).unwrap();
    (record, text)
}

/// The ready record appears after three counted pauses (the empty file the supervisor made
/// first reads as not written), and `wait_ready` returns it; a supervisor that ends first with
/// an exit record gives that record's end; one that ends with none and status 125 gives a
/// failure of the records, one killed by a signal `Signalled`; a ready record written just
/// before the end is still found; a supervisor that cannot be looked at ends the wait. Catches
/// an agent that waits forever on a supervisor that has ended, or reads its end wrongly.
#[test]
fn the_agent_side_waits_for_ready_or_the_end() {
    let want = Ready {
        job_pid: 4321,
        asid: Some(77),
    };

    let dir = records("ready");
    let (mut run, pauses) = watched(&dir, FakeChild::new([]), Some(3));
    assert_eq!(run.wait_ready(), Ok(want));
    assert_eq!(pauses.load(Ordering::SeqCst), 3);
    assert_eq!((run.pid(), run.record_dir()), (999, dir.path()));

    let dir = records("ended");
    let (record, text) = profile_refused();
    fs::write(dir.join(EXIT), text).unwrap();
    let child = FakeChild::new([Ok(None), Ok(Some(exited(125)))]);
    let (mut run, pauses) = watched(&dir, child, None);
    assert_eq!(run.wait_ready(), Err(record.ended));
    assert_eq!(pauses.load(Ordering::SeqCst), 1);

    let dir = records("no-record");
    let (mut run, _) = watched(&dir, FakeChild::new([Ok(Some(exited(125)))]), None);
    let ended = run.wait_ready().unwrap_err();
    assert!(
        matches!(
            ended,
            Ended::SetupFailed {
                step: Step::Records,
                ..
            }
        ),
        "{ended:?}"
    );

    let dir = records("killed");
    let killed = ExitStatus::from_raw(9);
    let (mut run, _) = watched(&dir, FakeChild::new([Ok(Some(killed))]), None);
    assert_eq!(run.wait_ready(), Err(Ended::Signalled(9)));

    let dir = records("late-ready");
    let mut child = FakeChild::new([Ok(Some(exited(0)))]);
    child.at_end = Some((dir.join(READY), r#"{"job_pid":4321,"asid":77}"#.to_owned()));
    let (mut run, pauses) = watched(&dir, child, None);
    assert_eq!(run.wait_ready(), Ok(want));
    assert_eq!(pauses.load(Ordering::SeqCst), 0);

    let dir = records("unlooked");
    let child = FakeChild::new([Err(io::Error::other("gone"))]);
    let (mut run, _) = watched(&dir, child, None);
    assert_eq!(
        run.wait_ready(),
        Err(Ended::SetupFailed {
            step: Step::Records,
            why: "looking at the supervisor: gone".to_owned()
        })
    );
}

/// `try_exit` never waits (it never pauses, and answers `None` while the fake supervisor runs);
/// it returns the exit record once written; a supervisor that ends after its ready record with
/// no exit record gives a record made from its status that keeps the job's pid and session; an
/// exit record written just before the end is the one returned; a supervisor that cannot be
/// looked at is an error. Catches a poll that blocks the agent, or that loses the job's pid.
#[test]
fn try_exit_answers_at_once() {
    let dir = records("running");
    let (mut run, pauses) = watched(&dir, FakeChild::new([]), None);
    assert_eq!(run.try_exit().unwrap(), None);
    assert_eq!(run.try_exit().unwrap(), None);
    let (record, text) = profile_refused();
    fs::write(dir.join(EXIT), &text).unwrap();
    assert_eq!(run.try_exit().unwrap(), Some(record.clone()));
    assert_eq!(pauses.load(Ordering::SeqCst), 0);

    let dir = records("killed-after-ready");
    let killed = ExitStatus::from_raw(9);
    let (mut run, _) = watched(&dir, FakeChild::new([Ok(None), Ok(Some(killed))]), Some(1));
    assert!(run.wait_ready().is_ok());
    assert_eq!(
        run.try_exit().unwrap(),
        Some(ExitRecord {
            ended: Ended::Signalled(9),
            job_pid: Some(4321),
            asid: Some(77),
        })
    );

    let dir = records("late-exit");
    let mut child = FakeChild::new([Ok(Some(exited(125)))]);
    child.at_end = Some((dir.join(EXIT), text));
    let (mut run, _) = watched(&dir, child, None);
    assert_eq!(run.try_exit().unwrap(), Some(record));

    let dir = records("unlooked");
    let child = FakeChild::new([Err(io::Error::other("gone"))]);
    let (mut run, _) = watched(&dir, child, None);
    assert_eq!(run.try_exit().unwrap_err().to_string(), "gone");
}

/// The program the real launcher starts, standing in for the agent's own: a shell given the
/// verb's command line reads its first word as a script in the working directory, which here
/// records what it was started with and exits 3 with no record.
const STAND_IN: &str = r#"printf '%s\n' "$0" "$@" > seen
env > env
pwd > cwd
echo out
echo err >&2
exit 3
"#;

/// The real launcher writes the spec, mode 0600, to the record directory and starts its
/// program with `__supervise-native --spec <that file>`, in the working directory, with the
/// three streams and exactly the environment given (none of this process's); with no record,
/// the wait ends with the program's status. Catches a supervisor that inherits the agent's own
/// environment, or a spec another user could read.
#[test]
fn the_system_launcher_writes_the_spec_and_starts_its_program() {
    let dir = TestDir::under(std::env::temp_dir(), "mac-supervisor-system-launcher");
    let (records, cwd) = (dir.join("records"), dir.join("work"));
    fs::create_dir(&records).unwrap();
    fs::create_dir(&cwd).unwrap();
    fs::write(cwd.join(super::VERB), STAND_IN).unwrap();
    let cwd = fs::canonicalize(&cwd).unwrap();
    let (out, err) = (dir.join("out"), dir.join("err"));
    let env = [
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ("JS_MARK".to_owned(), "marked".to_owned()),
    ];
    let want = spec(&records);
    let request = LaunchRequest {
        spec: want.clone(),
        env: &env,
        cwd: &cwd,
        stdio: [
            Stdio::null(),
            fs::File::create(&out).unwrap().into(),
            fs::File::create(&err).unwrap().into(),
        ],
    };
    let mut run = SystemLauncher::at("/bin/sh").launch(request).unwrap();
    assert!(run.pid() > 0);
    assert_eq!(run.wait_ready(), Err(Ended::Exited(3)));
    let spec_file = records.join(SPEC);
    assert_eq!(
        fs::metadata(&spec_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let written: crate::mac_supervisor::SuperviseSpec =
        serde_json::from_str(&fs::read_to_string(&spec_file).unwrap()).unwrap();
    assert_eq!(written, want);
    let read = |name: &str| fs::read_to_string(cwd.join(name)).unwrap();
    assert_eq!(
        read("seen"),
        format!("{}\n--spec\n{}\n", super::VERB, spec_file.display())
    );
    let seen = read("env");
    assert!(seen.lines().any(|line| line == "JS_MARK=marked"), "{seen}");
    assert!(!seen.contains("CARGO"), "{seen}");
    assert_eq!(read("cwd"), format!("{}\n", cwd.display()));
    assert_eq!(fs::read_to_string(&out).unwrap(), "out\n");
    assert_eq!(fs::read_to_string(&err).unwrap(), "err\n");
    assert_eq!(run.record_dir(), records);
}

/// A program that cannot be started leaves no spec behind; a spec file already in the record
/// directory is not overwritten and stops the launch; the launcher of this program names an
/// absolute path; the real pause returns. Catches a profile text left on disk by a start that
/// never happened, and an old spec taken for a new one.
#[test]
fn a_launch_that_cannot_start_leaves_no_spec() {
    let dir = TestDir::under(std::env::temp_dir(), "mac-supervisor-launch-refused");
    let request = |stdio| LaunchRequest {
        spec: spec(dir.path()),
        env: &[],
        cwd: dir.path(),
        stdio,
    };
    let streams = || [Stdio::null(), Stdio::null(), Stdio::null()];
    let absent = SystemLauncher::at(dir.join("absent"));
    assert!(absent.launch(request(streams())).is_err());
    assert!(!dir.join(SPEC).exists());

    fs::write(dir.join(SPEC), "old").unwrap();
    let refused = absent.launch(request(streams())).err().unwrap();
    assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(dir.join(SPEC)).unwrap(), "old");

    assert!(SystemLauncher::current().unwrap().exe.is_absolute());
    super::pause();
}
