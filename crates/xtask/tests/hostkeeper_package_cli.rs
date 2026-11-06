//! The hostkeeper package verbs as `cargo xtask` runs them, through the program's own command
//! line: the flags parse, the line is printed on stdout and the status is 0; a refusal prints its
//! reason on stderr and exits 1. This is its own test target, not part of `hostkeeper_package`:
//! that target takes file locks, and a program started beside a lock being released would hold
//! the lock's file a moment longer than the test means.

use std::io::Read;
use std::process::{Command, Output};

use js_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

const VERSION: &str = "1.2.3";

/// The three packages, with their targets.
const PACKAGES: [(&str, &str); 3] = [
    (
        "x86_64-unknown-linux-musl",
        "js-agent-1.2.3-linux-x86_64.tar.zst",
    ),
    (
        "aarch64-unknown-linux-musl",
        "js-agent-1.2.3-linux-arm64.tar.zst",
    ),
    ("aarch64-apple-darwin", "js-agent-1.2.3-macos-arm64.tar.zst"),
];

fn xtask(words: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("release")
        .args(words)
        .output()
        .expect("xtask runs")
}

/// The decompressed tar of the package `bytes`.
fn tar_of(bytes: &[u8]) -> Vec<u8> {
    let mut rest = bytes;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut rest).expect("a zstd frame");
    let mut tar = Vec::new();
    decoder.read_to_end(&mut tar).expect("the frame decodes");
    tar
}

/// Each verb prints one line on stdout and exits 0: the package line states the file's own
/// digest and size, a package made without `--min-hostkeeper` names the default floor, the index
/// line counts the three packages, and a second index in the same directory is refused with its
/// reason on stderr and status 1, nothing on stdout. Catches a verb that is not wired to its
/// flags, a printed digest or size that is not the file's, a default floor that is not written
/// and an error that goes to stdout or exits 0.
#[test]
fn the_verbs_run_from_the_command_line_and_a_refusal_exits_1_on_stderr() {
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hostkeeper-package-cli");
    let dir = scratch.path();
    let bin = dir.join("js-agent");
    std::fs::write(
        &bin,
        format!("stand-in\0{VERSION}\nruntimes: docker crun\0"),
    )
    .expect("binary");
    let (bin, dir_text) = (
        bin.to_str().expect("UTF-8").to_owned(),
        dir.to_str().expect("UTF-8").to_owned(),
    );
    for (target, name) in PACKAGES {
        let out = xtask(&[
            "hostkeeper-package",
            "--binary",
            &bin,
            "--target",
            target,
            "--version",
            VERSION,
            "--out",
            &dir_text,
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bytes = std::fs::read(dir.join(name)).expect("the package");
        let line = String::from_utf8(out.stdout).expect("UTF-8");
        assert_eq!(
            line,
            format!(
                "{name}: sha256 {}, {} bytes\n",
                hex::encode(Sha256::digest(&bytes)),
                bytes.len()
            )
        );
        let tar = tar_of(&bytes);
        let floor = b"min_hostkeeper = \"0.1.92\"";
        assert!(tar.windows(floor.len()).any(|w| w == floor), "{name}");
    }
    let index = ["hostkeeper-index", "--dir", &dir_text, "--version", VERSION];
    let out = xtask(&index);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).expect("UTF-8"),
        "js-agent-index.json: 3 packages\n"
    );
    let again = xtask(&index);
    assert_eq!(again.status.code(), Some(1));
    assert!(again.stdout.is_empty());
    assert!(String::from_utf8_lossy(&again.stderr).starts_with("xtask: "));
}
