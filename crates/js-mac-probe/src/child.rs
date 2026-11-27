//! The child modes: tiny programs the probe starts inside a sandbox profile (or outside,
//! as the control) so the probe observes what a confined process can do. They are part of
//! this binary so no probe depends on a tool whose presence on the runner image is assumed.

use std::net::{SocketAddr, ToSocketAddrs as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use clap::Subcommand;

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
    /// Sleep, then print the tag. The tag is only there to be seen on the command line.
    Hold {
        /// How long to sleep, in seconds.
        #[arg(long)]
        secs: u64,
        /// Marks the process as the probe's in `ps`.
        #[arg(long)]
        tag: String,
    },
    /// What the launchd probe's job runs: start `hold` in a process group of its own, write
    /// `job=<pid> hold=<pid>` to `out` whole, and wait for it.
    LaunchdJob {
        /// Where to write the two pids.
        #[arg(long)]
        out: PathBuf,
        /// Passed to `hold`.
        #[arg(long)]
        secs: u64,
        /// Passed to `hold`.
        #[arg(long)]
        tag: String,
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
        Child::Hold { secs, tag } => {
            std::thread::sleep(Duration::from_secs(*secs));
            Ok(format!("{tag}\n"))
        }
        Child::LaunchdJob { out, secs, tag } => std::env::current_exe()
            .and_then(|exe| launchd_job(&mut hold_command(&exe, *secs, tag), out)),
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

/// `exe child hold --secs <secs> --tag <tag>`.
fn hold_command(exe: &Path, secs: u64, tag: &str) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(["child", "hold", "--secs", &secs.to_string(), "--tag", tag]);
    cmd
}

/// Starts `hold` in a process group of its own, writes both pids to `out` (to a `.part`
/// file first, renamed, so a reader never sees half a line), then waits for `hold`. If the
/// pids cannot be written, `hold` is ended: nothing would know to end it.
fn launchd_job(hold: &mut Command, out: &Path) -> std::io::Result<String> {
    let mut child = hold
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let line = crate::launchd::ready_line(std::process::id(), child.id());
    let part = out.with_extension("part");
    let written = std::fs::write(&part, line).and_then(|()| std::fs::rename(&part, out));
    if let Err(err) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    child.wait().map(|_| String::new())
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
    fn hold_sleeps_then_succeeds() {
        let hold = Child::Hold {
            secs: 0,
            tag: "t".into(),
        };
        assert_eq!(run(&hold), ExitCode::SUCCESS);
    }

    #[test]
    fn the_hold_command_names_the_mode_and_passes_secs_and_tag() {
        let cmd = hold_command(Path::new("/p/probe"), 300, "js-mac-probe.7");
        assert_eq!(cmd.get_program(), "/p/probe");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            args,
            ["child", "hold", "--secs", "300", "--tag", "js-mac-probe.7"]
        );
    }

    /// The test binary listing its tests stands in for `hold`: a child that exits at once.
    fn quick_child() -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.arg("--list");
        cmd
    }

    #[test]
    fn the_launchd_job_writes_its_own_pid_and_its_childs_whole_and_waits() {
        let out = scratch("launchd-ready");
        assert_eq!(launchd_job(&mut quick_child(), &out).unwrap(), "");
        let text = std::fs::read_to_string(&out).unwrap();
        std::fs::remove_file(&out).unwrap();
        let (job, hold) = crate::launchd::ready(&text).unwrap();
        assert_eq!(job, std::process::id());
        assert_ne!(hold, job);
        assert!(!out.with_extension("part").exists());
    }

    #[test]
    fn a_launchd_job_that_cannot_write_its_pids_fails() {
        let out = scratch("launchd-missing-dir").join("ready");
        assert!(launchd_job(&mut quick_child(), &out).is_err());
        let job = Child::LaunchdJob {
            out,
            secs: 0,
            tag: "t".into(),
        };
        assert_eq!(run(&job), ExitCode::FAILURE);
    }

    #[test]
    fn a_launchd_job_whose_child_cannot_start_fails() {
        let mut missing = Command::new(scratch("no-such-program"));
        assert!(launchd_job(&mut missing, &scratch("launchd-unused")).is_err());
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
