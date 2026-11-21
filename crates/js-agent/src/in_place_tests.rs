//! The run-again ending: the line it prints, the arguments it passes, and what a failed exec
//! ends with.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use super::{exec, run_again};

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

#[test]
fn running_again_says_so_and_passes_the_arguments_as_given() {
    let given = args(&["/usr/bin/js-agent", "--flags-file", "/etc/js-agent.flags"]);
    let seen = RefCell::new(Vec::new());
    let mut err = Vec::new();
    let again = |words: &[OsString]| {
        seen.borrow_mut().extend_from_slice(words);
        io::Error::other("stand-in")
    };
    let _ = run_again(&given, &again, &mut err);
    assert_eq!(*seen.borrow(), given);
    let said = String::from_utf8(err).expect("utf8");
    assert!(
        said.starts_with("js-agent: handed over in place: running /usr/bin/js-agent again\n"),
        "{said}"
    );
}

#[test]
fn an_exec_that_returns_ends_with_status_one_and_says_why() {
    let given = args(&["/usr/bin/js-agent"]);
    let mut err = Vec::new();
    let code = run_again(&given, &|_| io::Error::other("stand-in"), &mut err);
    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        String::from_utf8(err).expect("utf8"),
        "js-agent: handed over in place: running /usr/bin/js-agent again\n\
         js-agent: cannot run /usr/bin/js-agent again in place: stand-in; the runs are left to \
         the next process\n"
    );
}

#[test]
fn a_command_line_with_no_program_is_an_error_and_names_none() {
    let mut err = Vec::new();
    let code = run_again(&[], &exec, &mut err);
    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        String::from_utf8(err).expect("utf8"),
        "js-agent: handed over in place: running  again\n\
         js-agent: cannot run  again in place: there is no program name; the runs are left to \
         the next process\n"
    );
}

#[test]
fn the_real_exec_of_a_program_that_does_not_exist_returns_the_error() {
    let missing = std::env::temp_dir().join("js-agent-no-such-program-for-exec");
    let why = exec(&[missing.into_os_string(), OsString::from("--flag")]);
    assert_eq!(why.kind(), io::ErrorKind::NotFound);
}
