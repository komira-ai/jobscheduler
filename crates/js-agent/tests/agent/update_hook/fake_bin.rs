//! The test-only hook binary over the fake ports, started as the conformance harness will
//! start it: a process, the fakes' directories on its command line, a params file.

use std::process::{Command, Stdio};

use js_testkit::dir::TestDir;

const BIN: &str = env!("CARGO_BIN_EXE_js-update-hook-fake");
const PARAMS: &str = concat!(
    "cordon_lease_secs = 300\ncordon_max_secs = 17355\n",
    "\"updater.staged_release\" = \"/staged\"\n",
    "\"updater.target_version\" = \"1.4.0\"\n",
);

fn run(args: &[&str], envs: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let out = Command::new(BIN)
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

fn rig(name: &str) -> (TestDir, String, String, String) {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let path = |leaf: &str| tmp.path().join(leaf).to_str().unwrap().to_owned();
    std::fs::write(tmp.path().join("params.toml"), PARAMS).unwrap();
    let (world, control, params) = (path("world"), path("control"), path("params.toml"));
    (tmp, world, control, params)
}

fn assert_stub(out: &str) {
    assert!(out.ends_with('\n'), "{out:?}");
    assert_eq!(out.trim_end().lines().count(), 1, "{out:?}");
    let line: serde_json::Value = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(line["reason"], "not_implemented", "{line}");
}

#[test]
fn a_step_over_the_fake_ports_ends_with_a_last_line_and_exit_1() {
    let (_tmp, world, control, params) = rig("hook-fake-bin");
    let (code, out, _) = run(
        &[
            "--world",
            &world,
            "--control",
            &control,
            "update-hook",
            "cleanup",
            "--params",
            &params,
        ],
        &[],
    );
    assert_eq!(code, Some(1));
    assert_stub(&out);
    // The fakes' directories were made for the process.
    assert!(std::path::Path::new(&world).join("units").is_dir());
    assert!(std::path::Path::new(&control).is_dir());
}

#[test]
fn the_directories_may_come_from_the_variables_of_the_test_binary() {
    let (_tmp, world, control, params) = rig("hook-fake-bin-env");
    let envs = [
        ("UPDATE_HOOK_FAKE_WORLD", world.as_str()),
        ("UPDATE_HOOK_FAKE_CONTROL", control.as_str()),
    ];
    let (code, out, _) = run(&["update-hook", "verify", "--params", &params], &envs);
    assert_eq!(code, Some(1));
    assert_stub(&out);
}

#[test]
fn without_the_directories_it_exits_2_and_says_what_is_missing() {
    let (_tmp, world, _control, params) = rig("hook-fake-bin-missing");
    let (code, out, err) = run(
        &[
            "--world",
            &world,
            "update-hook",
            "verify",
            "--params",
            &params,
        ],
        &[],
    );
    assert_eq!(code, Some(2));
    assert_eq!(out, "");
    assert!(err.contains("--world and --control"), "{err}");
    // A flag with no value after it is not a directory either.
    let (code, ..) = run(&["update-hook", "verify", "--world"], &[]);
    assert_eq!(code, Some(2));
}
