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
    /// Print the reader's effective uid and `proc_pid_rusage` of process `pid`:
    /// `reader_uid=<uid> user_time=<ticks> system_time=<ticks> resident=<bytes>
    /// footprint=<bytes>`. macOS only; elsewhere it fails as unsupported.
    Rusage {
        /// The process.
        pid: i32,
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
        Child::Allocate { limit, bytes } => {
            set_limits(&[(Resource::As, both(*limit)), (Resource::Data, both(*limit))])
                .and_then(|()| allocate(*bytes))
        }
        Child::WriteBytes { limit, bytes, path } => {
            set_limits(&[(Resource::Fsize, both(*limit)), (Resource::Core, both(0))])
                .and_then(|()| write_bytes(path, *bytes))
        }
        Child::Burn { seconds, rounds } => {
            let hard = rustix::process::getrlimit(Resource::Cpu).maximum;
            let cpu = Rlimit {
                current: Some(*seconds),
                maximum: hard,
            };
            set_limits(&[(Resource::Cpu, cpu), (Resource::Core, both(0))])
                .map(|()| format!("burned={}\n", burn(*rounds)))
        }
        Child::Spawn {
            max_processes,
            program,
            args,
        } => {
            let limits: Vec<_> = max_processes
                .map(|n| (Resource::Nproc, both(n)))
                .into_iter()
                .collect();
            set_limits(&limits).and_then(|()| spawn(program, args))
        }
        Child::Rusage { pid } => pid_usage_text(*pid),
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
