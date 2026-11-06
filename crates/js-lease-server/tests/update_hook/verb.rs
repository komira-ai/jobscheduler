//! The verb as the process runs it: `js_lease_server::run` and the binary answer
//! `update-hook` over this machine's ports, before any server flag is read. These tests read
//! the declaration path of the machine they run on, so they need it to be empty.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use js_lease_server::update_hook::{DECLARATION_PATH, VERB};
use js_testkit::dir::TestDir;
use serde_json::Value;

/// The tests that run the verb over the machine's declaration path need that path empty.
fn assert_no_declaration_installed() {
    assert!(
        !Path::new(DECLARATION_PATH).exists(),
        "this test needs a machine with no declaration installed"
    );
}

/// A params file of an update in a directory of the test's own.
fn params_file(name: &str) -> (TestDir, String) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let file = dir.path().join("params.toml");
    std::fs::write(
        &file,
        "\"updater.staged_release\" = \"/opt/js-lease-server/releases/2.0.0\"\n\"updater.target_version\" = \"2.0.0\"\ndefer_retry_secs = 60\n",
    )
    .unwrap();
    let file = file.to_str().unwrap().to_owned();
    (dir, file)
}

fn last_line(out: &[u8]) -> Value {
    let text = String::from_utf8(out.to_vec()).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

/// Asserts that `js_lease_server::run` given `update-hook` runs the step over this machine
/// (here: no declaration installed, so `state_invalid` naming the declaration, exit 1) and
/// not the server: no usage text, nothing on standard error. Catches a verb parsed as the
/// server's flags, or a machine step that does not read the declaration first.
#[test]
fn run_answers_the_verb_over_the_machine_before_the_servers_flags() {
    assert_no_declaration_installed();
    let (_dir, params) = params_file("verb-run-machine");
    let args: Vec<OsString> = ["js-lease-server", VERB, "cleanup", "--params", &params]
        .into_iter()
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = js_lease_server::run(args, &mut out, &mut err);
    assert_eq!(
        format!("{code:?}"),
        format!("{:?}", std::process::ExitCode::from(1))
    );
    assert_eq!(String::from_utf8(err).unwrap(), "");
    let line = last_line(&out);
    assert_eq!(line["reason"], "state_invalid");
    assert_eq!(line["class"], "release");
    assert_eq!(line["reversible"], true);
    assert!(
        line["detail"].as_str().unwrap().contains("declaration"),
        "{line}"
    );
}

/// Asserts that the binary prints one valid last line and exits 1 for a step on a machine with
/// no declaration, and that standard error is empty. Catches a verb the binary does not reach.
#[test]
fn the_binary_prints_one_valid_last_line_and_exits_1() {
    assert_no_declaration_installed();
    let (_dir, params) = params_file("verb-binary");
    let output = Command::new(env!("CARGO_BIN_EXE_js-lease-server"))
        .args([
            VERB,
            "prepare",
            "/opt/js-lease-server/releases/2.0.0",
            "--params",
            &params,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    let line = last_line(&output.stdout);
    assert_eq!(line["reason"], "state_invalid");
    assert_eq!(line["promotion_pending"], false);
}
