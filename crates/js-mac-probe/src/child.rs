//! The child modes: tiny programs the probe starts inside a sandbox profile (or outside,
//! as the control) so the probe observes what a confined process can do. They are part of
//! this binary so no probe depends on a tool whose presence on the runner image is assumed.
//! Two of them are the launchd probe's: the job launchd runs and the children it starts.

use std::io::{self, BufRead as _, Write as _};
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use clap::Subcommand;

use crate::launchd::{self, Hello, Ids, Kind, Spawned};

/// How long a probe connection may take before it counts as refused.
const CONNECT_LIMIT: Duration = Duration::from_secs(5);

/// What the child does. Exit status 0 means it was allowed to; 1 that it was refused or
/// failed, with `errno=N` and the reason on stderr.
#[derive(Debug, Clone, Subcommand)]
pub enum Child {
    /// Connect a TCP socket.
    Connect {
        /// The address.
        addr: SocketAddr,
    },
    /// Resolve a name through the system resolver, printing the addresses.
    Resolve {
        /// The host name.
        host: String,
    },
    /// Print a file's contents.
    Read {
        /// The path.
        path: PathBuf,
    },
    /// Print `set=<value>` or `unset` for an environment variable.
    Env {
        /// The variable's name.
        name: String,
    },
    /// Create or overwrite a file.
    Write {
        /// The path.
        path: PathBuf,
    },
    /// The launchd probe's child: print its ids (`pid=.. pgid=.. sid=.. uid=..`), then stay.
    Hold {
        /// Only for `ps`: the probe tells its children apart, and from a reused pid, by it.
        #[arg(long)]
        tag: PathBuf,
        /// How long to stay, so that a child the probe fails to end still ends.
        #[arg(long)]
        secs: u64,
        /// Start a session of its own first.
        #[arg(long)]
        setsid: bool,
        /// Be a supervisor: start a `hold` child of its own in a group of its own as
        /// `<uid>:<gid>` (tagged as the case's supervised job), and print that child's line
        /// (or `error <reason>`) after its own.
        #[arg(long)]
        supervise: Option<Ids>,
    },
    /// The launchd probe's job: append its pid to the state directory's instances file and,
    /// on its first start there, start one `hold` child of each kind and list them in the
    /// children file; then stay.
    LaunchdJob {
        /// The case's state directory.
        #[arg(long)]
        state: PathBuf,
        /// How long the job and its children stay.
        #[arg(long)]
        secs: u64,
        /// Also start a child in a group of its own as `<uid>:<gid>`.
        #[arg(long)]
        drop: Option<Ids>,
    },
}

/// Runs a child mode.
#[must_use]
pub fn run(child: &Child) -> ExitCode {
    let outcome = match child {
        Child::Connect { addr } => {
            std::net::TcpStream::connect_timeout(addr, CONNECT_LIMIT).map(|_| String::new())
        }
        Child::Resolve { host } => resolve(host),
        Child::Read { path } => read_text(path),
        Child::Env { name } => Ok(env_text(name)),
        Child::Write { path } => std::fs::write(path, b"probe\n").map(|()| String::new()),
        Child::Hold {
            tag,
            secs,
            setsid,
            supervise,
        } => hold(tag, *setsid, *secs, *supervise),
        Child::LaunchdJob { state, secs, drop } => launchd_job(state, *secs, *drop),
    };
    match outcome {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("errno={} {err}", err.raw_os_error().unwrap_or(0));
            ExitCode::FAILURE
        }
    }
}

/// This process's ids.
fn own_hello() -> Hello {
    let pgid = rustix::process::getpgrp().as_raw_nonzero().get();
    // getsid of the calling process cannot fail; -1 would say it did.
    let sid = rustix::process::getsid(None).map_or(-1, |sid| sid.as_raw_nonzero().get());
    let uid = rustix::process::getuid().as_raw();
    Hello {
        pid: i32::try_from(std::process::id()).unwrap_or(-1),
        pgid,
        sid,
        uid,
    }
}

fn hold(tag: &Path, setsid: bool, secs: u64, supervise: Option<Ids>) -> io::Result<String> {
    // setsid fails only for a process group leader.
    if setsid {
        rustix::process::setsid().map_err(io::Error::from)?;
    }
    // A supervisor's child is its own, in a group of its own, as a run's job is.
    let job = supervise.map(|ids| {
        start_hold(&HoldSpec {
            tag: tag.with_file_name(Kind::SupervisedJob.name()),
            secs,
            setsid: false,
            own_group: true,
            ids: Some(ids),
            supervise: None,
        })
    });
    let mut out = io::stdout().lock();
    writeln!(out, "{}", own_hello())?;
    if let Some(job) = &job {
        writeln!(out, "{}", launchd::render_outcome(&job.outcome))?;
    }
    out.flush()?;
    drop(out);
    std::thread::sleep(Duration::from_secs(secs));
    // The supervisor waits for its child, as a run's supervisor waits for the run.
    if let Some(Started {
        child: Some(mut child),
        ..
    }) = job
    {
        let _ = child.wait();
    }
    Ok(String::new())
}

fn launchd_job(state: &Path, secs: u64, ids: Option<Ids>) -> io::Result<String> {
    let mut instances = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join(launchd::INSTANCES))?;
    writeln!(instances, "{}", std::process::id())?;
    let children = state.join(launchd::CHILDREN);
    let mut waiters = Vec::new();
    if !children.exists() {
        let mut listed = Vec::new();
        for kind in Kind::launched(ids.is_some()) {
            let started = start_hold(&HoldSpec::of(state, kind, secs, ids));
            listed.push(Spawned {
                kind,
                outcome: started.outcome,
            });
            if kind == Kind::Supervisor {
                listed.push(Spawned {
                    kind: Kind::SupervisedJob,
                    outcome: started.job,
                });
            }
            waiters.extend(started.child.map(|child| reap(state, kind, child)));
        }
        let temporary = state.join(format!("{}.tmp", launchd::CHILDREN));
        std::fs::write(&temporary, launchd::render_children(&listed))?;
        std::fs::rename(&temporary, &children)?;
    }
    std::thread::sleep(Duration::from_secs(secs));
    for waiter in waiters {
        let _ = waiter.join();
    }
    Ok(String::new())
}

/// Waits for a child on a thread of its own and appends how it ended to the ends file, so
/// that a child that ends while its job runs is seen to end, and how.
fn reap(state: &Path, kind: Kind, mut child: std::process::Child) -> std::thread::JoinHandle<()> {
    let ends = state.join(launchd::ENDS);
    std::thread::spawn(move || {
        let line = format!("{} {}\n", kind.name(), launchd::end_text(&child.wait()));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(ends);
        let _ = file.and_then(|mut file| file.write_all(line.as_bytes()));
    })
}

/// How to start one `hold` child.
struct HoldSpec {
    tag: PathBuf,
    secs: u64,
    setsid: bool,
    own_group: bool,
    /// The ids the child runs as.
    ids: Option<Ids>,
    /// The ids the child, a supervisor, gives its own child.
    supervise: Option<Ids>,
}

impl HoldSpec {
    /// The child of `kind` a job starts: only the dropped child runs as the ids, and the
    /// supervisor passes them on to its own child.
    fn of(state: &Path, kind: Kind, secs: u64, ids: Option<Ids>) -> Self {
        Self {
            tag: launchd::tag(state, kind),
            secs,
            setsid: kind == Kind::OwnSession,
            own_group: kind.own_group(),
            ids: ids.filter(|_| kind == Kind::Dropped),
            supervise: ids.filter(|_| kind == Kind::Supervisor),
        }
    }
}

/// A `hold` child as started: its line, a supervisor's line for its own child, and the
/// child if it started. What becomes of it is the probe's question.
struct Started {
    outcome: Result<Hello, String>,
    /// A supervisor's second line; for any other child, why there is none.
    job: Result<Hello, String>,
    child: Option<std::process::Child>,
}

/// Starts a `hold` child and reads the line it prints when it has started (and, from a
/// supervisor, the line for its own child).
fn start_hold(spec: &HoldSpec) -> Started {
    let started = std::env::current_exe().and_then(|exe| spawn_hold(&exe, spec));
    let (child, reader) = match started {
        Ok(started) => started,
        Err(err) => {
            let reason = format!("cannot start: {err}");
            return Started {
                job: Err(format!("its supervisor did not start: {reason}")),
                outcome: Err(reason),
                child: None,
            };
        }
    };
    let pid = child.id();
    let mut reader = io::BufReader::new(reader);
    let mut line = String::new();
    let read = reader.read_line(&mut line);
    let outcome = Hello::parse(&line).ok_or(format!("pid {pid}: no start line, read {read:?}"));
    let mut second = String::new();
    let read = spec.supervise.map(|_| reader.read_line(&mut second));
    let missing = Err(format!("pid {pid}: no line for its child, read {read:?}"));
    let job = launchd::parse_outcome(&second).unwrap_or(missing);
    Started {
        outcome,
        job,
        child: Some(child),
    }
}

/// The `hold` child, its standard output a pipe whose write end only the child holds.
fn spawn_hold(exe: &Path, spec: &HoldSpec) -> io::Result<(std::process::Child, io::PipeReader)> {
    let (reader, writer) = io::pipe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("child")
        .arg("hold")
        .arg("--tag")
        .arg(&spec.tag)
        .arg("--secs")
        .arg(spec.secs.to_string())
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::null());
    if spec.setsid {
        cmd.arg("--setsid");
    }
    if let Some(ids) = spec.supervise {
        cmd.arg("--supervise").arg(ids.to_string());
    }
    if spec.own_group {
        cmd.process_group(0);
    }
    if let Some(ids) = spec.ids {
        cmd.uid(ids.uid).gid(ids.gid);
    }
    let child = cmd.spawn()?;
    // `cmd` holds the parent's copy of the write end; dropping it leaves the child's alone.
    drop(cmd);
    Ok((child, reader))
}

fn read_text(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

fn env_text(name: &str) -> String {
    std::env::var_os(name).map_or_else(
        || "unset\n".to_owned(),
        |value| format!("set={}\n", value.to_string_lossy()),
    )
}

fn resolve(host: &str) -> std::io::Result<String> {
    let addrs: Vec<String> = (host, 0)
        .to_socket_addrs()?
        .map(|a| a.ip().to_string())
        .collect();
    Ok(addrs.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("js-mac-probe-{}-{name}", std::process::id()))
    }

    #[test]
    fn connect_succeeds_to_a_listener_and_fails_to_a_closed_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(run(&Child::Connect { addr }), ExitCode::SUCCESS);
        // Not `listener` dropped: that frees the number for any other test to bind.
        let dead = js_testkit::port::dead_addr();
        assert_eq!(run(&Child::Connect { addr: dead }), ExitCode::FAILURE);
        drop(listener);
    }

    #[test]
    fn resolve_answers_for_localhost_and_fails_for_an_invalid_name() {
        assert_eq!(
            run(&Child::Resolve {
                host: "localhost".into()
            }),
            ExitCode::SUCCESS
        );
        assert_eq!(
            run(&Child::Resolve {
                host: "no such host.invalid".into()
            }),
            ExitCode::FAILURE
        );
    }

    #[test]
    fn write_creates_the_file_and_fails_in_a_missing_directory() {
        let path = scratch("write");
        assert_eq!(run(&Child::Write { path: path.clone() }), ExitCode::SUCCESS);
        assert_eq!(std::fs::read(&path).unwrap(), b"probe\n");
        std::fs::remove_file(&path).unwrap();
        let missing = scratch("missing-dir").join("file");
        assert_eq!(run(&Child::Write { path: missing }), ExitCode::FAILURE);
    }

    #[test]
    fn read_prints_a_file_and_fails_for_a_missing_one() {
        let path = scratch("read");
        std::fs::write(&path, b"secret\n").unwrap();
        assert_eq!(read_text(&path).unwrap(), "secret\n");
        assert_eq!(run(&Child::Read { path: path.clone() }), ExitCode::SUCCESS);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(run(&Child::Read { path }), ExitCode::FAILURE);
    }

    #[test]
    fn a_hold_child_reports_this_processs_own_pid() {
        let hello = own_hello();
        assert_eq!(hello.pid, i32::try_from(std::process::id()).unwrap());
        let child = Child::Hold {
            tag: scratch("hold"),
            secs: 0,
            setsid: false,
            supervise: None,
        };
        assert_eq!(run(&child), ExitCode::SUCCESS);
    }

    #[test]
    fn a_job_lists_each_child_that_prints_no_start_line_with_the_reason() {
        // Here the current executable is the test harness, which has no `child hold`:
        // every child exits without its line.
        let state = js_testkit::dir::TestDir::under(std::env::temp_dir(), "job-no-line");
        let job = Child::LaunchdJob {
            state: state.to_path_buf(),
            secs: 0,
            drop: None,
        };
        assert_eq!(run(&job), ExitCode::SUCCESS);
        let text = std::fs::read_to_string(state.join(launchd::CHILDREN)).unwrap();
        let children = launchd::parse_children(&text);
        assert_eq!(children.len(), 3, "{text}");
        for child in children {
            let reason = child.outcome.unwrap_err();
            assert!(reason.starts_with("pid "), "{reason}");
            assert!(reason.contains(": no start line, read Ok(0)"), "{reason}");
        }
    }

    #[test]
    fn env_reports_a_set_variable_with_its_value_and_an_unset_one() {
        assert!(env_text("PATH").starts_with("set=/"));
        assert_eq!(env_text("JS_MAC_PROBE_SURELY_UNSET"), "unset\n");
        assert_eq!(
            run(&Child::Env {
                name: "PATH".into()
            }),
            ExitCode::SUCCESS
        );
    }
}
