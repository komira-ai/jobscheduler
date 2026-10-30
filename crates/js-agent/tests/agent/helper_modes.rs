//! The agent's binary as a helper container of the OCI runner runs it: the first
//! argument selects a helper mode, and the flags (of which `--server` is required) are
//! never looked at. (Linux only: only Linux runs containers.)

use std::path::Path;
use std::process::{Command, Output};

use js_testkit::dir::TestDir;

/// The agent binary run with `args`.
fn agent(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_js-agent"))
        .args(args)
        .output()
        .expect("spawn")
}

fn ids() -> String {
    format!(
        "{}:{}",
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw()
    )
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-agent"),
        name,
    )
}

/// Catches a binary that parses its flags before it answers as the unpack helper (clap
/// would stop at the missing `--server`, and at an argument that is no flag): given a
/// stage directory, a directory of blobs and one layer, it makes `rootfs` with the
/// layer's file and the mount points the runtime binds over and exits 0; arguments it
/// cannot read exit 2 with the mode's own usage line; a second unpack into the same stage
/// exits 1.
#[test]
fn the_binary_runs_as_the_unpack_helper_before_it_parses_its_flags() {
    let dir = scratch("helper-unpack");
    let (stage, blobs) = (dir.join("stage"), dir.join("blobs"));
    std::fs::create_dir_all(&stage).expect("stage");
    std::fs::create_dir_all(&blobs).expect("blobs");
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_uid(u64::from(rustix::process::geteuid().as_raw()));
    header.set_gid(u64::from(rustix::process::getegid().as_raw()));
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_data(&mut header, "etc/os-release", &b"image"[..])
        .expect("append");
    std::fs::write(blobs.join("0.tar"), builder.into_inner().expect("tar")).expect("layer");
    let (stage_arg, blobs_arg) = (stage.display().to_string(), blobs.display().to_string());

    let bad = agent(&["__unpack-layers", &stage_arg]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("__unpack-layers: usage"));

    let ok = agent(&["__unpack-layers", &stage_arg, &blobs_arg, "0.tar"]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(
        std::fs::read(stage.join("rootfs/etc/os-release")).expect("unpacked"),
        b"image"
    );
    for mount_point in ["dev", "proc", "sys/fs/cgroup"] {
        assert!(stage.join("rootfs").join(mount_point).is_dir());
    }
    assert!(stage.join("rootfs/etc/resolv.conf").is_file());

    let again = agent(&["__unpack-layers", &stage_arg, &blobs_arg, "0.tar"]);
    assert_eq!(again.status.code(), Some(1));
}

/// Catches a binary that parses its flags before it answers as the give helper: the
/// daemon's own ids are given to a directory and to a whole tree, and the walk succeeds
/// (clap would refuse the mode); arguments it cannot read exit 2 with the mode's own
/// usage line.
#[test]
fn the_binary_runs_as_the_give_helper_before_it_parses_its_flags() {
    let dir = scratch("helper-give");
    std::fs::create_dir(dir.join("sub")).expect("dir");
    let tree = dir.display().to_string();
    let ok = agent(&["__give-dirs", &ids(), &tree, "--whole", &tree]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let bad = agent(&["__give-dirs", "js"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("__give-dirs: usage"));
}

/// Catches a binary that takes a mode word for a helper mode anywhere but first: with
/// the mode word after a flag the flags are parsed as always, and the missing `--server`
/// is the usage error it is without the word.
#[test]
fn a_mode_word_after_a_flag_is_no_helper_mode() {
    let out = agent(&["--label", "rack=r9", "__give-dirs", &ids(), "-"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("__give-dirs: usage"));
}
