//! The test-only hook binary over the fake ports, started as the conformance harness starts
//! it: a process, the fakes' directories on its command line or in its variables, a params
//! file. It runs the hook's own steps.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use js_testkit::dir::TestDir;

use super::fakes::harness::{self, CONTROL_VAR, STATE_DIR_VAR, WORLD_VAR};

const BIN: &str = env!("CARGO_BIN_EXE_js-update-hook-fake");
const RESERVED: &str = concat!(
    "\"updater.artifact_digest\" = \"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
    "\"updater.staged_release\" = \"/staged\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
);

fn run(args: &[&str], envs: &[(&str, &str)]) -> (Option<i32>, String, String) {
    // A group of its own, as the harness starts it: a watchdog that fires kills only the
    // binary's group, never the test's.
    let out = Command::new(BIN)
        .process_group(0)
        .args(args)
        .env_clear()
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

struct Dirs {
    _tmp: TestDir,
    world: String,
    control: String,
    state: String,
    params: String,
}

fn rig(name: &str) -> Dirs {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let path = |leaf: &str| tmp.path().join(leaf).to_str().unwrap().to_owned();
    std::fs::write(
        tmp.path().join("params.toml"),
        format!("{}{RESERVED}", harness::params_text()),
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("state")).unwrap();
    Dirs {
        world: path("world"),
        control: path("control"),
        state: path("state"),
        params: path("params.toml"),
        _tmp: tmp,
    }
}

/// The one last line, parsed.
fn last_line(out: &str) -> serde_json::Value {
    assert!(out.ends_with('\n'), "{out:?}");
    assert_eq!(out.trim_end().lines().count(), 1, "{out:?}");
    serde_json::from_str(out.trim_end()).unwrap()
}

#[test]
fn a_step_over_the_fake_ports_runs_the_hooks_own_body_and_ends_with_its_last_line() {
    let dirs = rig("hook-fake-bin");
    let flags = [
        "--world",
        dirs.world.as_str(),
        "--control",
        &dirs.control,
        "--state-dir",
        &dirs.state,
        "update-hook",
    ];
    // `cleanup` with no record has finished before: success, and it says so.
    let (code, out, _) = run(
        &[&flags[..], &["cleanup", "--params", &dirs.params]].concat(),
        &[],
    );
    assert_eq!(code, Some(0));
    let line = last_line(&out);
    assert_eq!(line["detail"], "there is no record: cleanup has finished");
    // `verify` with no record is the step's own refusal, exit 1.
    let (code, out, _) = run(
        &[&flags[..], &["verify", "--params", &dirs.params]].concat(),
        &[],
    );
    assert_eq!(code, Some(1));
    assert_eq!(last_line(&out)["reason"], "state_invalid");
    // The fakes' directories were made for the process; the step took its lock in the state
    // directory.
    assert!(Path::new(&dirs.world).join("units").is_dir());
    assert!(Path::new(&dirs.control).is_dir());
    assert!(Path::new(&dirs.state).join("lock").is_file());
}

#[test]
fn the_directories_may_come_from_the_variables_of_the_test_binary() {
    let dirs = rig("hook-fake-bin-env");
    let envs = [
        (WORLD_VAR, dirs.world.as_str()),
        (CONTROL_VAR, dirs.control.as_str()),
        (STATE_DIR_VAR, dirs.state.as_str()),
    ];
    let (code, out, _) = run(&["update-hook", "verify", "--params", &dirs.params], &envs);
    assert_eq!(code, Some(1));
    assert_eq!(last_line(&out)["reason"], "state_invalid");
}

#[test]
fn a_state_directory_that_is_not_plain_fails_the_step_before_it_runs() {
    let dirs = rig("hook-fake-bin-relative");
    let (code, out, _) = run(
        &[
            "--world",
            &dirs.world,
            "--control",
            &dirs.control,
            "--state-dir",
            "relative/state",
            "update-hook",
            "verify",
            "--params",
            &dirs.params,
        ],
        &[],
    );
    assert_eq!(code, Some(1));
    let line = last_line(&out);
    assert_eq!(line["reason"], "state_invalid");
    assert!(
        line["detail"]
            .as_str()
            .unwrap()
            .contains("is not an absolute path of plain components"),
        "{line}"
    );
}

#[test]
fn without_the_directories_it_exits_2_and_says_what_is_missing() {
    let dirs = rig("hook-fake-bin-missing");
    let (code, out, err) = run(
        &[
            "--world",
            &dirs.world,
            "--control",
            &dirs.control,
            "update-hook",
            "verify",
            "--params",
            &dirs.params,
        ],
        &[],
    );
    assert_eq!(code, Some(2));
    assert_eq!(out, "");
    assert_eq!(
        err,
        "js-update-hook-fake: --world, --control and --state-dir (or their variables) are required\n"
    );
    // A flag with no value after it is not a directory either.
    let (code, ..) = run(&["update-hook", "verify", "--world"], &[]);
    assert_eq!(code, Some(2));
}
