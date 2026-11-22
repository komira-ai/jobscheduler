//! `js-agent __job-start --profile TEXT [--param NAME=VALUE]... -- PROGRAM [ARG]...`: the
//! program the helper starts for each native run. It applies the run's Seatbelt profile in
//! its own process, with `sandbox_init_with_parameters`, and then executes the run's program
//! in its place, so the program and everything it starts run under the profile and no second
//! program (`sandbox-exec`) stands between the helper and the job.
//!
//! The command line is [`js_mac_pool::seatbelt::Profile::start_argv`]'s; the grammar is
//! [`StartArgs`]. The mode is answered first, before any other verb of the agent and before
//! the signal handlers: it is a fresh process of one thread that becomes the job.
//!
//! Exit status: 2 for a command line that is not well formed (and for any line off macOS,
//! where there is no Seatbelt), 125 when the profile is refused (the program has not run),
//! 127 when the program cannot be executed. The program's own status is its own.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use js_mac_pool::seatbelt::{START_VERB, StartArgs};

#[cfg(target_os = "macos")]
mod macos;

/// The verb: `js-agent __job-start`.
pub const VERB: &str = START_VERB;

/// The status of a command line that is not well formed, and of any line off macOS.
const USAGE: u8 = 2;

/// The usage line printed after a refused command line.
const USAGE_LINE: &str =
    "usage: js-agent __job-start --profile TEXT [--param NAME=VALUE]... -- PROGRAM [ARG]...";

/// Answers `args` (the program name first) when its first word is [`VERB`]: `None` for any
/// other line, which is not this mode's.
pub fn dispatch(args: &[OsString], err: &mut dyn Write) -> Option<ExitCode> {
    match args {
        [_, verb, rest @ ..] if verb.to_str() == Some(VERB) => Some(run(rest, err)),
        _ => None,
    }
}

/// The mode on the words after the verb: parses them, then applies the profile and executes
/// the program on macOS.
fn run(rest: &[OsString], err: &mut dyn Write) -> ExitCode {
    match StartArgs::parse(rest) {
        Ok(start) => start_on_this_platform(&start, err),
        Err(why) => refuse(err, &why.to_string()),
    }
}

/// Prints the refusal and the usage line; the status is 2.
fn refuse(err: &mut dyn Write, why: &str) -> ExitCode {
    let _ = writeln!(err, "js-agent: {VERB}: {why}\n{USAGE_LINE}");
    ExitCode::from(USAGE)
}

#[cfg(target_os = "macos")]
fn start_on_this_platform(start: &StartArgs, err: &mut dyn Write) -> ExitCode {
    macos::start(start, err)
}

/// Seatbelt is macOS's: nothing is applied and nothing is executed elsewhere.
#[cfg(not(target_os = "macos"))]
fn start_on_this_platform(_start: &StartArgs, err: &mut dyn Write) -> ExitCode {
    let _ = writeln!(err, "js-agent: {VERB} runs on macOS only");
    ExitCode::from(USAGE)
}

#[cfg(test)]
#[path = "job_start_tests.rs"]
mod tests;
