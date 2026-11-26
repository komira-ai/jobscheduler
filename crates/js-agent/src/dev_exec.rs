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
use std::path::Path;
use std::process::ExitCode;

use js_mac_session::full_disk_access::{self, PRIVACY_DIR};

/// The verb.
pub const VERB: &str = "__dev-exec";

/// The status of a refused command line or a program that could not be started.
const USAGE: u8 = 2;

/// Runs the verb: `args` is the whole command line, `args[1]` the verb.
pub fn run(args: &[OsString], err: &mut dyn Write) -> ExitCode {
    let root = rustix::process::geteuid().is_root();
    run_with(args, err, root, Path::new(PRIVACY_DIR))
}

/// [`run`] with whether this process is root and the privacy database's directory given.
fn run_with(args: &[OsString], err: &mut dyn Write, root: bool, privacy: &Path) -> ExitCode {
    let rest = args.get(2..).unwrap_or_default();
    let Some((program, program_args)) = parse(rest) else {
        let _ = writeln!(err, "usage: js-agent {VERB} -- <program> [args...]");
        return ExitCode::from(USAGE);
    };
    if !root {
        let _ = writeln!(err, "js-agent: {VERB} runs as root only");
        return ExitCode::from(USAGE);
    }
    let listed = std::fs::read_dir(privacy).and_then(|mut dir| dir.next().transpose().map(|_| ()));
    let (state, why) = full_disk_access::judge(listed);
    let note = why.map(|why| format!(" ({why})")).unwrap_or_default();
    let _ = writeln!(err, "full disk access: {}{note}", state.as_str());
    let started = js_mac_session::disclaim::spawn_undisclaimed_for_proof(program, program_args);
    let mut child = match started {
        Ok(child) => child,
        Err(why) => {
            let shown = program.to_string_lossy();
            let _ = writeln!(err, "js-agent: {VERB}: cannot start {shown}: {why}");
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
    ExitCode::from(child.wait().map_or(USAGE, exit_code))
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
    // A status with neither a code nor a signal does not occur on Unix; it maps to 1.
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(-127));
    u8::try_from(code).unwrap_or(1)
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

    /// Asserts that a process that is not root is refused with status 2 before anything is
    /// read or started. Catches a verb that starts a program without root.
    #[test]
    fn a_process_that_is_not_root_is_refused() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "dev-exec-not-root");
        let mut err = Vec::new();
        let code = run_with(
            &os(&["js-agent", VERB, "--", "x"]),
            &mut err,
            false,
            dir.path(),
        );
        assert_eq!(code, ExitCode::from(USAGE));
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "js-agent: __dev-exec runs as root only\n"
        );
    }

    /// Asserts that, as root, the verb prints the agent's own Full Disk Access check (granted
    /// when the privacy directory can be listed, the error when it cannot), starts the program
    /// and exits with its status, and refuses a program it cannot start. Catches a verb that
    /// reports the grant wrongly or loses the child's status.
    #[test]
    fn as_root_it_checks_the_grant_and_runs_the_program() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "dev-exec-root");
        let me = std::env::current_exe().unwrap();
        let me = me.to_str().unwrap();
        let mut err = Vec::new();
        let line = os(&["js-agent", VERB, "--", me, "--exact", "no-such-test"]);
        assert_eq!(
            run_with(&line, &mut err, true, dir.path()),
            ExitCode::SUCCESS
        );
        let text = String::from_utf8(err).unwrap();
        assert!(text.starts_with("full disk access: granted\n"), "{text}");

        let missing = dir.path().join("missing");
        let mut err = Vec::new();
        let line = os(&["js-agent", VERB, "--", missing.to_str().unwrap()]);
        assert_eq!(
            run_with(&line, &mut err, true, &missing),
            ExitCode::from(USAGE)
        );
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("js-agent: __dev-exec: cannot start "),
            "{text}"
        );
        assert!(text.starts_with("full disk access: "), "{text}");
    }

    /// Asserts that the agent's command line hands `__dev-exec` and `__linux-vm` to their verbs
    /// before anything else runs: each answers its own usage with status 2.
    #[test]
    fn the_agent_dispatches_the_development_verbs() {
        let install = crate::SystemSleeper::install;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = crate::main_with(&install, os(&["js-agent", VERB]), &mut out, &mut err);
        assert_eq!(code, ExitCode::from(USAGE));
        assert!(
            String::from_utf8(err)
                .unwrap()
                .starts_with("usage: js-agent __dev-exec")
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let line = os(&["js-agent", crate::linux_vm::VERB, "bogus"]);
        assert_eq!(
            crate::main_with(&install, line, &mut out, &mut err),
            ExitCode::from(USAGE)
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "js-agent __linux-vm: unknown command \"bogus\"\n"
        );
    }
}
