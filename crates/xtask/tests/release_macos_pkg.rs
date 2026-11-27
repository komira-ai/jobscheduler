//! `cargo xtask release macos-pkg` as the black-box workflow runs it: its flags and exit codes.
//! What it downloads and checks is the unit tests' (`src/release/macos_pkg_tests.rs`); nothing
//! here reaches the network.

use std::process::{Command, Output};

use js_testkit::dir::TestDir;

fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("xtask")
}

/// Asserts that a version that is not `MAJOR.MINOR.PATCH` ends the verb with status 1, naming
/// it, before any download and with nothing written; and that the verb without `--version` or
/// `--below`, and `--below` without `--tags`, are usage errors (status 2).
/// Catches a verb that downloads from a URL made of an unchecked version.
#[test]
fn the_verb_refuses_a_bad_version_and_flags_that_do_not_fit() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "xtask-release-macos-pkg");
    let out_dir = dir.path().join("dist");
    let output = dir.path().join("output");
    let (out_dir, output) = (out_dir.to_str().unwrap(), output.to_str().unwrap());
    let base = ["release", "macos-pkg", "--out", out_dir, "--output", output];

    let ran = xtask(&[&base[..], &["--version", "0.4"]].concat());
    assert_eq!(ran.status.code(), Some(1), "{ran:?}");
    assert_eq!(
        String::from_utf8_lossy(&ran.stderr),
        "xtask: not a version MAJOR.MINOR.PATCH: \"0.4\"\n"
    );
    assert!(!dir.path().join("dist").exists());
    assert!(!dir.path().join("output").exists());

    for extra in [&[][..], &["--below", "0.4.2"][..]] {
        let ran = xtask(&[&base[..], extra].concat());
        assert_eq!(ran.status.code(), Some(2), "{extra:?}: {ran:?}");
    }
}
