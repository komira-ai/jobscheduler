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
            tag: _,
            secs,
            setsid,
        } => hold(*setsid, *secs),
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
    // SAFETY: getpgrp, getsid(0) and getuid have no preconditions and cannot fail for the
    // calling process.
    let (pgid, sid, uid) = unsafe { (libc::getpgrp(), libc::getsid(0), libc::getuid()) };
    Hello {
        pid: i32::try_from(std::process::id()).unwrap_or(-1),
        pgid,
        sid,
        uid,
    }
}

fn hold(setsid: bool, secs: u64) -> io::Result<String> {
    // SAFETY: setsid has no preconditions; it fails only for a process group leader.
    if setsid && unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut out = io::stdout().lock();
    writeln!(out, "{}", own_hello())?;
    out.flush()?;
    drop(out);
    std::thread::sleep(Duration::from_secs(secs));
    Ok(String::new())
}

fn launchd_job(state: &Path, secs: u64, ids: Option<Ids>) -> io::Result<String> {
    let mut instances = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join(launchd::INSTANCES))?;
    writeln!(instances, "{}", std::process::id())?;
    let children = state.join(launchd::CHILDREN);
    if !children.exists() {
        let exe = std::env::current_exe()?;
        let started: Vec<Spawned> = Kind::started(ids.is_some())
            .into_iter()
            .map(|kind| Spawned {
                kind,
                outcome: start_hold(
                    &exe,
                    state,
                    kind,
                    secs,
                    ids.filter(|_| kind == Kind::Dropped),
                ),
            })
            .collect();
        let temporary = state.join(format!("{}.tmp", launchd::CHILDREN));
        std::fs::write(&temporary, launchd::render_children(&started))?;
        std::fs::rename(&temporary, &children)?;
    }
    std::thread::sleep(Duration::from_secs(secs));
    Ok(String::new())
}

/// Starts a `hold` child of `kind` and reads the line it prints when it has started. The
/// child is left running: what becomes of it is the probe's question.
fn start_hold(
    exe: &Path,
    state: &Path,
    kind: Kind,
    secs: u64,
    ids: Option<Ids>,
) -> Result<Hello, String> {
    let started = spawn_hold(exe, state, kind, secs, ids);
    let (child, reader) = started.map_err(|err| format!("cannot start: {err}"))?;
    let mut line = String::new();
    let read = io::BufReader::new(reader).read_line(&mut line);
    let pid = child.id();
    Hello::parse(&line).ok_or(format!("pid {pid}: no start line, read {read:?}"))
}

/// The `hold` child, its standard output a pipe whose write end only the child holds.
fn spawn_hold(
    exe: &Path,
    state: &Path,
    kind: Kind,
    secs: u64,
    ids: Option<Ids>,
) -> io::Result<(std::process::Child, io::PipeReader)> {
    let (reader, writer) = io::pipe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("child")
        .arg("hold")
        .arg("--tag")
        .arg(launchd::tag(state, kind))
        .arg("--secs")
        .arg(secs.to_string())
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::null());
    if kind == Kind::OwnSession {
        cmd.arg("--setsid");
    }
    if kind.own_group() {
        cmd.process_group(0);
    }
    if let Some(ids) = ids {
        cmd.uid(ids.uid).gid(ids.gid);
    }
    let child = cmd.spawn()?;
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
