//! `js-guest-act STEP [; STEP]...`: the program this package's tests give the guest
//! agent to run as the command, in place of `sh -c`. Test-only: not part of what ships.
//!
//! It is the part of `kbf-testkit`'s `kbf-act` that these tests use, with the same step
//! names and meaning. Steps are separated by an argument that is exactly `;`, run in
//! order, and the program exits 0 after the last unless a step exits first. Every step
//! is checked before the first runs, so a malformed one exits 2 having done nothing; a
//! step that fails at run time exits 1.
//!
//! - `exit CODE`: exits with status `CODE` (0 to 255) at once;
//! - `sleep SECS`: sleeps `SECS` seconds, fractions allowed;
//! - `echo WORDS...`: prints the words, joined by spaces, and a newline on stdout;
//! - `append FILE WORDS...`: appends the words, joined by spaces, and a newline to
//!   `FILE`, creating it;
//! - `pwd`: prints the working directory;
//! - `kill-self SIGNAL`: sends itself signal number `SIGNAL`, and fails if it is still
//!   running afterwards (the signal was ignored or caught).

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

/// One step, checked.
#[derive(Debug, PartialEq)]
enum Step<'a> {
    Exit(u8),
    Sleep(Duration),
    Echo(String),
    Append { path: &'a str, text: String },
    Pwd,
    KillSelf(i32),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(run(&args))
}

/// The exit status of the steps in `args`.
fn run(args: &[String]) -> u8 {
    let steps = match parse(args) {
        Ok(steps) => steps,
        Err(e) => {
            eprintln!("js-guest-act: {e}");
            return 2;
        }
    };
    match perform(&steps) {
        Ok(status) => status.unwrap_or(0),
        Err(e) => {
            eprintln!("js-guest-act: {e}");
            1
        }
    }
}

/// The steps of `args`, split at each `;`, every one checked.
fn parse(args: &[String]) -> Result<Vec<Step<'_>>, String> {
    args.split(|a| a == ";").map(step).collect()
}

fn step(words: &[String]) -> Result<Step<'_>, String> {
    let strs: Vec<&str> = words.iter().map(String::as_str).collect();
    Ok(match strs.as_slice() {
        ["exit", code] => Step::Exit(
            code.parse()
                .map_err(|_| format!("{code:?} is not an exit status"))?,
        ),
        ["sleep", secs] => Step::Sleep(
            secs.parse::<f64>()
                .ok()
                .and_then(|s| Duration::try_from_secs_f64(s).ok())
                .ok_or_else(|| format!("{secs:?} is not a number of seconds"))?,
        ),
        ["echo", text @ ..] => Step::Echo(text.join(" ")),
        ["append", path, text @ ..] => Step::Append {
            path,
            text: text.join(" "),
        },
        ["pwd"] => Step::Pwd,
        ["kill-self", signal] => Step::KillSelf(
            signal
                .parse::<i32>()
                .ok()
                .filter(|s| (1..=31).contains(s))
                .ok_or_else(|| format!("{signal:?} is not a signal number"))?,
        ),
        _ => return Err(format!("bad step: {strs:?}")),
    })
}

/// Runs `steps` in order: `Some` status when one exits, `None` when all ran.
fn perform(steps: &[Step<'_>]) -> Result<Option<u8>, String> {
    for step in steps {
        match step {
            Step::Exit(code) => return Ok(Some(*code)),
            Step::Sleep(time) => std::thread::sleep(*time),
            Step::Echo(text) => print_line(text),
            Step::Append { path, text } => {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path)
                    .map_err(|e| format!("append {path}: {e}"))?;
                writeln!(file, "{text}").expect("write to a file just opened");
            }
            Step::Pwd => {
                let dir = std::env::current_dir().expect("the working directory exists");
                print_line(&dir.display().to_string());
            }
            Step::KillSelf(signal) => {
                // SAFETY: `kill` takes two integers and touches no memory; the pid is
                // this process's own and the signal is checked to be 1..=31.
                let sent = unsafe { libc::kill(libc::getpid(), *signal) };
                assert_eq!(sent, 0, "kill of this process by a valid signal");
                return Err(format!("still running after signal {signal}"));
            }
        }
    }
    Ok(None)
}

/// Writes `text` and a newline to stdout, flushed.
fn print_line(text: &str) {
    let mut out = std::io::stdout();
    writeln!(out, "{text}")
        .and_then(|()| out.flush())
        .expect("write to stdout");
}
