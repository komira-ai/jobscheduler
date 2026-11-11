//! `cargo xtask release server-deploy`, run as the built binary with a stand-in `gh` (a small
//! shell script in the scratch directory that answers the calls the verb makes, and for the
//! image download copies a tar the test built): the exit statuses (0 for a deploy or a
//! deliberate no-deploy, 1 for a refusal, 2 for flags that do not fit), what reaches stdout and
//! stderr, and the lines appended to the output file. The real `gh` and `curl` are never started
//! and nothing touches the network.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::sync::Mutex;

use js_testkit::dir::TestDir;

/// A script being written is a file open for writing: one test at a time writes and starts its
/// stand-in, or another test's child could hold the descriptor and fail the start ("text file
/// busy").
static SERIAL: Mutex<()> = Mutex::new(());

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const TAR: &str = "js-lease-server-1.2.3-x86_64-unknown-linux-musl.oci.tar";
const BUNDLE: &str = "js-lease-server-1.2.3-x86_64-unknown-linux-musl.oci.tar.sigstore.json";
const BASE: &str = "https://github.com/o/r/releases/download/v1.2.3";

/// Runs `release server-deploy` with `flags`, in `dir`, with the stand-in `gh` that runs
/// `gh_body`.
fn deploy(dir: &TestDir, gh_body: &str, flags: &[&str]) -> Output {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let gh = dir.join("gh");
    std::fs::write(&gh, format!("#!/bin/sh\n{gh_body}")).expect("script");
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).expect("mode");
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["release", "server-deploy", "--repo", "o/r", "--dir"])
        .arg(dir.join("d"))
        .arg("--output")
        .arg(dir.join("out"))
        .arg("--gh")
        .arg(&gh)
        .args(flags)
        .output()
        .expect("run xtask")
}

/// The release JSON of a published release holding `assets`.
fn release(assets: &[&str]) -> String {
    let assets: Vec<String> = assets
        .iter()
        .map(|a| format!("{{\"name\":\"{a}\"}}"))
        .collect();
    format!(
        "{{\"draft\":false,\"prerelease\":false,\"assets\":[{}]}}",
        assets.join(",")
    )
}

/// A tar holding `index.json` of one linux/amd64 manifest with digest [`DIGEST`].
fn image_tar() -> Vec<u8> {
    let index = format!(
        "{{\"schemaVersion\":2,\"manifests\":[{{\"digest\":\"{DIGEST}\",\"size\":7,\"platform\":{{\"os\":\"linux\",\"architecture\":\"amd64\"}}}}]}}"
    );
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(index.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "index.json", index.as_bytes())
        .expect("append");
    builder.into_inner().expect("tar")
}

/// Asserts that both flags empty, and both given, exit 2 with the usage reason on stderr and
/// nothing on stdout or in the output file.
#[test]
fn both_and_neither_of_version_and_after_run_exit_two() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-server-deploy-usage");
    for flags in [
        ["--version", "", "--after-run", ""],
        ["--version", "1.2.3", "--after-run", "9"],
    ] {
        let out = deploy(&dir, "exit 9\n", &flags);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(out.stdout, b"");
        assert_eq!(
            String::from_utf8(out.stderr).expect("utf-8"),
            "xtask: give one of --version and --after-run, not both and not neither\n"
        );
        assert!(!dir.join("out").exists());
    }
}

/// Asserts that a run whose `publish` job did not succeed exits 0, prints why nothing is
/// deployed and `deploy=false`, and appends `deploy=false` to the output file.
#[test]
fn a_run_that_published_nothing_exits_zero_with_deploy_false() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-server-deploy-none");
    let out = deploy(
        &dir,
        "echo '{\"total_count\":1,\"jobs\":[{\"name\":\"publish\",\"conclusion\":\"skipped\"}]}'\n",
        &["--version", "", "--after-run", "42"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8"),
        "run 42 of o/r did not publish a release (publish did not succeed): nothing is deployed\ndeploy=false\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        "deploy=false\n"
    );
}

/// Asserts that a release without the attestation bundle exits 1 with the refusal naming the
/// missing asset, and that nothing is appended to the output file.
#[test]
fn a_release_missing_its_bundle_exits_one_naming_it() {
    let dir = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        "release-server-deploy-nobundle",
    );
    let out = deploy(
        &dir,
        &format!("echo '{}'\n", release(&[TAR, "SHA256SUMS"])),
        &["--version", "1.2.3", "--after-run", ""],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        String::from_utf8(out.stderr).expect("utf-8"),
        format!("xtask: release v1.2.3 has no {BUNDLE}\n")
    );
    assert!(!dir.join("out").exists());
}

/// Asserts that a release holding the image and its bundle exits 0 and appends exactly the five
/// output lines: the digest of the one linux/amd64 manifest and the two release URLs.
#[test]
fn a_good_release_exits_zero_and_writes_the_five_outputs() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-server-deploy-ok");
    let fixture = dir.join("fixture.tar");
    std::fs::write(&fixture, image_tar()).expect("fixture");
    let body = format!(
        "case \"$1\" in\n  api) echo '{}' ;;\n  release) cp '{}' \"$7/{TAR}\" ;;\n  *) exit 9 ;;\nesac\n",
        release(&[TAR, BUNDLE, "SHA256SUMS"]),
        fixture.display()
    );
    let out = deploy(&dir, &body, &["--version", "1.2.3", "--after-run", ""]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines = format!(
        "deploy=true\nversion=1.2.3\nimage-digest={DIGEST}\nimage-url={BASE}/{TAR}\nbundle-url={BASE}/{BUNDLE}\n"
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8"),
        format!(
            "release v1.2.3: {TAR} holds one linux/amd64 image, {DIGEST}, and {BUNDLE} is attached\n{lines}"
        )
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        lines
    );
}
