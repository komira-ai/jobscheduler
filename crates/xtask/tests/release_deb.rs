//! `cargo xtask release deb`, run as the built binary: the package it writes, the line it
//! prints and its refusals, with their exit statuses (1 for a refusal, 2 for flags clap
//! rejects). The bytes are the library's `build` of the same inputs.

use std::path::Path;
use std::process::{Command, Output};

use js_testkit::dir::TestDir;
use xtask::release::deb::{Platform, build, files, version_marker};

const VERSION: &str = "4.5.6";

/// Runs `release deb` for `binary` into `out`, with `platform` and `version` as given.
fn deb(binary: &Path, version: &str, platform: &str, out: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["release", "deb", "--binary"])
        .arg(binary)
        .args(["--version", version, "--platform", platform, "--out"])
        .arg(out)
        .output()
        .expect("run xtask")
}

/// A binary that says it is `VERSION`.
fn binary(dir: &TestDir) -> Vec<u8> {
    let bytes = format!(
        "\u{7f}ELF {}\n",
        version_marker(VERSION.parse().expect("version"))
    )
    .into_bytes();
    std::fs::write(dir.join("js-agent"), &bytes).expect("binary");
    bytes
}

/// Asserts, for each platform: exit 0, the one line `<name>: sha256 <hex>, <n> bytes` naming
/// the package of the platform's Debian architecture, the file in `--out` (made when missing)
/// equal to the library's package of the same binary, and the same bytes from a second
/// directory (the build depends on nothing of the run).
#[test]
fn a_run_writes_the_package_of_the_platform_and_prints_its_digest() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-deb");
    let bytes = binary(&dir);
    for (platform, arch, built) in [
        ("linux-x86_64", "amd64", Platform::LinuxX86_64),
        ("linux-arm64", "arm64", Platform::LinuxArm64),
    ] {
        let name = format!("js-agent_{VERSION}_{arch}.deb");
        let out = deb(&dir.join("js-agent"), VERSION, platform, &dir.join(arch));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let written = std::fs::read(dir.join(arch).join(&name)).expect("the package");
        assert_eq!(
            written,
            build(VERSION.parse().expect("version"), built, &files(&bytes)).expect("build")
        );
        let line = String::from_utf8(out.stdout).expect("utf-8");
        assert!(
            line.starts_with(&format!("{name}: sha256 "))
                && line.ends_with(&format!(", {} bytes\n", written.len())),
            "{line}"
        );
        assert_eq!(line.lines().count(), 1);
        let again = deb(
            &dir.join("js-agent"),
            VERSION,
            platform,
            &dir.join(format!("{arch}2")),
        );
        assert!(again.status.success());
        assert_eq!(
            std::fs::read(dir.join(format!("{arch}2")).join(&name)).expect("package"),
            written
        );
        assert_eq!(again.stdout, line.as_bytes());
    }
    // Nothing but the package is written: no sums file.
    assert_eq!(
        std::fs::read_dir(dir.join("amd64")).expect("dir").count(),
        1
    );
}

/// Asserts the refusals, each with status 1 and its one reason on stderr: an existing package
/// is not replaced, a binary of another version is refused by name, a version that is not three
/// numbers is refused, and none leaves a package behind.
#[test]
fn a_run_refuses_an_existing_package_and_a_binary_of_another_version() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-deb-refuse");
    binary(&dir);
    let out_dir = dir.join("out");
    assert!(
        deb(&dir.join("js-agent"), VERSION, "linux-x86_64", &out_dir)
            .status
            .success()
    );
    let again = deb(&dir.join("js-agent"), VERSION, "linux-x86_64", &out_dir);
    assert_eq!(again.status.code(), Some(1));
    let stderr = String::from_utf8(again.stderr).expect("utf-8");
    assert!(
        stderr.starts_with("xtask: ")
            && stderr.contains("js-agent_4.5.6_amd64.deb")
            && stderr.contains("exists"),
        "{stderr}"
    );

    let other = deb(
        &dir.join("js-agent"),
        "4.5.7",
        "linux-x86_64",
        &dir.join("other"),
    );
    assert_eq!(other.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(other.stderr).expect("utf-8"),
        format!(
            "xtask: {} does not hold the version 4.5.7 and its runtimes line: it is not a build of that version\n",
            dir.join("js-agent").display()
        )
    );
    assert!(!dir.join("other").exists());

    let bad = deb(
        &dir.join("js-agent"),
        "4.5",
        "linux-x86_64",
        &dir.join("bad"),
    );
    assert_eq!(bad.status.code(), Some(2));
    assert!(!dir.join("bad").exists());
}

/// Asserts that a platform that has no package (macOS) is a usage error, status 2, and writes
/// nothing.
#[test]
fn a_platform_without_a_package_is_a_usage_error() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-deb-mac");
    binary(&dir);
    let out = deb(
        &dir.join("js-agent"),
        VERSION,
        "macos-arm64",
        &dir.join("out"),
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("macos-arm64"));
    assert!(!dir.join("out").exists());
}
