//! `cargo xtask release image`, run as the built binary: the image tar it writes, the line it
//! prints and its refusals, with their exit statuses (1 for a refusal, 2 for flags clap
//! rejects). The bytes are the library's `build` of the same inputs.

use std::path::Path;
use std::process::{Command, Output};

use js_testkit::dir::TestDir;
use xtask::release::server_image::{Target, build};

const VERSION: &str = "4.5.6";

/// Runs `release image` for `binary` into `out`, with `target` and `version` as given.
fn image(binary: &Path, version: &str, target: &str, out: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["release", "image", "--binary"])
        .arg(binary)
        .args(["--version", version, "--target", target, "--out"])
        .arg(out)
        .output()
        .expect("run xtask")
}

/// A binary that says it is `VERSION`.
fn binary(dir: &TestDir) -> Vec<u8> {
    let bytes = format!("\u{7f}ELF\n{VERSION}\n").into_bytes();
    std::fs::write(dir.join("js-lease-server"), &bytes).expect("binary");
    bytes
}

/// Asserts, for each target: exit 0, the one line
/// `<name>: sha256 <hex>, <n> bytes, image sha256:<hex>` naming the image of the target, the file
/// in `--out` (made when missing) equal to the library's image of the same binary with the
/// digest of its manifest in the line, and the same bytes from a second directory (the build
/// depends on nothing of the run).
#[test]
fn a_run_writes_the_image_of_the_target_and_prints_its_digests() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-server-image");
    let bytes = binary(&dir);
    for (target, built) in [
        ("x86_64-unknown-linux-musl", Target::X86_64),
        ("aarch64-unknown-linux-musl", Target::Aarch64),
    ] {
        let name = format!("js-lease-server-{VERSION}-{target}.oci.tar");
        let out = image(
            &dir.join("js-lease-server"),
            VERSION,
            target,
            &dir.join(target),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let written = std::fs::read(dir.join(target).join(&name)).expect("the image");
        let expected = build(VERSION.parse().expect("version"), built, &bytes).expect("build");
        assert_eq!(written, expected.tar);
        let line = String::from_utf8(out.stdout).expect("utf-8");
        assert!(
            line.starts_with(&format!("{name}: sha256 "))
                && line.ends_with(&format!(
                    ", {} bytes, image sha256:{}\n",
                    written.len(),
                    expected.manifest_digest
                )),
            "{line}"
        );
        assert_eq!(line.lines().count(), 1);
        let second = dir.join(format!("{target}-2"));
        let again = image(&dir.join("js-lease-server"), VERSION, target, &second);
        assert!(again.status.success());
        assert_eq!(std::fs::read(second.join(&name)).expect("image"), written);
        assert_eq!(again.stdout, line.as_bytes());
    }
    // Nothing but the image is written: no sums file.
    assert_eq!(
        std::fs::read_dir(dir.join("x86_64-unknown-linux-musl"))
            .expect("dir")
            .count(),
        1
    );
}

/// Asserts the refusals, each with its one reason on stderr: an existing image is not replaced
/// and a binary of another version is refused by name (status 1), a version that is not three
/// numbers is refused (status 2), and none leaves an image behind.
#[test]
fn a_run_refuses_an_existing_image_and_a_binary_of_another_version() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-server-image-refuse");
    binary(&dir);
    let out_dir = dir.join("out");
    let target = "x86_64-unknown-linux-musl";
    assert!(
        image(&dir.join("js-lease-server"), VERSION, target, &out_dir)
            .status
            .success()
    );
    let again = image(&dir.join("js-lease-server"), VERSION, target, &out_dir);
    assert_eq!(again.status.code(), Some(1));
    let stderr = String::from_utf8(again.stderr).expect("utf-8");
    assert!(
        stderr.starts_with("xtask: ")
            && stderr.contains("js-lease-server-4.5.6-x86_64-unknown-linux-musl.oci.tar")
            && stderr.contains("exists"),
        "{stderr}"
    );

    let other = image(
        &dir.join("js-lease-server"),
        "4.5.7",
        target,
        &dir.join("other"),
    );
    assert_eq!(other.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(other.stderr).expect("utf-8"),
        format!(
            "xtask: {} does not hold the version 4.5.7: it is not a build of that version\n",
            dir.join("js-lease-server").display()
        )
    );
    assert!(!dir.join("other").exists());

    let bad = image(
        &dir.join("js-lease-server"),
        "4.5",
        target,
        &dir.join("bad"),
    );
    assert_eq!(bad.status.code(), Some(2));
    assert!(!dir.join("bad").exists());
}

/// Asserts that a target that has no image (macOS, or a glibc Linux) is a usage error, status 2,
/// that names the two targets that have one, and writes nothing.
#[test]
fn a_target_without_an_image_is_a_usage_error_naming_the_accepted_ones() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-server-image-target");
    binary(&dir);
    for target in ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"] {
        let out = image(
            &dir.join("js-lease-server"),
            VERSION,
            target,
            &dir.join("out"),
        );
        assert_eq!(out.status.code(), Some(2), "{target}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(target), "{stderr}");
        assert!(
            stderr.contains("x86_64-unknown-linux-musl")
                && stderr.contains("aarch64-unknown-linux-musl"),
            "{stderr}"
        );
        assert!(!dir.join("out").exists());
    }
}
