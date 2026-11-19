//! `js-agent clear-taint` through the program's entry: the verb is answered before the agent's
//! flags are parsed and before any signal handler is installed.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use js_testkit::dir::TestDir;
use serde_json::{Value, json};

/// Proves that `main_with` answers the verb over the machine's own unit manager: on a state
/// root with no taint it touches no unit, prints the hook contract's success line and exits
/// 0, and installs no signal handler. Catches the verb falling through to the agent's own
/// flags (exit 2, `--server` missing).
#[test]
fn main_with_answers_the_clear_taint_verb_before_it_installs_the_signal_handlers() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "clear-taint-verb");
    let state = dir.path().join("state");
    fs::create_dir(&state).expect("the state root");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("0700");
    let installed = Cell::new(false);
    let install = || {
        installed.set(true);
        Err(std::io::Error::other("no signal handlers for the verb"))
    };
    let args: Vec<OsString> = vec![
        "js-agent".into(),
        "clear-taint".into(),
        "--reason".into(),
        "a test".into(),
        "--by".into(),
        "ops".into(),
        "--state-root".into(),
        state.into(),
        "--scratch-root".into(),
        dir.path().join("scratch/runs").into(),
    ];
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = js_agent::main_with(&install, args, &mut stdout, &mut stderr);

    assert!(!installed.get(), "the verb installed the signal handlers");
    assert_eq!(format!("{status:?}"), crate::support::code(0));
    let stdout = String::from_utf8(stdout).expect("utf-8");
    let line: Value = serde_json::from_str(stdout.trim_end()).expect("one JSON line");
    assert_eq!(
        line,
        json!({
            "class": "release", "reversible": true, "promotion_pending": false,
            "detail": "the host is not tainted: there is no marker, pin file or owed record",
        })
    );
    assert!(stderr.is_empty());
}
