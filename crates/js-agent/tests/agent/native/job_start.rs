//! The agent's start mode (`js-agent __job-start`) on a Mac: it applies a Seatbelt profile to
//! its own process and executes the program there, so the program is held by the profile and
//! no `sandbox-exec` is involved. Run as the user running the tests; the profiles here are
//! the tests' own.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use crate::support::Scratch;

/// Writes below `DIR` alone.
const WRITES_BELOW_DIR: &str = "(version 1)\n(allow default)\n(deny file-write*)\n\
    (allow file-write* (subpath (param \"DIR\")) (literal \"/dev/null\"))\n";

/// Runs the start mode with `profile`, the parameters `params` and then `program`.
fn start(profile: &str, params: &[(&str, &Path)], program: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_js-agent"));
    command.args(["__job-start", "--profile", profile]);
    for (name, value) in params {
        command
            .arg("--param")
            .arg(format!("{name}={}", value.display()));
    }
    command.arg("--").args(program).output().unwrap()
}

/// Asserts that a profile that denies writes outside a directory lets the program write a
/// file inside it and refuses one outside it. It catches a start mode that executes the
/// program without applying the profile, or applies it to a process the program does not
/// run in.
#[test]
fn the_program_is_held_by_the_profile_the_start_mode_applied() {
    let dir = Scratch::new("job-start-write");
    let inside = fs::canonicalize(dir.path("")).unwrap();
    let other = Scratch::new("job-start-write-other");
    let outside = fs::canonicalize(other.path("")).unwrap();
    let write = |target: &Path| {
        let script = format!("echo data > {}/made.txt", target.display());
        start(
            WRITES_BELOW_DIR,
            &[("DIR", &inside)],
            &["/bin/sh", "-c", &script],
        )
    };
    let allowed = write(&inside);
    assert!(allowed.status.success(), "{allowed:?}");
    assert_eq!(
        fs::read_to_string(inside.join("made.txt")).unwrap(),
        "data\n"
    );
    let denied = write(&outside);
    assert!(!denied.status.success(), "{denied:?}");
    assert!(
        !outside.join("made.txt").exists(),
        "the profile let the program write outside its directory"
    );
}

/// Asserts that a profile the library refuses exits 125 with the library's message on the
/// `refused` line and does not run the program. It catches a start mode that goes on to the
/// program after a profile that was not applied.
#[test]
fn a_refused_profile_exits_125_before_the_program_runs() {
    let dir = Scratch::new("job-start-refused");
    let marker = fs::canonicalize(dir.path("")).unwrap().join("ran");
    let out = start(
        "(version 1)\n(this-is-not-a-rule)\n",
        &[],
        &["/usr/bin/touch", &marker.display().to_string()],
    );
    assert_eq!(out.status.code(), Some(125), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("js-agent: the Seatbelt profile was refused: "),
        "{err}"
    );
    assert!(!marker.exists(), "the program ran under a refused profile");
}

/// Asserts that a program that does not exist exits 127 with a line naming it and why. It
/// catches a start mode that exits 0 or with the usage status when the exec fails.
#[test]
fn a_missing_program_exits_127_naming_it() {
    let out = start(
        "(version 1)\n(allow default)\n",
        &[],
        &["/nonexistent/js-program"],
    );
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("js-agent: executing /nonexistent/js-program: "),
        "{err}"
    );
}
