//! P-B9: the raw image route `js-vmm` takes, unprivileged: `set_len` a file, attach it raw
//! without mounting, `newfs_apfs` it, detach, attach again with ownership honoured, write
//! and read a file, check the mount flags and detach. Every step is reported with its exit
//! status and error; the privilege each needs on a runner is not known from here.

use std::fs;
use std::path::Path;
use std::process::Command;

use super::Scratch;
use super::exec::{Outcome, describe, run};
use super::sys::mount_info;
use crate::parse::{first_device, plist_strings};
use crate::report::{Probe, Report};

const SIZE: u64 = 256 << 20;
const RAW: [&str; 2] = ["-imagekey", "diskimage-class=CRawDiskImage"];

fn hdiutil(args: &[&str], image: Option<&Path>) -> Outcome {
    let mut cmd = Command::new("/usr/bin/hdiutil");
    cmd.args(args);
    if let Some(image) = image {
        cmd.arg(image);
    }
    run(cmd)
}

fn detach(device: &str) -> Outcome {
    hdiutil(&["detach", "-force", device], None)
}

pub fn probes(scratch: &Scratch, report: &mut Report) {
    let image = scratch.base.join("raw.img");
    let made = fs::File::create(&image).and_then(|f| f.set_len(SIZE));
    let mut probe = Probe::info("image/raw-apfs").fact("set_len_ok", made.is_ok());
    if made.is_ok() {
        probe = attach_format_mount(probe, &image);
    }
    report.push(probe);
}

fn attach_format_mount(mut probe: Probe, image: &Path) -> Probe {
    let mut args = vec!["attach"];
    args.extend(RAW);
    args.push("-nomount");
    let first = hdiutil(&args, Some(image));
    probe = describe(
        probe.fact("attach_nomount_ok", first.ok),
        "attach_nomount_",
        &first,
    );
    let Some(device) = first_device(&first.stdout) else {
        return probe.fact("device", "none");
    };
    let newfs = run({
        let mut cmd = Command::new("/sbin/newfs_apfs");
        cmd.args(["-v", "probe", &device]);
        cmd
    });
    probe = describe(probe.fact("newfs_apfs_ok", newfs.ok), "newfs_apfs_", &newfs);
    let first_detach = detach(&device);
    probe = describe(
        probe.fact("detach_nomount_ok", first_detach.ok),
        "detach_nomount_",
        &first_detach,
    );
    if !newfs.ok {
        return probe;
    }

    let mut args = vec!["attach"];
    args.extend(RAW);
    args.extend(["-owners", "on", "-nobrowse", "-plist"]);
    let second = hdiutil(&args, Some(image));
    probe = describe(
        probe.fact("attach_mount_ok", second.ok),
        "attach_mount_",
        &second,
    );
    let devices = plist_strings(&second.stdout, "dev-entry");
    let mounts = plist_strings(&second.stdout, "mount-point");
    probe = probe.fact("mount_points", mounts.join(" "));
    if let Some(mount) = mounts.first() {
        let file = Path::new(mount).join("probe");
        let wrote = fs::write(&file, b"raw image\n");
        let back = fs::read(&file).is_ok_and(|b| b == b"raw image\n");
        probe = probe
            .fact("write_ok", wrote.is_ok())
            .fact("read_back_ok", back);
        probe = match mount_info(Path::new(mount)) {
            Ok((from, _, flags)) => probe
                .fact("mounted_from", from)
                .fact("ignores_ownership", flags & 0x0020_0000 != 0),
            Err(err) => probe.fact("mount_info_error", err),
        };
    }
    if let Some(device) = devices.first() {
        let last = detach(device);
        probe = describe(probe.fact("detach_ok", last.ok), "detach_", &last);
    }
    probe
}
