//! `js-isolation-probe`: a program that runs AS a native job on a Mac pool host and says what
//! a job can write, read, reach and leave behind, one line per probe
//! (`name: allowed | denied (ERRNO) | n/a ...; detail`), then compares every answer with the
//! answer pinned in `expected.toml` and fails by name. It is the acceptance job an operator
//! runs on a Mac after an install (docs/runbooks/mac-isolation-probe.md).
//!
//! It changes nothing it cannot put back: `probe` removes at once whatever it managed to make;
//! `leave` makes only markers that carry the run's token, and `find`, run as the next job on
//! the same slot, removes each one the reset left. It never prints the contents of a file it
//! opened, and it only connects and looks up the addresses it is given.
//!
//! - [`phase`]: the three phases against a [`phase::Host`];
//! - [`files`], [`net`], [`privilege`], [`limits`], [`marks`] (with [`ipc`]): the probes;
//! - [`expect`]: the pinned answers and the verdict;
//! - [`tools`]: the system tools, behind a trait;
//! - [`host`]: the account, the places and the `NAME=VALUE` arguments.
//!
//! The probes are written against an injected host and tested everywhere. Handing them the
//! real host (the account, the system's places, the tools) is macOS only: elsewhere the
//! phases refuse to run.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

pub mod expect;
pub mod files;
pub mod host;
pub mod ipc;
pub mod limits;
pub mod marks;
pub mod net;
pub mod outcome;
pub mod phase;
pub mod privilege;
pub mod tools;

#[cfg(target_os = "macos")]
mod macos;

use host::Named;
use phase::Phase;

/// Command line.
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Runs as a job on a Mac pool host and says what a job can write, read, reach and leave behind"
)]
pub struct Cli {
    /// The phase.
    #[command(subcommand)]
    pub command: Command,
}

/// The phases.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Try every write, read, connect, lookup and privilege, report the limits, and remove
    /// whatever was made.
    Probe(ProbeArgs),
    /// Make a marker of every kind the job is allowed to and keep it, for `find`.
    Leave(LeaveArgs),
    /// Look for the markers `leave` made; on the same slot, remove what is left.
    Find(FindArgs),
    /// The background process `leave` starts: start a session and wait.
    #[command(hide = true)]
    Linger(LingerArgs),
}

/// `probe`'s flags.
#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// A host-specific place to try writing in, `NAME=DIR` (another pool user's home, the
    /// bundle store); repeatable.
    #[arg(long = "write", value_parser = host::named)]
    pub writes: Vec<Named>,
    /// A host-specific path to try opening, `NAME=PATH` (the agent's flags, token and state,
    /// the helper's state, another service's key); repeatable.
    #[arg(long = "read", value_parser = host::named)]
    pub reads: Vec<Named>,
    /// An address to try one TCP connect to, `NAME=HOST:PORT`; repeatable.
    #[arg(long = "connect", value_parser = host::named)]
    pub connects: Vec<Named>,
    /// A host name to look up, `NAME=HOST`; repeatable.
    #[arg(long = "lookup", value_parser = host::named)]
    pub lookups: Vec<Named>,
}

/// `leave`'s flags.
#[derive(Debug, Args)]
pub struct LeaveArgs {
    /// The run's token, which every marker carries: 1 to 16 lower-case letters and digits.
    #[arg(long, value_parser = token)]
    pub token: String,
    /// A host-specific place to leave a file in, as `probe --write`; repeatable.
    #[arg(long = "write", value_parser = host::named)]
    pub writes: Vec<Named>,
    /// How long the background process waits before it ends by itself, in seconds.
    #[arg(long, default_value_t = 21_600)]
    pub linger_secs: u64,
}

/// `find`'s flags.
#[derive(Debug, Args)]
pub struct FindArgs {
    /// The token `leave` was given.
    #[arg(long, value_parser = token)]
    pub token: String,
    /// The places `leave` was given with `--write`.
    #[arg(long = "write", value_parser = host::named)]
    pub writes: Vec<Named>,
    /// The uid `leave` ran as (its output's `# next:` line says).
    #[arg(long)]
    pub left_by_uid: u32,
    /// The user `leave` ran as.
    #[arg(long)]
    pub left_by_user: String,
    /// That user's home.
    #[arg(long)]
    pub left_by_home: PathBuf,
}

/// `linger`'s flags.
#[derive(Debug, Args)]
pub struct LingerArgs {
    /// The run's token, so `find` can tell this process by its command line.
    #[arg(long, value_parser = token)]
    pub token: String,
    /// How long to wait before ending, in seconds.
    #[arg(long)]
    pub secs: u64,
}

/// A run's token: 1 to 16 lower-case letters and digits (a named semaphore's name holds 31
/// bytes on macOS).
///
/// # Errors
/// What is wrong with it.
pub fn token(arg: &str) -> Result<String, String> {
    let fits = (1..=16).contains(&arg.len())
        && arg
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if fits {
        Ok(arg.to_owned())
    } else {
        Err(format!(
            "{arg:?} is not a token: 1 to 16 lower-case letters and digits"
        ))
    }
}

impl Command {
    /// The phase this command runs, or `None` for `linger`.
    #[must_use]
    pub fn phase(&self) -> Option<Phase> {
        match self {
            Self::Probe(a) => Some(Phase::Probe {
                writes: a.writes.clone(),
                reads: a.reads.clone(),
                connects: a.connects.clone(),
                lookups: a.lookups.clone(),
            }),
            Self::Leave(a) => Some(Phase::Leave {
                token: a.token.clone(),
                writes: a.writes.clone(),
                linger_secs: a.linger_secs,
            }),
            Self::Find(a) => Some(Phase::Find {
                token: a.token.clone(),
                writes: a.writes.clone(),
                uid: a.left_by_uid,
                user: a.left_by_user.clone(),
                home: a.left_by_home.clone(),
            }),
            Self::Linger(_) => None,
        }
    }
}

/// Runs the command line: 0 when the phase answered as pinned, 1 when it did not, 2 when it
/// could not run.
#[must_use]
pub fn run(cli: &Cli) -> ExitCode {
    if let Command::Linger(args) = &cli.command {
        return linger(args.secs);
    }
    cli.command
        .phase()
        .map_or(ExitCode::from(2), |phase| on_this_host(&phase))
}

/// `linger`: leaves the job's session (so a stop of the job's process group misses it) and
/// waits `secs`. A failed `setsid` (the process leads its group already) is no reason to stop:
/// `find` tells by the process group whether it left.
fn linger(secs: u64) -> ExitCode {
    let _ = rustix::process::setsid();
    std::thread::sleep(std::time::Duration::from_secs(secs));
    ExitCode::SUCCESS
}

#[cfg(target_os = "macos")]
fn on_this_host(phase: &Phase) -> ExitCode {
    macos::run(phase)
}

#[cfg(not(target_os = "macos"))]
fn on_this_host(_: &Phase) -> ExitCode {
    eprintln!(
        "js-isolation-probe: the probes run as a job on a Mac; this is {}",
        std::env::consts::OS
    );
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_short_lower_case_letters_and_digits() {
        assert_eq!(token("probe1"), Ok("probe1".to_owned()));
        assert_eq!(token(&"a".repeat(16)), Ok("a".repeat(16)));
        for bad in ["", "Probe", "a-b", "a b", &"a".repeat(17)] {
            assert!(
                token(bad).unwrap_err().contains("is not a token"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn each_command_names_its_phase() {
        let cli = Cli::parse_from([
            "p",
            "probe",
            "--write",
            "bundle-store=/s",
            "--read",
            "agent-flags=/f",
            "--connect",
            "job-service=127.0.0.1:1",
            "--lookup",
            "public=example.org",
        ]);
        let one = |name: &str, value: &str| {
            vec![Named {
                name: name.to_owned(),
                value: value.to_owned(),
            }]
        };
        assert_eq!(
            cli.command.phase(),
            Some(Phase::Probe {
                writes: one("bundle-store", "/s"),
                reads: one("agent-flags", "/f"),
                connects: one("job-service", "127.0.0.1:1"),
                lookups: one("public", "example.org"),
            })
        );
        let cli = Cli::parse_from(["p", "leave", "--token", "t1"]);
        assert_eq!(
            cli.command.phase(),
            Some(Phase::Leave {
                token: "t1".to_owned(),
                writes: vec![],
                linger_secs: 21_600
            })
        );
        let cli = Cli::parse_from([
            "p",
            "find",
            "--token",
            "t1",
            "--left-by-uid",
            "600",
            "--left-by-user",
            "pool-0",
            "--left-by-home",
            "/Users/pool-0",
        ]);
        assert!(matches!(
            cli.command.phase(),
            Some(Phase::Find { uid: 600, .. })
        ));
        let cli = Cli::parse_from(["p", "linger", "--token", "t1", "--secs", "0"]);
        assert_eq!(cli.command.phase(), None);
    }

    #[test]
    fn a_bad_argument_is_refused_before_anything_runs() {
        for args in [
            &["p", "leave", "--token", "BAD"][..],
            &["p", "probe", "--write", "noequals"][..],
            &["p", "find", "--token", "t1"][..],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }
}
