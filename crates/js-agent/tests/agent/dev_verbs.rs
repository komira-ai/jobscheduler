//! The agent's binary answers the prototype's development verbs, `__dev-exec` and
//! `__linux-vm`, before it reads any flag: each refuses a command line it cannot run with its
//! own usage and status 2.

use std::process::{Command, Output};

/// The agent binary run with `args`.
fn agent(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args(args)
        .output()
        .expect("spawn")
}

/// Asserts that `__dev-exec` with no program, and `__linux-vm` with an unknown command, are
/// answered by their verbs (status 2, each verb's own message) and not by the agent's flag
/// parser. Catches a binary that drops a development verb from its dispatch.
#[test]
fn the_binary_answers_the_development_verbs() {
    let dev = agent(&["__dev-exec"]);
    assert_eq!(dev.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&dev.stderr),
        "usage: js-agent __dev-exec -- <program> [args...]\n"
    );
    let vm = agent(&["__linux-vm", "bogus"]);
    assert_eq!(vm.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&vm.stderr),
        "js-agent __linux-vm: unknown command \"bogus\"\n"
    );
}
