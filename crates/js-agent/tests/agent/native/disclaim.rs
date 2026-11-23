//! Which process each program the agent and the helper start answers to for the privacy
//! controls (TCC), and so whose grants it holds (`js_mac_session::disclaim`). A start is
//! disclaimed unless it keeps the responsibility: then the program answers for the process its
//! starter answers for. Two starts keep it, a native job's supervisor and the job's program,
//! so that a job answers for the agent and holds its Full Disk Access; every other program
//! the agent starts (a probe, `pfctl`), and every job the helper starts, answers for itself.
//!
//! Each test reads the responsible process back through the system's own interface while the
//! process runs: the program says on a pipe that it is running, and waits for a line on its
//! input, which the test sends only after it has looked, so nothing races a clock. The test
//! process stands in for the agent: what answers for the agent answers for the test's own
//! responsible process (the agent, a launchd daemon, is its own).
//!
//! The hosted runner has no grant to give, so these tests check which process answers, not a
//! read that a grant would have allowed. They also check that the disclaimed execution finds
//! and runs the program as `execvp` would (the macOS start no longer goes through it), in the
//! job's own environment and working directory.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, PipeReader, PipeWriter, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use js_mac_provision::pfctl::{Pfctl as _, SystemPfctl};
use js_mac_session::disclaim::{Kept, Program, responsible_pid};
use js_mac_session::proto::Session;
use js_mac_session::spawn::{self, Spawn};

use crate::support::Scratch;

/// `/dev/null`, open for reading and writing: a stream either way.
fn null() -> OwnedFd {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .unwrap()
        .into()
}

/// A running job: what it prints, line by line, and its input.
struct Job {
    child: Child,
    out: BufReader<PipeReader>,
    feed: PipeWriter,
}

impl Job {
    /// The next line the job printed, without its newline; the job ending first fails the
    /// test.
    fn line(&mut self) -> String {
        let mut line = String::new();
        let read = self.out.read_line(&mut line).unwrap();
        assert_ne!(read, 0, "the job ended before it printed a line");
        line.trim_end().to_owned()
    }

    /// Ends the job with a line on its input, and checks that it ran to the end.
    fn finish(mut self) {
        self.feed.write_all(b"done\n").unwrap();
        drop(self.feed);
        let status = self.child.wait().unwrap();
        assert!(status.success(), "{status}");
    }
}

/// Starts `argv` the way the helper's `run` does, as the user running the tests, in the
/// background session, with the environment `env` asked for and `cwd` as the lease
/// directory.
fn start(cwd: &Path, argv: &[&str], env: &[(&str, &str)]) -> std::io::Result<Job> {
    let (stdin, feed) = std::io::pipe().unwrap();
    let (out, out_end) = std::io::pipe().unwrap();
    let child = spawn::spawn(Spawn {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        user: "kbf-lease-7-1".to_owned(),
        home: PathBuf::from("/var/empty"),
        argv: argv.iter().map(|&arg| arg.to_owned()).collect(),
        env: env
            .iter()
            .map(|&(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        stdin: stdin.into(),
        stdout: out_end.into(),
        stderr: null(),
        cwd: std::fs::File::open(cwd).unwrap().into(),
        session: Session::Background,
        limits: None,
    })?;
    Ok(Job {
        child,
        out: BufReader::new(out),
        feed,
    })
}

/// A shell that says `ready` and its pid, then waits for a line on its input.
const READY: [&str; 3] = ["/bin/sh", "-c", "echo ready $$; read line"];

/// Writes an executable file `name` in `dir` holding `text`.
fn script(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// What a finished job printed.
fn printed(mut job: Job) -> String {
    drop(job.feed);
    let status = job.child.wait().unwrap();
    assert!(status.success(), "{status}");
    let mut text = String::new();
    job.out.read_to_string(&mut text).unwrap();
    text
}

/// A job's process, started as the helper starts every job, is its own responsible
/// process, read once its program has printed: the helper's start is not one of the two that
/// keep the responsibility. It catches a helper start that keeps it: the job would answer for
/// the helper, not for the agent whose grant a job is to hold.
#[test]
fn a_job_the_helper_starts_answers_for_itself() {
    let dir = Scratch::new("disclaim-job");
    let mut job = start(&dir.path(""), &READY, &[]).unwrap();
    let pid = job.child.id();
    assert_eq!(job.line(), format!("ready {pid}"));
    assert_eq!(
        responsible_pid(pid).unwrap(),
        pid,
        "the job answers for its starter"
    );
    job.finish();
}

/// A native job the helper starts runs as `sandbox-exec ... <program>`: once the program
/// itself has printed (so `sandbox-exec` has executed it), it is still its own responsible
/// process. It catches a disclaim lost on the way from `sandbox-exec` to the job's program.
#[test]
fn a_helper_job_under_sandbox_exec_answers_for_itself() {
    let dir = Scratch::new("disclaim-sandbox");
    let argv = [
        "/usr/bin/sandbox-exec",
        "-p",
        "(version 1) (allow default)",
        READY[0],
        READY[1],
        READY[2],
    ];
    let mut job = start(&dir.path(""), &argv, &[]).unwrap();
    let pid = job.child.id();
    assert_eq!(
        job.line(),
        format!("ready {pid}"),
        "the program has not run"
    );
    assert_eq!(responsible_pid(pid).unwrap(), pid);
    job.finish();
}

/// What a job the helper starts starts the usual way, a child and a grandchild, answers for
/// the job, not for the helper: a process's children follow it. It catches a disclaim that
/// the job's own processes escape (they would answer for the helper again).
#[test]
fn a_helper_jobs_children_answer_for_the_job() {
    let dir = Scratch::new("disclaim-descendants");
    // Each shell ends with `:` so that none executes its last command in its own place.
    let script =
        r#"echo job $$; /bin/sh -c 'echo child $$; /bin/sh -c "echo grand \$\$; read line"; :'; :"#;
    let mut job = start(&dir.path(""), &["/bin/sh", "-c", script], &[]).unwrap();
    let pid = job.child.id();
    assert_eq!(job.line(), format!("job {pid}"));
    let child = job.line();
    let grand = job.line();
    for (name, line) in [("child", child), ("grand", grand)] {
        let theirs: u32 = line
            .strip_prefix(&format!("{name} "))
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(theirs, pid, "{name} is the job itself");
        assert_eq!(responsible_pid(theirs).unwrap(), pid, "{name}");
    }
    job.finish();
}

/// A program the agent starts itself (a probe, the `sandbox-exec` self-test) is its own
/// responsible process too: it does not keep the responsibility. It catches a [`Program`]
/// that disclaims only when it is given steps, as the helper's runs are, or one that keeps
/// the responsibility by default.
#[test]
fn a_program_the_agent_starts_answers_for_itself() {
    let (stdin, mut feed) = std::io::pipe().unwrap();
    let (out, out_end) = std::io::pipe().unwrap();
    let mut child = Program::new(READY[0], &READY[1..])
        .stdin(stdin)
        .stdout(out_end)
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut line = String::new();
    BufReader::new(out).read_line(&mut line).unwrap();
    assert_eq!(line, format!("ready {pid}\n"));
    assert_eq!(responsible_pid(pid).unwrap(), pid);
    feed.write_all(b"done\n").unwrap();
    assert!(child.wait().unwrap().success());
}

/// The agent's `pfctl`, which js-mac-provision starts for the agent, is started disclaimed (a
/// firewall tool given fixed arguments needs no grant):
/// a stand-in started through `SystemPfctl` sends its pid through a named pipe and waits on
/// another while the test looks. It catches a program the agent starts through a dependency
/// that bypasses the spawner.
#[test]
fn the_agents_pfctl_answers_for_itself() {
    let dir = Scratch::new("disclaim-pfctl");
    let (pids, go) = (dir.path("pid"), dir.path("go"));
    for fifo in [&pids, &go] {
        let made = Command::new("/usr/bin/mkfifo").arg(fifo).status().unwrap();
        assert!(made.success());
    }
    let (pids_arg, go_arg) = (pids.to_str().unwrap(), go.to_str().unwrap());
    // A pipe opened for reading and writing never waits for the other end, so nothing here
    // blocks for good when the stand-in does not start: the starter thread then writes
    // `end` where the pid would have been.
    let open = |fifo: &Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(fifo)
            .unwrap()
    };
    let (said, ran) = std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            let script = r#"echo $$ > "$1"; read line < "$2""#;
            let ran = SystemPfctl::new("/bin/sh").run(&["-c", script, "sh", pids_arg, go_arg]);
            open(&pids).write_all(b"end\n").unwrap();
            ran
        });
        let mut said = String::new();
        BufReader::new(std::fs::File::open(&pids).unwrap())
            .read_line(&mut said)
            .unwrap();
        let answer = said
            .trim()
            .parse::<u32>()
            .map(|pid| (pid, responsible_pid(pid)));
        // Held open until the stand-in has ended: a pipe whose ends are all closed drops
        // what was written, and the stand-in may open it only after this write.
        let mut go_end = open(&go);
        go_end.write_all(b"go\n").unwrap();
        let ran = run.join().unwrap();
        drop(go_end);
        (answer, ran)
    });
    assert_eq!(ran.unwrap().code, Some(0));
    let (pid, responsible) = said.unwrap();
    assert_eq!(responsible.unwrap(), pid);
}

/// The responsible process of this test process: what a program that keeps its starter's
/// responsibility answers for, as a job answers for the agent.
fn starters() -> u32 {
    responsible_pid(std::process::id()).unwrap()
}

/// Starts [`READY`] through a [`Program`], keeping the responsibility for `kept`, and waits
/// until it has said it is ready; returns it and its pid.
fn ready(kept: Option<Kept>) -> (Job, u32) {
    let (stdin, feed) = std::io::pipe().unwrap();
    let (out, out_end) = std::io::pipe().unwrap();
    let mut program = Program::new(READY[0], &READY[1..]);
    program.stdin(stdin).stdout(out_end);
    if let Some(why) = kept {
        program.keep_responsibility(why);
    }
    let child = program.spawn().unwrap();
    // The program holds the pipes' other ends until it is dropped: dropped here, a job that
    // ends early is read as the end of its output, not waited for.
    drop(program);
    let mut job = Job {
        child,
        out: BufReader::new(out),
        feed,
    };
    let pid = job.child.id();
    assert_eq!(job.line(), format!("ready {pid}"));
    (job, pid)
}

/// A job's program started keeping the responsibility (as a supervisor starts it) answers
/// for its starter's responsible process, not for itself: it holds what the agent holds. It
/// catches a keep that still disclaims, under which a job is judged by its own signature and
/// refused the network file system mount its build trees are served from.
#[test]
fn a_job_that_keeps_its_responsibility_answers_for_its_starter() {
    let (job, pid) = ready(Some(Kept::Job));
    let mine = starters();
    assert_ne!(mine, pid);
    assert_eq!(
        responsible_pid(pid).unwrap(),
        mine,
        "the job answers for itself"
    );
    job.finish();
}

/// What a job started keeping the responsibility starts the usual way, a child and a
/// grandchild, answers for the job's starter too: a compiler or a shell the job runs holds the
/// same grants as the job. It catches a keep that only the job's first process gets.
#[test]
fn a_kept_jobs_children_and_grandchildren_answer_for_its_starter() {
    // Each shell ends with `:` so that none executes its last command in its own place.
    let script =
        r#"echo job $$; /bin/sh -c 'echo child $$; /bin/sh -c "echo grand \$\$; read line"; :'; :"#;
    let (stdin, feed) = std::io::pipe().unwrap();
    let (out, out_end) = std::io::pipe().unwrap();
    let mut job = Job {
        child: Program::new("/bin/sh", ["-c", script])
            .keep_responsibility(Kept::Job)
            .stdin(stdin)
            .stdout(out_end)
            .spawn()
            .unwrap(),
        out: BufReader::new(out),
        feed,
    };
    let pid = job.child.id();
    assert_eq!(job.line(), format!("job {pid}"));
    let mine = starters();
    for name in ["child", "grand"] {
        let line = job.line();
        let theirs: u32 = line
            .strip_prefix(&format!("{name} "))
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(theirs, pid, "{name} is the job itself");
        assert_eq!(responsible_pid(theirs).unwrap(), mine, "{name}");
    }
    assert_eq!(responsible_pid(pid).unwrap(), mine, "the job");
    job.finish();
}

/// How the stand-in supervisor starts its job: `kept` or `disclaimed`, from the test that
/// starts the stand-in.
const STAND_IN_JOB: &str = "JS_DISCLAIM_TEST_STAND_IN_JOB";

/// The stand-in supervisor the chain tests start in a process of its own, as the agent starts
/// a native job's supervisor: it says its pid, then starts [`READY`] as its job through a
/// [`Program`], keeping the responsibility when the parent asks for `kept`, with its own
/// input and output, and waits for it. Ignored in a plain run: it needs its parent.
#[test]
#[ignore = "run by the chain tests as a stand-in supervisor, in a process of its own"]
fn stand_in_supervisor_starts_its_job() {
    let how = std::env::var(STAND_IN_JOB).expect("the parent test says how to start the job");
    let mut job = Program::new(READY[0], &READY[1..]);
    if how == "kept" {
        job.keep_responsibility(Kept::Job);
    } else {
        assert_eq!(how, "disclaimed", "no way to start a job called so");
    }
    println!("supervisor {}", std::process::id());
    let status = job.spawn().unwrap().wait().unwrap();
    assert!(status.success(), "{status}");
}

/// What a chain of starts reads: the pid of the supervisor and of its job, and the responsible
/// process of each.
struct Chain {
    supervisor: u32,
    job: u32,
    supervisor_answers_for: u32,
    job_answers_for: u32,
}

/// Starts the stand-in supervisor, keeping the responsibility for `supervisor`, with its job
/// started `job` (`kept` or `disclaimed`), reads both answers while the job waits, then ends
/// the job and checks that the supervisor ran to the end.
fn chain(supervisor: Option<Kept>, job: &str) -> Chain {
    let (stdin, mut feed) = std::io::pipe().unwrap();
    let (out, out_end) = std::io::pipe().unwrap();
    let test = std::env::current_exe().unwrap();
    let mut program = Program::new(
        test,
        [
            "--exact",
            "native::disclaim::stand_in_supervisor_starts_its_job",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ],
    );
    program.env(STAND_IN_JOB, job).stdin(stdin).stdout(out_end);
    if let Some(why) = supervisor {
        program.keep_responsibility(why);
    }
    let mut started = program.spawn().unwrap();
    // The program holds the pipes' other ends until it is dropped: dropped here, a stand-in
    // that ends early is read as the end of its output, not waited for.
    drop(program);
    let mut out = BufReader::new(out);
    // libtest's own `test <name> ... ` may come first on the line a pid is on.
    let (mut said_supervisor, mut said_job) = (None, None);
    while said_supervisor.is_none() || said_job.is_none() {
        let mut line = String::new();
        assert_ne!(out.read_line(&mut line).unwrap(), 0, "the stand-in ended");
        let pid = |word: &str| {
            line.split_once(word)
                .map(|(_, pid)| pid.trim().parse::<u32>().unwrap())
        };
        said_supervisor = said_supervisor.or(pid("supervisor "));
        said_job = said_job.or(pid("ready "));
    }
    let (supervisor, job) = (said_supervisor.unwrap(), said_job.unwrap());
    let read = Chain {
        supervisor,
        job,
        supervisor_answers_for: responsible_pid(supervisor).unwrap(),
        job_answers_for: responsible_pid(job).unwrap(),
    };
    feed.write_all(b"done\n").unwrap();
    drop(feed);
    let status = started.wait().unwrap();
    assert!(status.success(), "{status}");
    assert_eq!(supervisor, started.id());
    read
}

/// The chain the agent runs a native job by: the agent starts the supervisor keeping the
/// responsibility, and the supervisor starts the job keeping it, so the job answers for the
/// agent (here, for the test's own responsible process), and so does the supervisor. It
/// catches a supervisor or a job start that disclaims: the job would answer for the
/// supervisor or for itself, and lose the agent's grant.
#[test]
fn a_job_started_by_a_kept_supervisor_answers_for_the_agent() {
    let read = chain(Some(Kept::Supervisor), "kept");
    let mine = starters();
    assert_ne!(read.job, read.supervisor);
    assert_ne!(mine, read.supervisor);
    assert_eq!(read.supervisor_answers_for, mine, "the supervisor");
    assert_eq!(read.job_answers_for, mine, "the job");
}

/// Any start in the chain that disclaims breaks it: a supervisor started disclaimed answers
/// for itself, and its job, kept, answers for the supervisor, not the agent; a supervisor
/// started kept whose job is disclaimed leaves the job answering for itself. Without this, a
/// read-back that named the test's responsible process for every process of the chain would
/// pass the test above.
#[test]
fn a_disclaimed_start_in_the_chain_breaks_it() {
    let read = chain(None, "kept");
    assert_eq!(
        read.supervisor_answers_for, read.supervisor,
        "the supervisor"
    );
    assert_eq!(read.job_answers_for, read.supervisor, "the job");
    let read = chain(Some(Kept::Supervisor), "disclaimed");
    assert_eq!(read.supervisor_answers_for, starters(), "the supervisor");
    assert_eq!(read.job_answers_for, read.job, "the job");
}

/// Programs started at the same moment from several threads, some keeping the responsibility
/// and some not, each answer as their own start asked. It catches a keep that reaches another
/// start, or a disclaim that does, through what the starts make before the fork.
#[test]
fn kept_and_disclaimed_starts_at_the_same_moment_keep_their_own_answer() {
    let barrier = std::sync::Barrier::new(4);
    let kinds = [Some(Kept::Job), None, Some(Kept::Supervisor), None];
    let started: Vec<(Option<Kept>, (Job, u32))> = std::thread::scope(|scope| {
        let threads: Vec<_> = kinds
            .iter()
            .map(|&kept| {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    (kept, ready(kept))
                })
            })
            .collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });
    let mine = starters();
    for (kept, (job, pid)) in started {
        let want = if kept.is_some() { mine } else { pid };
        assert_eq!(responsible_pid(pid).unwrap(), want, "{kept:?}");
        job.finish();
    }
}

/// The read-back tells the two apart: a process started the usual way answers for the
/// process that started it (this test's own responsible process), not for itself. Without
/// this, a read-back that always named the process itself would pass the tests above.
#[test]
fn a_process_started_the_usual_way_answers_for_its_starter() {
    let (stdin, mut feed) = std::io::pipe().unwrap();
    let mut child = Command::new("/bin/cat")
        .stdin(stdin)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mine = responsible_pid(std::process::id()).unwrap();
    assert_eq!(responsible_pid(pid).unwrap(), mine);
    assert_ne!(mine, pid);
    feed.write_all(b"done\n").unwrap();
    drop(feed);
    assert!(child.wait().unwrap().success());
}

/// A pid above the system's largest is refused, not wrapped to another process's. It
/// catches an out-of-range pid that would read some other process's answer.
#[test]
fn a_pid_out_of_range_is_refused() {
    let error = responsible_pid(u32::MAX).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

/// The job gets exactly the environment the helper built for it, and a program named
/// without a directory is found in the job's `PATH`, which only the job has: a script in a
/// scratch directory that the test's own `PATH` does not name. It catches an execution
/// that passes the helper's (or the agent's) own environment to the job, leaking the
/// daemon's variables into user code, or one that looks the program up on the daemon's
/// `PATH`.
#[test]
fn a_disclaimed_job_gets_exactly_its_own_environment() {
    let dir = Scratch::new("disclaim-env");
    let bin = dir.path("job only bin");
    std::fs::create_dir_all(&bin).unwrap();
    // `env` itself under a name only this directory has: no shell stands between it and
    // the job's environment.
    std::os::unix::fs::symlink("/usr/bin/env", bin.join("js-disclaim-only-here")).unwrap();
    let path = bin.to_str().unwrap();
    let env = [("PATH", path), ("KEEP", "kept")];
    let job = start(&dir.path(""), &["js-disclaim-only-here"], &env).unwrap();
    let text = printed(job);
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    let path_line = format!("PATH={path}");
    assert_eq!(
        lines,
        [
            "HOME=/var/empty",
            "KEEP=kept",
            "LOGNAME=kbf-lease-7-1",
            path_line.as_str(),
            "USER=kbf-lease-7-1",
        ]
    );
}

/// An executable text file with no `#!` line runs through `/bin/sh`, with its arguments
/// and the job's environment, named by a path or found in `PATH`, as `execvp` runs it.
/// It catches a start that refuses such a script (`ENOEXEC`), which `execvp` never did.
#[test]
fn a_script_without_an_interpreter_line_runs_through_the_shell() {
    let dir = Scratch::new("disclaim-plain-script");
    let bin = dir.path("bin");
    let file = script(&bin, "plain", "echo \"plain $1 $GREETING\"\n", 0o755);
    let env = [("PATH", bin.to_str().unwrap()), ("GREETING", "hi")];
    for program in [file.to_str().unwrap(), "plain"] {
        let job = start(&dir.path(""), &[program, "arg"], &env).unwrap();
        assert_eq!(printed(job), "plain arg hi\n", "{program}");
    }
}

/// The search passes over a file it may not execute and a directory it may not enter, to
/// a later file that runs; when no file may be executed the start fails with the
/// refusal, not as not found. It catches a non-executable first file that hides a good
/// later one, or a refusal reported as a missing program.
#[test]
fn the_search_passes_over_what_it_may_not_execute() {
    let dir = Scratch::new("disclaim-denied");
    script(&dir.path("first"), "prog", "#!/bin/sh\necho first\n", 0o644);
    let locked = dir.path("locked");
    script(&locked, "prog", "#!/bin/sh\necho locked\n", 0o755);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Opened again whatever happens, before the scratch directory is removed.
    let _unlock = Unlock(locked.clone());
    script(
        &dir.path("second"),
        "prog",
        "#!/bin/sh\necho second\n",
        0o755,
    );
    let root = dir.path("");
    let every = format!("{0}first:{0}locked:{0}second", root.display());
    let job = start(&root, &["prog"], &[("PATH", &every)]).unwrap();
    assert_eq!(printed(job), "second\n");
    let denied = format!("{0}first:{0}locked", root.display());
    let refused = start(&root, &["prog"], &[("PATH", &denied)]).err().unwrap();
    assert_eq!(refused.raw_os_error(), Some(libc::EACCES));
}

/// Gives a directory back its permissions when it is dropped.
struct Unlock(PathBuf);

impl Drop for Unlock {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// A relative directory of the job's `PATH`, and an empty one (the working directory),
/// are searched in the directory the job runs in: the helper's lease directory, or a
/// [`Program`]'s `current_dir`, never the starter's own. It catches a lookup made in the
/// parent before the child has entered its directory.
#[test]
fn relative_path_directories_are_the_jobs_own() {
    let dir = Scratch::new("disclaim-relative");
    let root = dir.path("");
    script(
        &root.join("bin"),
        "rel-prog",
        "#!/bin/sh\necho rel\n",
        0o755,
    );
    script(&root, "here-prog", "#!/bin/sh\necho here\n", 0o755);
    let cases = [
        ("bin", "rel-prog", "rel\n"),
        (":/nonexistent", "here-prog", "here\n"),
    ];
    for (path, program, said) in cases {
        let job = start(&root, &[program], &[("PATH", path)]).unwrap();
        assert_eq!(printed(job), said, "the lease directory, {path:?}");
        let out = Program::new(program, [""; 0])
            .env_clear()
            .env("PATH", path)
            .current_dir(&root)
            .output()
            .unwrap();
        assert_eq!(out.stdout, said.as_bytes(), "current_dir, {path:?}");
    }
}

/// The environment of a [`Program`] is its calls applied in order, as the macOS start
/// builds it: the last value set wins, `env_clear` drops what was set before it, an empty
/// value is a value, and a value that is not UTF-8 arrives byte for byte; an argument that
/// is not UTF-8 too. It catches an environment or an argument rebuilt wrongly on the way
/// to `posix_spawn`.
#[test]
fn the_environment_and_arguments_arrive_as_set() {
    let raw = OsString::from_vec(vec![0xff, b'v', 0xfe]);
    let out = Program::new("/usr/bin/env", [""; 0])
        .env("GONE", "set before the clear")
        .env_clear()
        .env("A", "first")
        .envs([("A", "second"), ("B", "b")])
        .env("B", "last")
        .env("EMPTY", "")
        .env("RAW", &raw)
        .output()
        .unwrap();
    let mut lines: Vec<Vec<u8>> = out
        .stdout
        .split(|&b| b == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    lines.retain(|line| !line.is_empty());
    lines.sort_unstable();
    let expected: [&[u8]; 4] = [b"A=second", b"B=last", b"EMPTY=", b"RAW=\xffv\xfe"];
    assert_eq!(lines, expected);
    let arg = OsString::from_vec(vec![0xff, b'a']);
    let echoed = Program::new("/bin/echo", [&arg]).output().unwrap();
    assert_eq!(echoed.stdout, [0xff, b'a', b'\n']);
}

/// Jobs started at the same moment from several threads keep their own arguments,
/// environments, streams and responsibility, and one that fails to start leaves the others
/// alone. It catches state of one start reaching another through what the starts make
/// before the fork.
#[test]
fn jobs_started_at_the_same_moment_keep_their_own_state() {
    let dir = Scratch::new("disclaim-together");
    let root = dir.path("");
    let barrier = std::sync::Barrier::new(3);
    let script = "echo \"$1 $WHO $$\"; read line";
    let one = |name: &str| {
        let who = format!("env-{name}");
        barrier.wait();
        start(
            &root,
            &["/bin/sh", "-c", script, "sh", name],
            &[("WHO", &who)],
        )
        .unwrap()
    };
    let (a, b, missing) = std::thread::scope(|scope| {
        let a = scope.spawn(|| one("a"));
        let b = scope.spawn(|| one("b"));
        let missing = scope.spawn(|| {
            barrier.wait();
            start(&root, &["/no/such/program"], &[]).map(drop)
        });
        (
            a.join().unwrap(),
            b.join().unwrap(),
            missing.join().unwrap(),
        )
    });
    assert_eq!(missing.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    for (mut job, name) in [(a, "a"), (b, "b")] {
        let pid = job.child.id();
        assert_eq!(job.line(), format!("{name} env-{name} {pid}"));
        assert_eq!(responsible_pid(pid).unwrap(), pid, "{name}");
        job.finish();
    }
}

/// An empty program is not found, and no directory of the job's `PATH` is tried for it. It
/// catches an empty name joined to each directory, which `posix_spawn` would refuse as a
/// directory (`EACCES`) instead of a missing program.
#[test]
fn an_empty_program_is_not_found() {
    let dir = Scratch::new("disclaim-empty");
    let refused = start(&dir.path(""), &[""], &[("PATH", "/bin:/usr/bin")])
        .err()
        .unwrap();
    assert_eq!(refused.kind(), std::io::ErrorKind::NotFound);
    let error = Program::new("", [""; 0]).spawn().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
