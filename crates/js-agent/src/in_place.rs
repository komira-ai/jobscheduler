//! The end of a handover in place: the program runs its own command line again in the same
//! process.
//!
//! The update hook, when the agent that is running and the one the package installed both list
//! `in-place` on the `stops:` line of `--version`, sends SIGHUP to the main process instead of
//! stopping the unit. The loop hands the runs over (see the `run` module) and ends with
//! [`Exit::Replaced`](crate::Exit::Replaced); once everything the agent held is dropped (its
//! claims, its worker, its listener), [`run_again`] runs `argv[0]` as it was started, with the
//! arguments as they were given (a `--flags-file` is not yet expanded, so the next program reads
//! the file again), over this process. The process id stays, so systemd places no process and the
//! unit stays active; the package replaced the program file, so the new program is the staged
//! release, which adopts the runs and removes the handover file.

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use js_mac_session::disclaim;

use crate::exit;

/// Runs the command line `args` (the program first) over this process, through the one
/// spawner ([`disclaim::exec`]). It returns only when that failed, with the error.
#[must_use]
pub fn exec(args: &[OsString]) -> io::Error {
    match args.split_first() {
        Some((program, rest)) => disclaim::exec(program, rest),
        None => io::Error::new(io::ErrorKind::InvalidInput, "there is no program name"),
    }
}

/// Runs `args` again through `exec` ([`exec`] outside a test), after a line to `err` that says
/// so. `exec` returns only when it failed: the line that says so is printed and the status is
/// [`exit::FAILED`], so that the supervisor's restart starts a new process, which adopts the
/// runs the hand-over left.
pub fn run_again(
    args: &[OsString],
    exec: &dyn Fn(&[OsString]) -> io::Error,
    err: &mut dyn Write,
) -> ExitCode {
    let program = args.first().map(|program| program.to_string_lossy());
    let program = program.as_deref().unwrap_or_default();
    let _ = writeln!(
        err,
        "js-agent: handed over in place: running {program} again"
    );
    let why = exec(args);
    let _ = writeln!(
        err,
        "js-agent: cannot run {program} again in place: {why}; the runs are left to the next \
         process"
    );
    ExitCode::from(exit::FAILED)
}

#[cfg(test)]
#[path = "in_place_tests.rs"]
mod tests;
