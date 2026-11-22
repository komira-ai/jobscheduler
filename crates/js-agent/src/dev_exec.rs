//! `js-agent __dev-exec -- <program> [args...]`: a development verb of the one-program
//! prototype, for one proof only, and removed before any release.
//!
//! The proof: a privacy profile grants Full Disk Access to the signed agent by its path and
//! its signature, and the agent, started by launchd as its own responsible process, holds it.
//! The verb shows that, then starts `program` as a plain child of the agent with its privacy
//! responsibility NOT disclaimed, so the child answers for the agent's grant (every start of
//! a job disclaims it; this verb is the one exception, and it runs no job). It prints, on
//! standard error:
//!
//! - `full disk access: <state>`: the agent's own check, a listing of the privacy database's
//!   directory, judged as the agent judges it;
//! - `responsible pid of the child: <pid> (agent pid <pid>)` on macOS, so the output shows
//!   whose grant the child uses;
//!
//! and passes the child's standard output and error through. It exits with the child's
//! status (128 + the signal when a signal ended it).
//!
//! It refuses (status 2) unless it runs as root, and when no program follows `--`.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use js_mac_session::full_disk_access::{self, PRIVACY_DIR};

/// The verb.
pub const VERB: &str = "__dev-exec";

/// The status of a refused command line or a program that could not be started.
const USAGE: u8 = 2;

/// Runs the verb: `args` is the whole command line, `args[1]` the verb.
pub fn run(args: &[OsString], err: &mut dyn Write) -> ExitCode {
    let rest = args.get(2..).unwrap_or_default();
    let Some((program, program_args)) = parse(rest) else {
        let _ = writeln!(err, "usage: js-agent {VERB} -- <program> [args...]");
        return ExitCode::from(USAGE);
    };
    if !rustix::process::geteuid().is_root() {
        let _ = writeln!(err, "js-agent: {VERB} runs as root only");
        return ExitCode::from(USAGE);
    }
    let listed =
        std::fs::read_dir(PRIVACY_DIR).and_then(|mut dir| dir.next().transpose().map(|_| ()));
    let (state, why) = full_disk_access::judge(listed);
    match why {
        None => {
            let _ = writeln!(err, "full disk access: {}", state.as_str());
        }
        Some(why) => {
            let _ = writeln!(err, "full disk access: {} ({why})", state.as_str());
        }
    }
    let mut child =
        match js_mac_session::disclaim::spawn_undisclaimed_for_proof(program, program_args) {
            Ok(child) => child,
            Err(why) => {
                let _ = writeln!(
                    err,
                    "js-agent: {VERB}: cannot start {}: {why}",
                    program.to_string_lossy()
                );
                return ExitCode::from(USAGE);
            }
        };
    #[cfg(target_os = "macos")]
    match js_mac_session::disclaim::responsible_pid(child.id()) {
        Ok(pid) => {
            let _ = writeln!(
                err,
                "responsible pid of the child: {pid} (agent pid {})",
                std::process::id()
            );
        }
        Err(why) => {
            let _ = writeln!(err, "responsible pid of the child: unknown ({why})");
        }
    }
    let _ = err.flush();
    match child.wait() {
        Ok(status) => ExitCode::from(exit_code(status)),
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: waiting for the program: {why}");
            ExitCode::from(USAGE)
        }
    }
}

/// The program and its arguments after `--`, or `None` when there is no `--` first or no
/// program after it.
fn parse(rest: &[OsString]) -> Option<(&OsString, &[OsString])> {
    let (dashes, after) = rest.split_first()?;
    if dashes != "--" {
        return None;
    }
    after.split_first()
}

/// The status to exit with for the child's: its code, or 128 + the signal that ended it.
fn exit_code(status: std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => u8::try_from(code).unwrap_or(1),
        (None, Some(signal)) => u8::try_from(128 + signal).unwrap_or(1),
        (None, None) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt as _;

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    /// Asserts that the program and its arguments are what follows `--`, and that a line
    /// without `--` first or without a program after it is refused. Catches a verb that runs
    /// its first argument without the `--`, or runs nothing and exits 0.
    #[test]
    fn the_program_is_what_follows_the_dashes() {
        let line = os(&["--", "/bin/bash", "script.sh", "x"]);
        let (program, args) = parse(&line).unwrap();
        assert_eq!(program, "/bin/bash");
        assert_eq!(args, os(&["script.sh", "x"]).as_slice());
        let line = os(&["--", "/bin/true"]);
        assert_eq!(parse(&line).unwrap().1, &[] as &[OsString]);
        assert!(parse(&os(&[])).is_none());
        assert!(parse(&os(&["--"])).is_none());
        assert!(parse(&os(&["/bin/bash"])).is_none());
    }

    /// Asserts that the verb exits with the child's code, and with 128 + the signal when a
    /// signal ended it. Catches a verb that reports success for a script that failed.
    #[test]
    fn the_status_is_the_childs() {
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(1 << 8)), 1);
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code(std::process::ExitStatus::from_raw(9)), 137);
    }

    /// Asserts that a command line without a program is refused with status 2 and the usage,
    /// before anything else is looked at. Catches a verb that starts nothing and exits 0.
    #[test]
    fn a_line_without_a_program_is_refused() {
        let mut err = Vec::new();
        let code = run(&os(&["js-agent", VERB]), &mut err);
        assert_eq!(code, ExitCode::from(USAGE));
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "usage: js-agent __dev-exec -- <program> [args...]\n"
        );
    }
}
