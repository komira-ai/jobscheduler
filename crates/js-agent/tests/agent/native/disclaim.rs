//! Every process started for a job answers to the privacy controls (TCC) for itself, never
//! for the helper or the agent that started it, so no grant of theirs reaches it
//! (`js_mac_session::disclaim`). Each test reads the responsible process back through the
//! system's own interface while the process is still running: a `cat` that waits for the end
//! of a pipe that the test holds open, so nothing races a clock.
//!
//! The hosted runner has no grant to give, so these tests check the disclaim, not a read
//! that a grant would have allowed.

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use js_mac_session::disclaim::{Program, responsible_pid};
use js_mac_session::proto::Session;
use js_mac_session::spawn::{self, Spawn};

use crate::support::Scratch;

/// A pipe: the end a `cat` reads, and the end the test holds until it has looked.
fn pipe() -> (OwnedFd, std::io::PipeWriter) {
    let (read, write) = std::io::pipe().unwrap();
    (read.into(), write)
}

fn null() -> OwnedFd {
    std::fs::File::open("/dev/null").unwrap().into()
}

/// Starts `argv` the way the helper's `run` does, as the user running the tests, in the
/// background session, reading `stdin`.
fn run(dir: &Scratch, argv: &[&str], stdin: OwnedFd) -> Child {
    run_in(dir, argv, &[], stdin, null())
}

/// [`run`] with the environment `env` asked for, writing to `stdout`.
fn run_in(
    dir: &Scratch,
    argv: &[&str],
    env: &[(&str, &str)],
    stdin: OwnedFd,
    stdout: OwnedFd,
) -> Child {
    spawn::spawn(Spawn {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        user: "kbf-lease-7-1".to_owned(),
        home: PathBuf::from("/var/empty"),
        argv: argv.iter().map(|&arg| arg.to_owned()).collect(),
        env: env
            .iter()
            .map(|&(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        stdin,
        stdout,
        stderr: null(),
        cwd: std::fs::File::open(dir.path("")).unwrap().into(),
        session: Session::Background,
        limits: None,
    })
    .unwrap()
}

/// Ends `child` by closing its input, and checks that it ran to the end.
fn finish(mut child: Child, mut feed: std::io::PipeWriter) {
    feed.write_all(b"done\n").unwrap();
    drop(feed);
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
}

/// A job's process, started as the helper starts every job, is its own responsible
/// process. It catches a start that does not disclaim the responsibility: the job would
/// answer for the helper, and use any grant the helper was given.
#[test]
fn spawned_job_has_responsibility_disclaimed() {
    let dir = Scratch::new("disclaim-job");
    let (stdin, feed) = pipe();
    let child = run(&dir, &["/bin/cat"], stdin);
    let pid = child.id();
    assert_eq!(
        responsible_pid(pid).unwrap(),
        pid,
        "the job answers for its starter"
    );
    finish(child, feed);
}

/// A native job runs as `sandbox-exec ... <program>`: `sandbox-exec` executes the program
/// in its own place, so the program stays its own responsible process. It catches a start
/// whose disclaim is lost on the way from `sandbox-exec` to the job's program.
#[test]
fn a_job_under_sandbox_exec_answers_for_itself() {
    let dir = Scratch::new("disclaim-sandbox");
    let (stdin, feed) = pipe();
    let argv = [
        "/usr/bin/sandbox-exec",
        "-p",
        "(version 1) (allow default)",
        "/bin/cat",
    ];
    let child = run(&dir, &argv, stdin);
    let pid = child.id();
    assert_eq!(responsible_pid(pid).unwrap(), pid);
    finish(child, feed);
}

/// A program the agent starts itself (a probe, the `sandbox-exec` self-test) is its own
/// responsible process too. It catches a [`Program`] that disclaims only when it is given
/// steps, as the helper's runs are.
#[test]
fn a_program_the_agent_starts_answers_for_itself() {
    let (stdin, feed) = pipe();
    let child = Program::new("/bin/cat", [""; 0])
        .stdin(stdin)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    assert_eq!(responsible_pid(pid).unwrap(), pid);
    finish(child, feed);
}

/// The read-back tells the two apart: a process started the usual way answers for the
/// process that started it (this test's own responsible process), not for itself. Without
/// this, a read-back that always named the process itself would pass the tests above.
#[test]
fn a_process_started_the_usual_way_answers_for_its_starter() {
    let (stdin, feed) = pipe();
    let child = Command::new("/bin/cat")
        .stdin(stdin)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mine = responsible_pid(std::process::id()).unwrap();
    assert_eq!(responsible_pid(pid).unwrap(), mine);
    assert_ne!(mine, pid);
    finish(child, feed);
}

/// The disclaimed execution gets the environment the helper built for the job, and finds a
/// program named without a directory on that environment's `PATH`, as `execvp` would. It
/// catches an execution that passes the helper's (or the agent's) own environment to the
/// job, which would leak the daemon's variables into user code, or one that looks the
/// program up on the daemon's `PATH`.
#[test]
fn a_disclaimed_job_gets_exactly_its_own_environment() {
    let dir = Scratch::new("disclaim-env");
    let (out, out_end) = std::io::pipe().unwrap();
    // `env` with no directory: found through the job's `PATH`, not the test's.
    let env = [("PATH", "/usr/bin"), ("KEEP", "kept")];
    let mut child = run_in(&dir, &["env"], &env, null(), out_end.into());
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
    let mut printed = String::new();
    std::io::Read::read_to_string(&mut { out }, &mut printed).unwrap();
    let mut lines: Vec<&str> = printed.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "HOME=/var/empty",
            "KEEP=kept",
            "LOGNAME=kbf-lease-7-1",
            "PATH=/usr/bin",
            "USER=kbf-lease-7-1",
        ]
    );
}
