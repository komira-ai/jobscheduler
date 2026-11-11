//! The `cargo xtask debs` binary: its flags, what it prints and its exit codes, with the pinned
//! files already in the directory (as the actions cache restores them), so that no test
//! downloads anything. The downloads, the mirror fallback and the rounds are the unit tests'
//! (src/debs/tests.rs).

use std::path::Path;
use std::process::Command;

use xtask::debs::Arch;
use xtask::fetch::Sha256;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// An empty directory of its own for one test, removed when the value is dropped.
fn scratch(name: &str) -> js_testkit::dir::TestDir {
    js_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("xtask-debs-{name}"))
}

/// A pins file of one package, `p 1.0`, for both architectures, from a mirror no test reaches.
fn pins(dir: &Path) -> std::path::PathBuf {
    let mut text = String::new();
    for arch in ["amd64", "arm64"] {
        text.push_str(&format!(
            "[{arch}]\nmirrors = [\"https://mirror.invalid/ubuntu\"]\n\
             debs = [{{ package = \"p\", version = \"1.0\", path = \"pool/main/p/p_1.0_{arch}.deb\", sha256 = \"{}\" }}]\n",
            Sha256::of(b"p")
        ));
    }
    let file = dir.join("pins.toml");
    std::fs::write(&file, text).expect("pins");
    file
}

/// This host's file name of `p`.
fn name() -> String {
    format!("p_1.0_{}.deb", Arch::host().expect("arch").name())
}

fn xtask(pins: &Path, dir: &Path) -> std::process::Output {
    Command::new(XTASK)
        .args(["debs", "--pins"])
        .arg(pins)
        .arg("--dir")
        .arg(dir)
        .output()
        .expect("run xtask")
}

/// Asserts that with the pinned file in the directory the verb exits 0 and prints one line
/// saying it was cached, and prints nothing on stderr.
#[test]
fn a_restored_directory_passes_and_says_so() {
    let dir = scratch("restored");
    let debs = dir.path().join("debs");
    std::fs::create_dir_all(&debs).expect("debs");
    std::fs::write(debs.join(name()), b"p").expect("p");
    let out = xtask(&pins(dir.path()), &debs);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "ok: the cached {} has SHA-256 {}\n",
            name(),
            Sha256::of(b"p")
        )
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));
}

/// Asserts that a directory holding a file no pin names exits 1 with the reason on stderr and
/// nothing on stdout, and that a missing flag is a usage error (exit 2).
#[test]
fn a_stray_file_fails_and_a_missing_flag_is_a_usage_error() {
    let dir = scratch("stray");
    let debs = dir.path().join("debs");
    std::fs::create_dir_all(&debs).expect("debs");
    std::fs::write(debs.join(name()), b"p").expect("p");
    std::fs::write(debs.join("extra.deb"), b"x").expect("extra");
    let out = xtask(&pins(dir.path()), &debs);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("xtask: {debs:?} holds \"extra.deb\", which no pin names; nothing was installed\n")
    );
    assert_eq!(out.status.code(), Some(1));

    let usage = Command::new(XTASK)
        .args(["debs", "--pins", "pins.toml"])
        .output()
        .expect("run xtask");
    assert_eq!(usage.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&usage.stderr).contains("--dir <DIR>"),
        "{usage:?}"
    );
}
