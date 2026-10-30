//! The helper modes of the daemon's binary: what each makes of its arguments, which
//! mode a first argument selects, and the real binary answering as a helper before it
//! looks at anything else.

#[allow(dead_code)]
mod support;

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use js_contain::modes::{dispatch, dispatch_args};
use js_contain::oci::helper::HelperModes;
use js_contain::oci::image::ROOTFS;
use js_contain::unpack_layers::{MOUNT_DIRS, MOUNT_FILES, run};
use support::{Item, gzip, my_ids, scratch, tar_of};

const HELPER: &str = env!("CARGO_BIN_EXE_js-helper");

fn args(stage: &Path, blobs: &Path, layers: &[&str]) -> Vec<OsString> {
    let mut args = vec![stage.into(), blobs.into()];
    args.extend(layers.iter().map(OsString::from));
    args
}

/// A tar of one file `path`, owned by `ids`.
fn tar_with(path: &str, data: &[u8], ids: (u64, u64)) -> Vec<u8> {
    tar_of(
        &[Item::File {
            path,
            data,
            mode: 0o644,
        }],
        ids,
    )
}

fn mine() -> (u64, u64) {
    (u64::from(my_ids().0), u64::from(my_ids().1))
}

/// A stage and a blob directory under a scratch directory.
fn dirs(name: &str) -> (support::Scratch, std::path::PathBuf, std::path::PathBuf) {
    let dir = scratch(name);
    let (stage, blobs) = (dir.join("stage"), dir.join("blobs"));
    std::fs::create_dir_all(&stage).expect("stage");
    std::fs::create_dir_all(&blobs).expect("blobs");
    (dir, stage, blobs)
}

/// Catches a mode that does not make the root filesystem the runtime needs: the layers'
/// files (a gzip and a plain layer, in order), and every mount point, as directories and
/// files, with an image's own `/etc` kept.
#[test]
fn the_mode_makes_the_root_and_its_mount_points() {
    let (_dir, stage, blobs) = dirs("mode-ok");
    std::fs::write(
        blobs.join("0.tar.gz"),
        gzip(&tar_with("etc/os-release", b"image", mine())),
    )
    .expect("layer");
    std::fs::write(
        blobs.join("1.tar"),
        tar_with("usr/bin/tool", b"tool", mine()),
    )
    .expect("layer");
    let modes = HelperModes::default();
    assert_eq!(
        run(&modes, &args(&stage, &blobs, &["0.tar.gz", "1.tar"])),
        0
    );
    let root = stage.join(ROOTFS);
    assert_eq!(
        std::fs::read(root.join("etc/os-release")).expect("file"),
        b"image"
    );
    assert_eq!(
        std::fs::read(root.join("usr/bin/tool")).expect("file"),
        b"tool"
    );
    for dir in MOUNT_DIRS {
        assert!(root.join(dir).is_dir(), "{dir}");
    }
    for file in MOUNT_FILES {
        assert!(root.join(file).is_file(), "{file}");
    }
}

/// Catches arguments the mode cannot read being run (a missing blob directory, a layer
/// name that is a path or has no tar suffix, or is not text) and the two ways the work
/// can fail, a layer that is not there and an image whose `etc` is a file so a mount
/// point cannot be made, reported as success.
#[test]
fn the_mode_refuses_bad_arguments_and_says_why_work_failed() {
    use std::os::unix::ffi::OsStringExt as _;
    let (_dir, stage, blobs) = dirs("mode-bad");
    std::fs::write(
        blobs.join("0.tar"),
        tar_with("etc", b"a file, not a directory", mine()),
    )
    .expect("layer");
    let modes = HelperModes::default();
    for bad in [
        vec![stage.clone().into_os_string()],
        args(&stage, &blobs, &["../escape.tar"]),
        args(&stage, &blobs, &["sub/0.tar"]),
        args(&stage, &blobs, &["0.zip"]),
        args(&stage, &blobs, &["0"]),
        vec![
            stage.clone().into(),
            blobs.clone().into(),
            OsString::from_vec(vec![0xff]),
        ],
    ] {
        assert_eq!(run(&modes, &bad), 2, "{bad:?}");
    }
    assert!(
        !stage.join(ROOTFS).exists(),
        "a refused request made a root"
    );
    assert_eq!(run(&modes, &args(&stage, &blobs, &["absent.tar"])), 1);
    support::force_remove(&stage.join(ROOTFS));
    assert_eq!(run(&modes, &args(&stage, &blobs, &["0.tar"])), 1);
}

/// Catches a damaged or hostile layer being taken for an image: a gzip layer cut short,
/// a layer that names an owner beyond the 65,536 ids a job's namespace maps, and a layer
/// whose entry climbs out of the root each end the mode with 1 (the reason on stderr),
/// and none leaves a file outside the stage.
#[test]
fn a_damaged_or_hostile_layer_fails_the_mode() {
    let good = gzip(&tar_with("f", &[7; 4000], mine()));
    for (name, layer) in [
        ("cut", good[..good.len() / 2].to_vec()),
        (
            "owner",
            gzip(&tar_with("f", b"x", (70_000, u64::from(my_ids().1)))),
        ),
        ("climb", gzip(&tar_with("../escape", b"x", mine()))),
    ] {
        let (dir, stage, blobs) = dirs(&format!("mode-{name}"));
        std::fs::write(blobs.join("0.tar.gz"), &layer).expect("layer");
        let code = run(
            &HelperModes::default(),
            &args(&stage, &blobs, &["0.tar.gz"]),
        );
        assert_eq!(code, 1, "{name}");
        assert!(!dir.join("escape").exists(), "{name}");
        assert!(!stage.join("escape").exists(), "{name}");
    }
}

/// Catches a mode that does not pass the unpack's default bound on (the bound is 32 GiB
/// of file bytes): a header that claims 40 GiB ends the mode with 1, and nothing of the
/// file is in the root.
#[test]
fn the_mode_applies_the_default_bound() {
    let (_dir, stage, blobs) = dirs("mode-bound");
    std::fs::write(blobs.join("0.tar"), claiming_40_gib()).expect("layer");
    let code = run(&HelperModes::default(), &args(&stage, &blobs, &["0.tar"]));
    assert_eq!(code, 1);
    assert!(!stage.join(ROOTFS).join("big").exists());
}

/// A tar of one header that claims a 40 GiB file and holds nothing after it.
fn claiming_40_gib() -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_path("big").expect("path");
    header.set_size(40 << 30);
    header.set_mode(0o644);
    header.set_uid(mine().0);
    header.set_gid(mine().1);
    header.set_cksum();
    header.as_bytes().to_vec()
}

/// Catches a first argument sent to the wrong mode, or to one when it names none: the
/// give and unpack words select their modes, the words come from the `HelperModes` given
/// (other words select, the default words then select nothing), and the program's name
/// alone, an empty list, a flag and a different word select no mode.
#[test]
fn the_first_argument_selects_the_mode_the_modes_name() {
    let (_dir, stage, blobs) = dirs("dispatch");
    std::fs::write(blobs.join("0.tar"), tar_with("f", b"x", mine())).expect("layer");
    let ids = format!("{}:{}", my_ids().0, my_ids().1);
    let words = |list: &[&str]| -> Vec<OsString> { list.iter().map(OsString::from).collect() };
    let with = |mode: &str, rest: Vec<OsString>| {
        let mut all = words(&["program", mode]);
        all.extend(rest);
        all
    };
    let defaults = HelperModes::default();
    let renamed = HelperModes {
        give: "give".to_owned(),
        whole: "all".to_owned(),
        unpack: "unpack".to_owned(),
        binary: "/other".to_owned(),
    };

    let give = |modes: &HelperModes, word: &str| {
        let line = with(
            word,
            vec![ids.clone().into(), stage.clone().into_os_string()],
        );
        dispatch_args(modes, &line)
    };
    assert_eq!(give(&defaults, &defaults.give), Some(0));
    assert_eq!(give(&renamed, "give"), Some(0));
    assert_eq!(give(&renamed, &defaults.give), None);
    assert_eq!(give(&defaults, "give"), None);

    let unpack = |modes: &HelperModes, word: &str| {
        dispatch_args(modes, &with(word, args(&stage, &blobs, &["0.tar"])))
    };
    assert_eq!(unpack(&renamed, &defaults.unpack), None);
    assert_eq!(unpack(&renamed, "unpack"), Some(0));
    assert!(stage.join(ROOTFS).join("f").is_file());
    // The root exists now: a second unpack into the same stage is refused, not merged.
    assert_eq!(unpack(&defaults, &defaults.unpack), Some(1));

    for none in [
        words(&[]),
        words(&["program"]),
        words(&["program", "--server"]),
        words(&["program", "unpack-layers"]),
    ] {
        assert_eq!(dispatch_args(&defaults, &none), None, "{none:?}");
    }
    // The test harness's own first argument is no mode.
    assert_eq!(dispatch(&defaults), None);
}

/// Runs the helper binary with `args`.
fn helper(args: &[&str]) -> std::process::Output {
    Command::new(HELPER).args(args).output().expect("spawn")
}

/// Catches the real binary not answering as the unpack helper: with the default mode
/// word it makes `rootfs` with the layer's file and the mount points and exits 0;
/// arguments it cannot read exit 2 with a usage line; a second unpack into the same stage
/// exits 1; a claimed size beyond the default bound exits 1 and the line names the bound.
#[test]
fn the_binary_answers_as_the_unpack_helper() {
    let (_dir, stage, blobs) = dirs("binary-unpack");
    std::fs::write(
        blobs.join("0.tar"),
        tar_with("etc/os-release", b"image", mine()),
    )
    .expect("layer");
    std::fs::write(blobs.join("1.tar"), claiming_40_gib()).expect("layer");
    let (stage_arg, blobs_arg) = (stage.display().to_string(), blobs.display().to_string());

    let bad = helper(&["__unpack-layers", &stage_arg]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("usage"));

    let ok = helper(&["__unpack-layers", &stage_arg, &blobs_arg, "0.tar"]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let root = stage.join(ROOTFS);
    assert_eq!(
        std::fs::read(root.join("etc/os-release")).expect("unpacked"),
        b"image"
    );
    for mount_point in ["dev", "proc", "sys/fs/cgroup"] {
        assert!(root.join(mount_point).is_dir(), "{mount_point}");
    }
    assert!(root.join("etc/resolv.conf").is_file());

    let again = helper(&["__unpack-layers", &stage_arg, &blobs_arg, "0.tar"]);
    assert_eq!(again.status.code(), Some(1));

    support::force_remove(&root);
    let big = helper(&["__unpack-layers", &stage_arg, &blobs_arg, "1.tar"]);
    assert_eq!(big.status.code(), Some(1));
    let said = String::from_utf8_lossy(&big.stderr);
    assert!(said.contains("34359738368"), "{said}");
}

/// Catches the real binary not answering as the give helper: the daemon's own ids are
/// given and the walk succeeds, with and without a whole tree; arguments it cannot read
/// exit 2 with a usage line; a tree it cannot open exits 1.
#[test]
fn the_binary_answers_as_the_give_helper() {
    let dir = scratch("binary-give");
    std::fs::create_dir(dir.join("sub")).expect("dir");
    let ids = format!("{}:{}", my_ids().0, my_ids().1);
    let tree = dir.display().to_string();
    let ok = helper(&["__give-dirs", &ids, &tree]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(
        helper(&["__give-dirs", &ids, "-", "--whole", &tree])
            .status
            .code(),
        Some(0)
    );
    let bad = helper(&["__give-dirs", "js"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("usage"));
    let absent = dir.join("absent").display().to_string();
    assert_eq!(
        helper(&["__give-dirs", &ids, &absent]).status.code(),
        Some(1)
    );
}

/// Catches the binary running something when its first argument is no helper mode: it
/// says so and exits 2, for a flag and for no argument at all.
#[test]
fn the_binary_is_nothing_but_a_helper() {
    for args in [&["--server"][..], &[]] {
        let out = helper(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("not a helper mode"));
    }
}
