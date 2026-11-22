//! The run-again ending with the real exec, in a process of its own: the program runs in the
//! same process, with the arguments as given.

use std::ffi::OsString;
use std::io;

use js_agent::in_place::{exec, run_again};

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// The process [`the_real_exec_runs_the_program_in_this_process_with_the_arguments`] starts:
/// it says its pid, then runs a shell over itself through the real exec, as the agent runs its
/// program again. Ignored in a plain run: it ends the process it runs in.
#[test]
#[ignore = "run by its parent test as a process of its own: it replaces that process"]
fn exec_child_runs_a_shell_over_itself() {
    use std::io::Write as _;
    let mut out = io::stdout();
    writeln!(out, "before {}", std::process::id()).expect("stdout");
    out.flush().expect("stdout");
    let program = args(&[
        "/bin/sh",
        "-c",
        "echo \"after $$ $*\"",
        "sh",
        "--flags-file",
        "the flags",
    ]);
    let _ = run_again(&program, &exec, &mut io::stderr());
}

/// Asserts that the real exec runs the program in the same process, with the arguments as
/// given: a child process says its pid and runs a shell through [`run_again`](js_agent::in_place::run_again) with the real
/// [`exec`](js_agent::in_place::exec), and the shell says the same pid and the arguments; the line that it runs again comes
/// first, and none that it could not. Catches an exec that starts a new process (systemd would
/// see the main process end) or loses or reorders the arguments.
#[test]
fn the_real_exec_runs_the_program_in_this_process_with_the_arguments() {
    let test = std::env::current_exe().expect("the test binary");
    let ran = std::process::Command::new(test)
        .args([
            "--exact",
            "run_again::exec_child_runs_a_shell_over_itself",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("the child runs");
    let out = String::from_utf8(ran.stdout).expect("utf8");
    let err = String::from_utf8(ran.stderr).expect("utf8");
    assert!(ran.status.success(), "{out}{err}");
    // libtest's own `test <name> ... ` comes first on the line the child's pid is on.
    let pid = out
        .lines()
        .find_map(|line| line.split_once("before ").map(|(_, pid)| pid))
        .expect("the child says its pid");
    assert!(
        out.lines()
            .any(|line| line == format!("after {pid} --flags-file the flags")),
        "{out}"
    );
    assert!(
        err.contains("js-agent: handed over in place: running /bin/sh again\n"),
        "{err}"
    );
    assert!(!err.contains("cannot run"), "{err}");
}
