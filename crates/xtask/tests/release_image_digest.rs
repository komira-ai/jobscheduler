//! `cargo xtask release image-digest`, run as the built binary: the three lines it prints for an
//! image `release image` wrote, and its exit statuses (1 for a refusal, 2 for flags clap rejects).

use std::path::Path;
use std::process::{Command, Output};

use js_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

const VERSION: &str = "4.5.6";
const IMAGE: &str = "js-lease-server-4.5.6-x86_64-unknown-linux-musl.oci.tar";

fn xtask(args: &[&str], dist: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["release", "image-digest", "--dist"])
        .arg(dist)
        .args(args)
        .output()
        .expect("run xtask")
}

/// Asserts that, for the image `release image` wrote and a `SHA256SUMS` and bundle beside it, the
/// verb exits 0 and prints the digest `release image` printed and the release's two addresses;
/// that a `SHA256SUMS` that does not list the image exits 1 with the reason on stderr and nothing
/// on stdout; and that a version clap rejects exits 2.
#[test]
fn the_digest_of_the_image_release_image_wrote() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "release-image-digest");
    std::fs::write(
        dir.join("js-lease-server"),
        format!("\u{7f}ELF\n{VERSION}\n"),
    )
    .expect("bin");
    let made = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["release", "image", "--binary"])
        .arg(dir.join("js-lease-server"))
        .args([
            "--version",
            VERSION,
            "--target",
            "x86_64-unknown-linux-musl",
            "--out",
        ])
        .arg(&*dir)
        .output()
        .expect("run xtask");
    assert!(made.status.success());
    let printed = String::from_utf8(made.stdout).expect("utf-8");
    let digest = printed
        .trim_end()
        .rsplit_once(", image ")
        .expect("the image digest")
        .1;
    let image = std::fs::read(dir.join(IMAGE)).expect("image");
    let sums = format!("{}  {IMAGE}\n", hex::encode(Sha256::digest(&image)));
    std::fs::write(dir.join("SHA256SUMS"), sums).expect("sums");
    std::fs::write(dir.join(format!("{IMAGE}.sigstore.json")), b"{}").expect("bundle");

    let flags = ["--repo", "komira-ai/jobscheduler", "--version", VERSION];
    let out = xtask(&flags, &dir);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));
    let base = "https://github.com/komira-ai/jobscheduler/releases/download/v4.5.6";
    assert_eq!(
        String::from_utf8(out.stdout).expect("utf-8"),
        format!(
            "digest={digest}\nimage-url={base}/{IMAGE}\nbundle-url={base}/{IMAGE}.sigstore.json\n"
        )
    );

    std::fs::write(dir.join("SHA256SUMS"), "").expect("sums");
    let refused = xtask(&flags, &dir);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(refused.stdout, b"");
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        format!("xtask: SHA256SUMS lists {IMAGE} 0 times, not once\n")
    );

    let usage = xtask(
        &["--repo", "komira-ai/jobscheduler", "--version", "v4.5.6"],
        &dir,
    );
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(usage.stdout, b"");
}
