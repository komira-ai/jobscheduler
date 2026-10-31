//! `cargo xtask count-run --count-file <path> -- <test binary> <args>...`: what a `crun-tests`
//! unit runs in place of the test binary, so the verb that reads the unit's result also
//! learns how many tests ran. A unit's output goes to the log through `systemd-run --pipe`,
//! and a binary that ran no test exits 0 like one that ran them all.
//!
//! The wrapper runs the binary, passes its stdout through line by line, adds up the
//! `passed` counts of its `test result:` lines (libtest prints one per test binary, and
//! one per doctest run) and writes the sum to the count file, then exits with the binary's
//! own status. [`read_count`] is the other side: the verb reads the file after the unit ends.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::Error;

/// The `passed` count of one line of a test binary's output: `Some(n)` for
/// `test result: ok. n passed; ...` (or `FAILED.`), `None` for any other line.
pub fn passed_in(line: &str) -> Option<u64> {
    let (_, summary) = line.strip_prefix("test result: ")?.split_once(". ")?;
    summary
        .split("; ")
        .find_map(|part| part.strip_suffix(" passed")?.parse().ok())
}

/// The number of tests a count file records, which must be at least one.
///
/// # Errors
/// The file cannot be read, is not a decimal count, or counts no test: the reason.
pub fn read_count(file: &Path) -> Result<u64, String> {
    let text = std::fs::read_to_string(file).map_err(|e| {
        format!(
            "could not read the count of tests run from {}: {e}",
            file.display()
        )
    })?;
    let count: u64 = text.trim().parse().map_err(|_| {
        format!(
            "{} is not a count of tests: {:?}",
            file.display(),
            text.trim()
        )
    })?;
    if count == 0 {
        return Err(format!(
            "the unit ran no test (the count in {} is 0)",
            file.display()
        ));
    }
    Ok(count)
}

/// Runs `command`, passes its stdout through, and writes the count of tests it reported to
/// `count_file`.
///
/// # Errors
/// The command is empty or cannot be started, or the file cannot be written. A command that
/// ends with a failure status is [`Error::Child`] with that status, after the count is written.
pub fn run(count_file: &Path, command: &[OsString]) -> Result<String, Error> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| Error::Usage("count-run needs a command after `--`".to_owned()))?;
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Failed(format!("{}: {e}", program.to_string_lossy())))?;
    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut stdout = std::io::stdout().lock();
    let (mut count, mut line) = (0u64, Vec::new());
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::Failed(format!("reading the output of the tests: {e}")))?;
        if n == 0 {
            break;
        }
        count += passed_in(&String::from_utf8_lossy(&line)).unwrap_or(0);
        let _ = stdout.write_all(&line).and_then(|()| stdout.flush());
    }
    let status = child
        .wait()
        .map_err(|e| Error::Failed(format!("waiting for the tests: {e}")))?;
    std::fs::write(count_file, format!("{count}\n"))
        .map_err(|e| Error::Failed(format!("could not write {}: {e}", count_file.display())))?;
    if status.success() {
        return Ok(format!("{count} tests ran"));
    }
    Err(Error::Child(exit_code(&status)))
}

/// The exit code a failed child maps to: its own, or 128 plus the signal that ended it.
fn exit_code(status: &std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => u8::try_from(code).unwrap_or(1).max(1),
        (None, Some(signal)) => u8::try_from(128 + signal).unwrap_or(1),
        (None, None) => 1,
    }
}

#[cfg(test)]
mod tests;
