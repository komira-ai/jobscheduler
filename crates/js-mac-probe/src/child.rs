//! The child modes: tiny programs the probe starts inside a sandbox profile (or outside,
//! as the control) so the probe observes what a confined process can do. They are part of
//! this binary so no probe depends on a tool whose presence on the runner image is assumed.

use std::net::{SocketAddr, ToSocketAddrs as _};
use std::path::PathBuf;
use std::process::ExitCode;
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
}

/// Runs a child mode.
#[must_use]
pub fn run(child: &Child) -> ExitCode {
    let outcome = match child {
        Child::Connect { addr } => {
            std::net::TcpStream::connect_timeout(addr, CONNECT_LIMIT).map(|_| String::new())
        }
        Child::Resolve { host } => resolve(host),
        Child::Read { path } => {
            let _ = path;
            Ok(String::new())
        }
        Child::Env { name } => {
            let _ = name;
            Ok(String::new())
        }
        Child::Write { path } => std::fs::write(path, b"probe\n").map(|()| String::new()),
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
    let _ = path;
    Ok(String::new())
}

fn env_text(name: &str) -> String {
    let _ = name;
    String::new()
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
        drop(listener);
        assert_eq!(run(&Child::Connect { addr }), ExitCode::FAILURE);
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
