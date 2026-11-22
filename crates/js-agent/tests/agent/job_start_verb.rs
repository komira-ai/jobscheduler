//! `main_with` hands the start mode's verb to it before anything else of the agent starts.

use crate::support::code;

/// `js-agent __job-start` is answered by `main_with` before the signal handlers are installed:
/// a line with no `--` is refused with the mode's own message and status 2, and nothing is
/// printed to standard output.
///
/// It catches the verb falling through to the agent's own flags (a different message), or the
/// agent starting its signal handlers and its work before the mode runs.
#[test]
fn main_with_answers_the_start_verb_before_it_installs_the_signal_handlers() {
    let installed = std::cell::Cell::new(false);
    let install = || {
        installed.set(true);
        Err(std::io::Error::other(
            "no signal handlers for the start mode",
        ))
    };
    let args: Vec<std::ffi::OsString> = vec!["js-agent".into(), "__job-start".into()];
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = js_agent::main_with(&install, args, &mut stdout, &mut stderr);
    assert!(!installed.get(), "the verb installed the signal handlers");
    assert_eq!(format!("{status:?}"), code(2));
    assert!(stdout.is_empty());
    assert_eq!(
        String::from_utf8(stderr).expect("utf-8"),
        "js-agent: __job-start: no -- before the program\n\
         usage: js-agent __job-start --profile TEXT [--param NAME=VALUE]... -- PROGRAM [ARG]...\n"
    );
}
