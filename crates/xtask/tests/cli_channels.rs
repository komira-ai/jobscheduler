//! `cargo xtask channels ...` through the built program: the exit status of each kind of end
//! (a pass, a failure, a usage error), what is printed where, and the verbs that need no program
//! outside. Nothing here reaches a network, a cloud or a signing key: the programs the verbs
//! run are paths of files that are not there.

use std::path::Path;
use std::process::{Command, Output};

use js_testkit::dir::TestDir;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// A fake bearer for the runner's token request; it must never be printed.
const BEARER: &str = "runner-bearer-for-the-test";

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// Runs `xtask channels <args>` with the runner's token variables unset.
fn channels(args: &[&str]) -> Output {
    Command::new(XTASK)
        .arg("channels")
        .args(args)
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .output()
        .expect("run xtask")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

fn code(out: &Output) -> Option<i32> {
    out.status.code()
}

#[test]
fn a_gate_with_no_setting_set_is_green_and_says_so_on_stdout() {
    // Catches: an unset repository variable turning the job red, or the notice going to stderr
    // where the runner does not read it; `enabled=false` is the job's output.
    let dir = scratch("gate-none");
    let output = dir.join("output");
    let out = channels(&[
        "gate",
        "--label",
        "testing-apt-read",
        "--setting",
        "APT_BUCKET=",
        "--output",
        output.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        "::notice title=The testing-apt-read job is off::None of its settings (APT_BUCKET) is set, so it writes nothing and ends green.\n"
    );
    assert_eq!(text(&out.stderr), "");
    assert_eq!(
        std::fs::read_to_string(&output).expect("output"),
        "enabled=false\n"
    );
}

#[test]
fn a_gate_with_some_settings_set_is_a_failure_naming_the_missing_ones() {
    // Catches: a half-made configuration left silently off.
    let out = channels(&[
        "gate",
        "--label",
        "j",
        "--setting",
        "A=set",
        "--setting",
        "B=",
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        "xtask: j: A set but B not: set all of them or none, so that a channel is never left half on\n"
    );
}

#[test]
fn a_setting_that_is_not_name_value_is_a_usage_error_with_status_2() {
    // Catches: a usage error of a channels verb exiting 1 like a failure.
    let out = channels(&["gate", "--label", "j", "--setting", "no-equals"]);
    assert_eq!(code(&out), Some(2), "{out:?}");
    assert_eq!(
        text(&out.stderr),
        "xtask: --setting \"no-equals\" is not NAME=VALUE\n"
    );
}

#[test]
fn flags_clap_refuses_are_usage_errors_too() {
    // Catches: a write that names a pool and none (or half of one), and a suite that is not one.
    for args in [
        &[
            "apt",
            "write",
            "--bucket",
            "b",
            "--distribution-id",
            "E1",
            "--hostname",
            "h",
            "--next",
            "n",
            "--debs",
            "d",
        ][..],
        &[
            "apt",
            "write",
            "--bucket",
            "b",
            "--distribution-id",
            "E1",
            "--hostname",
            "h",
            "--next",
            "n",
            "--no-pool",
            "--debs",
            "d",
            "--version",
            "1.2.3",
        ][..],
        &[
            "apt", "read", "--suite", "beta", "--bucket", "b", "--out", "o",
        ][..],
    ] {
        let out = channels(args);
        assert_eq!(code(&out), Some(2), "{args:?}: {out:?}");
    }
}

#[test]
fn assume_without_the_runners_token_variables_fails_before_anything_is_written() {
    // Catches: a job without `id-token: write` going on with an empty token.
    let dir = scratch("assume-none");
    let env_file = dir.join("env");
    let out = channels(&[
        "assume",
        "--role-arn",
        "arn:aws:iam::123456789012:role/r",
        "--session-name",
        "jobscheduler-testing-apt-read-1",
        "--region",
        "us-east-1",
        "--env-file",
        env_file.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        "xtask: the runner offers no OIDC token (the job needs `permissions: id-token: write`)\n"
    );
    assert!(!env_file.exists());
}

#[test]
fn assume_masks_the_role_and_the_bearer_and_refuses_a_token_request_over_plain_http() {
    // Catches: the bearer sent in the clear, and a secret printed in the failure.
    let dir = scratch("assume-http");
    let env_file = dir.join("env");
    let out = Command::new(XTASK)
        .args([
            "channels",
            "assume",
            "--role-arn",
            "arn:aws:iam::123456789012:role/r",
            "--session-name",
            "jobscheduler-testing-apt-read-1",
            "--region",
            "us-east-1",
            "--env-file",
        ])
        .arg(&env_file)
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", "http://127.0.0.1:9/token")
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", BEARER)
        .output()
        .expect("run xtask");
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        format!("::add-mask::arn:aws:iam::123456789012:role/r\n::add-mask::{BEARER}\n")
    );
    let err = text(&out.stderr);
    assert!(
        err.starts_with("xtask: the OIDC token request failed: "),
        "{err}"
    );
    assert!(!err.contains(BEARER), "{err}");
    assert!(!env_file.exists());
}

#[test]
fn check_apt_names_the_program_it_could_not_start() {
    // Catches: a missing `dpkg` reported as a pass, or with no program named.
    let dir = scratch("check-spawn");
    let dpkg = dir.join("no-dpkg");
    let out = channels(&[
        "check-apt",
        "--arch",
        "amd64",
        "--version",
        "1.2.3",
        "--root",
        dir.to_str().expect("path"),
        "--dpkg",
        dpkg.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    let err = text(&out.stderr);
    assert_eq!(
        err,
        format!(
            "xtask: {}: No such file or directory (os error 2)\n",
            dpkg.display()
        )
    );
}

#[test]
fn check_apt_with_a_version_out_of_its_form_is_a_usage_error() {
    // Catches: a version with a prefix reaching `apt-get install js-agent=...`.
    let out = channels(&["check-apt", "--arch", "amd64", "--version", "v1.2.3"]);
    assert_eq!(code(&out), Some(2), "{out:?}");
    assert_eq!(
        text(&out.stderr),
        "xtask: --version \"v1.2.3\" is not <major>.<minor>.<patch>\n"
    );
}

#[test]
fn a_failed_sign_removes_the_key_and_the_passphrase_it_was_given() {
    // Catches: a private key left on the runner's disk when the signing fails.
    let dir = scratch("sign-fails");
    let (key, passphrase) = (dir.join("key.asc"), dir.join("passphrase"));
    std::fs::write(&key, "not a key").expect("key");
    std::fs::write(&passphrase, "phrase").expect("passphrase");
    let out = channels(&[
        "apt",
        "sign",
        "--dir",
        dir.join("dists/testing").to_str().expect("path"),
        "--work",
        dir.join("work").to_str().expect("path"),
        "--fingerprint",
        "0123456789ABCDEF0123456789ABCDEF01234567",
        "--key-file",
        key.to_str().expect("path"),
        "--passphrase-file",
        passphrase.to_str().expect("path"),
        "--gpg",
        dir.join("no-gpg").to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert!(text(&out.stderr).starts_with("xtask: "));
    assert!(!key.exists() && !passphrase.exists());
}

#[test]
fn an_apt_read_with_no_aws_program_fails_naming_it_and_writes_nothing() {
    // Catches: a read that reports success with nothing on disk.
    let dir = scratch("read-no-aws");
    let aws = dir.join("no-aws");
    let out_dir = dir.join("read");
    let out = channels(&[
        "apt",
        "read",
        "--suite",
        "testing",
        "--bucket",
        "apt-bucket",
        "--out",
        out_dir.to_str().expect("path"),
        "--aws",
        aws.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains(&aws.display().to_string()));
    assert!(!Path::new(&out_dir).join("etags.txt").exists());
}
