//! The start mode's command line: which lines are its own, and what each refused line says.
//! What applying a profile does is run on a Mac (`tests/agent/native/job_start.rs`).

use std::ffi::OsString;
use std::process::ExitCode;

use super::{VERB, dispatch};

const USAGE: &str =
    "usage: js-agent __job-start --profile TEXT [--param NAME=VALUE]... -- PROGRAM [ARG]...";

fn line(words: &[&str]) -> Vec<OsString> {
    let mut args = vec![OsString::from("js-agent"), OsString::from(VERB)];
    args.extend(words.iter().map(OsString::from));
    args
}

fn same(code: ExitCode, status: u8) -> bool {
    format!("{code:?}") == format!("{:?}", ExitCode::from(status))
}

/// Runs the mode on `args`: its status and what it printed to standard error.
fn answer(args: &[OsString]) -> (ExitCode, String) {
    let mut err = Vec::new();
    let code = dispatch(args, &mut err).expect("the line is the start mode's");
    (code, String::from_utf8(err).expect("the mode prints text"))
}

/// A line that is refused: it exits 2 and says what is wrong, then how to say it.
fn refused(words: &[&str], why: &str) {
    let (code, text) = answer(&line(words));
    assert!(same(code, 2), "{words:?}: {code:?}");
    assert_eq!(text, format!("js-agent: __job-start: {why}\n{USAGE}\n"));
}

/// The verb is the word the profile's command line starts with.
///
/// It catches a mode that answers to another word than the one `Profile::start_argv` writes.
#[test]
fn the_verb_is_the_word_of_the_profiles_command_line() {
    assert_eq!(VERB, "__job-start");
}

/// Any other word, and a line with no word, is not the mode's: the agent goes on to its other
/// verbs and its flags.
///
/// It catches a mode that takes every line, or a line without a verb.
#[test]
fn a_line_of_another_word_is_not_the_start_modes() {
    let mut err = Vec::new();
    for args in [
        vec![OsString::from("js-agent")],
        vec![OsString::from("js-agent"), OsString::from("cordon")],
        vec![
            OsString::from("js-agent"),
            OsString::from("__job-start-not"),
        ],
        vec![OsString::from("js-agent"), OsString::from("--profile")],
        vec![],
    ] {
        assert!(dispatch(&args, &mut err).is_none(), "{args:?}");
    }
    assert!(err.is_empty());
}

/// A verb with nothing after it has no `--` and no program.
///
/// It catches a mode that runs a program, or panics, on a line with no arguments.
#[test]
fn a_bare_verb_is_refused() {
    refused(&[], "no -- before the program");
}

/// Each way a line can be wrong is refused with a message that names it, the usage line and
/// status 2, before anything is applied or run.
///
/// It catches a refusal that is silent, that has another status, or that runs the program
/// anyway.
#[test]
fn each_malformed_line_is_refused_with_what_is_wrong() {
    refused(&["--", "/bin/true"], "no --profile was given");
    refused(
        &["--profile", "a", "--profile", "b", "--", "/bin/true"],
        "--profile was given twice",
    );
    refused(&["--profile"], "--profile has no value");
    refused(
        &["--profile", "(version 1)", "--param"],
        "--param has no value",
    );
    refused(
        &[
            "--profile",
            "(version 1)",
            "--param",
            "=x",
            "--",
            "/bin/true",
        ],
        "--param =x is not NAME=VALUE with a name",
    );
    refused(
        &[
            "--profile",
            "(version 1)",
            "--param",
            "none",
            "--",
            "/bin/true",
        ],
        "--param none is not NAME=VALUE with a name",
    );
    refused(
        &["--profile", "(version 1)", "--silent", "--", "/bin/true"],
        "unknown option --silent",
    );
    refused(&["--profile", "(version 1)"], "no -- before the program");
    refused(&["--profile", "(version 1)", "--"], "no program after --");
    refused(
        &["--profile", "(version 1)\0", "--", "/bin/true"],
        "the profile text holds a NUL byte",
    );
}

/// A value that is not UTF-8 is refused by the option it belongs to.
///
/// It catches a mode that lossily converts a profile and applies a different one.
#[cfg(unix)]
#[test]
fn a_profile_that_is_not_text_is_refused() {
    use std::os::unix::ffi::OsStringExt as _;

    let mut args = line(&["--profile"]);
    args.push(OsString::from_vec(vec![0xff, 0xfe]));
    let (code, text) = answer(&args);
    assert!(same(code, 2));
    assert_eq!(
        text,
        format!("js-agent: __job-start: the value of --profile is not UTF-8 text\n{USAGE}\n")
    );
}

/// A well-formed line off macOS says so and exits 2: there is no Seatbelt to apply, and the
/// program is not run unsandboxed.
///
/// It catches a mode that executes the job's program without applying its profile.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_well_formed_line_off_macos_is_refused_without_running_the_program() {
    let (code, text) = answer(&line(&[
        "--profile",
        "(version 1)\n(allow default)\n",
        "--param",
        "HOME=/Users/x",
        "--",
        "/bin/echo",
        "ran",
    ]));
    assert!(same(code, 2));
    assert_eq!(text, "js-agent: __job-start runs on macOS only\n");
}
