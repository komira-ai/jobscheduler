//! The child modes: tiny programs the probe starts inside a sandbox profile (or outside,
//! as the control) so the probe observes what a confined process can do. They are part of
//! this binary so no probe depends on a tool whose presence on the runner image is assumed.
//!
//! The limit modes (`allocate`, `write-bytes`, `burn`, `spawn`) set a resource limit on
//! themselves with `setrlimit` and then act past it, so the probe sees what the kernel
//! does to a process over that limit. They also set `RLIMIT_CORE` to zero where a signal may
//! end them, so an ended child leaves no core file.

use std::io::{self, Read as _};
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use clap::Subcommand;
use rustix::process::{Resource, Rlimit};

/// How long a probe connection may take before it counts as refused.
const CONNECT_LIMIT: Duration = Duration::from_secs(5);

/// The steps of one round of `burn`: enough that a round is CPU work, few enough that one
/// round ends at once.
const BURN_STEPS: u64 = 1_000_000;

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
    /// A mode that sets a resource limit on itself and then acts past it.
    #[command(flatten)]
    Limited(Limited),
    /// Print the reader's effective uid and `proc_pid_rusage` of process `pid`:
    /// `reader_uid=<uid> user_time=<ticks> system_time=<ticks> resident=<bytes>
    /// footprint=<bytes>`. macOS only; elsewhere it fails as unsupported.
    Rusage {
        /// The process.
        pid: i32,
    },
}

/// The limit modes: each sets its limits on itself ([`Limited::limits`]), then acts
/// ([`Limited::act`]).
#[derive(Debug, Clone, Subcommand)]
pub enum Limited {
    /// Set `RLIMIT_AS` and `RLIMIT_DATA` (soft and hard) to `--limit` bytes, then allocate
    /// `--bytes` and write every byte; print `allocated=<bytes>`. (On macOS `RLIMIT_RSS` is
    /// `RLIMIT_AS`.)
    Allocate {
        /// The limit, in bytes.
        #[arg(long)]
        limit: u64,
        /// How much to allocate, in bytes.
        #[arg(long)]
        bytes: usize,
    },
    /// Set `RLIMIT_FSIZE` (soft and hard) to `--limit` bytes, then write `--bytes` zero
    /// bytes to a new `path`; print `wrote=<bytes>`.
    WriteBytes {
        /// The limit, in bytes.
        #[arg(long)]
        limit: u64,
        /// How much to write, in bytes.
        #[arg(long)]
        bytes: u64,
        /// The file, created or truncated.
        path: PathBuf,
    },
    /// Set the soft `RLIMIT_CPU` to `--seconds` (the hard limit stays), then burn CPU for
    /// `--rounds` rounds of work; print `burned=<rounds>`. The rounds bound the child by
    /// work done, so a limit that is not enforced ends in an exit, not a hang.
    Burn {
        /// The soft limit, in CPU seconds.
        #[arg(long)]
        seconds: u64,
        /// How many rounds of work to do.
        #[arg(long)]
        rounds: u64,
    },
    /// With `--max-processes`, set `RLIMIT_NPROC` (soft and hard) to it; then start
    /// `program` with `args` and wait for it; print `spawned <its exit status>`.
    Spawn {
        /// The most processes this process's uid may have.
        #[arg(long)]
        max_processes: Option<u64>,
        /// The program to start (an absolute path).
        program: PathBuf,
        /// Its arguments.
        args: Vec<String>,
    },
}

impl Limited {
    /// The limits the mode sets on itself, in order.
    #[must_use]
    pub fn limits(&self) -> Vec<(Resource, Rlimit)> {
        match self {
            Self::Allocate { limit, .. } => {
                vec![(Resource::As, both(*limit)), (Resource::Data, both(*limit))]
            }
            Self::WriteBytes { limit, .. } => {
                vec![(Resource::Fsize, both(*limit)), (Resource::Core, both(0))]
            }
            Self::Burn { seconds, .. } => {
                let cpu = Rlimit {
                    current: Some(*seconds),
                    maximum: rustix::process::getrlimit(Resource::Cpu).maximum,
                };
                vec![(Resource::Cpu, cpu), (Resource::Core, both(0))]
            }
            Self::Spawn { max_processes, .. } => max_processes
                .map(|n| (Resource::Nproc, both(n)))
                .into_iter()
                .collect(),
        }
    }

    /// What the mode does once its limits are set; its output.
    ///
    /// # Errors
    /// The action's failure: an allocation that cannot be reserved, a file that cannot be
    /// created, a program that cannot be started.
    pub fn act(&self) -> io::Result<String> {
        match self {
            Self::Allocate { bytes, .. } => allocate(*bytes),
            Self::WriteBytes { bytes, path, .. } => write_bytes(path, *bytes),
            Self::Burn { rounds, .. } => Ok(format!("burned={}\n", burn(*rounds))),
            Self::Spawn { program, args, .. } => spawn(program, args),
        }
    }
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
        Child::Limited(mode) => set_limits(&mode.limits()).and_then(|()| mode.act()),
        Child::Rusage { pid } => pid_usage_text(*pid),
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

/// A limit of `value`, soft and hard.
fn both(value: u64) -> Rlimit {
    Rlimit {
        current: Some(value),
        maximum: Some(value),
    }
}

/// Sets each limit on this process, in order, stopping at the first the kernel refuses.
fn set_limits(limits: &[(Resource, Rlimit)]) -> io::Result<()> {
    limits.iter().try_for_each(|(resource, limit)| {
        rustix::process::setrlimit(*resource, *limit).map_err(io::Error::from)
    })
}

/// Allocates `bytes` and writes every byte, so each page is in use.
fn allocate(bytes: usize) -> io::Result<String> {
    let mut block: Vec<u8> = Vec::new();
    block
        .try_reserve_exact(bytes)
        .map_err(|err| io::Error::other(format!("cannot allocate {bytes} bytes: {err}")))
        .map(|()| {
            block.resize(bytes, 1);
            format!("allocated={}\n", std::hint::black_box(&block).len())
        })
}

/// Writes `bytes` zero bytes to a new `path`.
fn write_bytes(path: &Path, bytes: u64) -> io::Result<String> {
    std::fs::File::create(path)
        .and_then(|mut file| io::copy(&mut io::repeat(0).take(bytes), &mut file))
        .map(|wrote| format!("wrote={wrote}\n"))
}

/// Does `rounds` rounds of arithmetic the compiler cannot drop; returns `rounds`.
fn burn(rounds: u64) -> u64 {
    let mut acc = 0u64;
    for round in 0..rounds {
        for step in 0..BURN_STEPS {
            acc = std::hint::black_box(acc.wrapping_mul(6_364_136_223_846_793_005) ^ step ^ round);
        }
    }
    std::hint::black_box(acc);
    rounds
}

/// Starts `program` with `args`, its output discarded, and waits for it.
fn spawn(program: &Path, args: &[String]) -> io::Result<String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| format!("spawned {status}\n"))
}

#[cfg(target_os = "macos")]
fn pid_usage_text(pid: i32) -> io::Result<String> {
    crate::macos::pid_usage_text(pid)
}

#[cfg(not(target_os = "macos"))]
fn pid_usage_text(_pid: i32) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "proc_pid_rusage is macOS only",
    ))
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

    /// A limit whose soft value is above its hard value is refused (`EINVAL`) before the
    /// kernel changes anything, so this runs in the test's own process. Catches a refusal
    /// that is dropped instead of ending the mode before it acts.
    #[test]
    fn a_limit_the_kernel_refuses_is_an_error() {
        let refused = Rlimit {
            current: Some(1),
            maximum: Some(0),
        };
        let err = set_limits(&[(Resource::Core, refused)]).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
        assert!(set_limits(&[]).is_ok());
    }

    /// Each limit mode's limits, in the order it sets them. Catches a mode that limits the
    /// wrong resource, sets a soft limit without the hard one (or the CPU hard limit, which
    /// Linux answers with `SIGKILL` instead of `SIGXCPU`), or leaves core files possible.
    #[test]
    fn each_limit_mode_sets_its_own_limits() {
        let allocate = Limited::Allocate { limit: 7, bytes: 1 };
        assert_eq!(
            allocate.limits(),
            [(Resource::As, both(7)), (Resource::Data, both(7))]
        );
        let write = Limited::WriteBytes {
            limit: 9,
            bytes: 1,
            path: scratch("unused"),
        };
        assert_eq!(
            write.limits(),
            [(Resource::Fsize, both(9)), (Resource::Core, both(0))]
        );
        let hard = rustix::process::getrlimit(Resource::Cpu).maximum;
        let burn = Limited::Burn {
            seconds: 3,
            rounds: 1,
        };
        let cpu = Rlimit {
            current: Some(3),
            maximum: hard,
        };
        assert_eq!(
            burn.limits(),
            [(Resource::Cpu, cpu), (Resource::Core, both(0))]
        );
        let spawn = |max_processes| Limited::Spawn {
            max_processes,
            program: scratch("unused"),
            args: Vec::new(),
        };
        assert_eq!(spawn(Some(2)).limits(), [(Resource::Nproc, both(2))]);
        assert!(spawn(None).limits().is_empty());
    }

    /// Each limit mode's action, run here without its limits: what it prints on success and
    /// the error a failed action returns.
    #[test]
    fn each_limit_mode_acts_and_reports_what_it_did() {
        let allocate = Limited::Allocate {
            limit: 0,
            bytes: 16,
        };
        assert_eq!(allocate.act().unwrap(), "allocated=16\n");
        let path = scratch("act-write");
        let write = Limited::WriteBytes {
            limit: 0,
            bytes: 10,
            path: path.clone(),
        };
        assert_eq!(write.act().unwrap(), "wrote=10\n");
        assert_eq!(std::fs::read(&path).unwrap(), [0u8; 10]);
        std::fs::remove_file(&path).unwrap();
        let burn = Limited::Burn {
            seconds: 0,
            rounds: 1,
        };
        assert_eq!(burn.act().unwrap(), "burned=1\n");
        // This test binary lists its tests and exits 0.
        let me = std::env::current_exe().unwrap();
        let spawn = Limited::Spawn {
            max_processes: None,
            program: me,
            args: vec!["--list".to_owned()],
        };
        assert_eq!(spawn.act().unwrap(), "spawned exit status: 0\n");
        let missing = Limited::Spawn {
            max_processes: None,
            program: scratch("no-such-program"),
            args: Vec::new(),
        };
        assert_eq!(missing.act().unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    /// A limit mode whose action fails exits 1. A spawn with no limit sets nothing on this
    /// process, so it runs here.
    #[test]
    fn a_limit_mode_whose_action_fails_exits_one() {
        let missing = Child::Limited(Limited::Spawn {
            max_processes: None,
            program: scratch("no-such-program"),
            args: Vec::new(),
        });
        assert_eq!(run(&missing), ExitCode::FAILURE);
    }

    #[test]
    fn allocate_writes_every_byte_it_asks_for() {
        assert_eq!(allocate(4096).unwrap(), "allocated=4096\n");
        assert_eq!(allocate(0).unwrap(), "allocated=0\n");
    }

    #[test]
    fn an_allocation_too_large_to_reserve_is_an_error() {
        let err = allocate(usize::MAX).unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("cannot allocate {} bytes: ", usize::MAX)),
            "{err}"
        );
    }

    #[test]
    fn burn_does_the_rounds_it_is_given() {
        assert_eq!(burn(0), 0);
        assert_eq!(burn(2), 2);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn rusage_is_unsupported_off_macos() {
        let err = pid_usage_text(1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert_eq!(run(&Child::Rusage { pid: 1 }), ExitCode::FAILURE);
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
