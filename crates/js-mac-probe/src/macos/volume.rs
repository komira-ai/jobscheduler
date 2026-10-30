//! P-B7: whether the work and home volumes are case-sensitive, how they are mounted
//! (ownership honoured or ignored) and who owns a file made there. Reported only.

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::Command;

use super::Scratch;
use super::exec::run;
use super::sys::mount_info;
use crate::report::{Probe, Report};

/// `MNT_IGNORE_OWNERSHIP`: the volume is mounted with `noowners`.
const MNT_IGNORE_OWNERSHIP: u32 = 0x0020_0000;
const MNT_RDONLY: u32 = 0x1;
const MNT_NOSUID: u32 = 0x8;
const MNT_NODEV: u32 = 0x10;

fn observe(mut probe: Probe, name: &str, dir: &Path, mounts: &str) -> Probe {
    let id = std::process::id();
    let mixed = dir.join(format!("js-mac-probe-{id}-CaseProbe"));
    let lower = dir.join(format!("js-mac-probe-{id}-caseprobe"));
    let wrote = fs::write(&mixed, b"x");
    probe = probe.fact(
        &format!("{name}_case_sensitive"),
        wrote.is_ok() && !lower.exists(),
    );
    let owner = fs::metadata(&mixed).map_or("?".to_owned(), |m| m.uid().to_string());
    let _ = fs::remove_file(&mixed);
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    probe = probe
        .fact(&format!("{name}_new_file_uid"), owner)
        .fact(&format!("{name}_caller_uid"), uid);
    match mount_info(dir) {
        Ok((from, on, flags)) => {
            let line = mounts
                .lines()
                .find(|l| l.starts_with(&format!("{from} on {on} ")))
                .unwrap_or("");
            probe
                .fact(&format!("{name}_mounted_on"), &on)
                .fact(
                    &format!("{name}_ignores_ownership"),
                    flags & MNT_IGNORE_OWNERSHIP != 0,
                )
                .fact(&format!("{name}_read_only"), flags & MNT_RDONLY != 0)
                .fact(&format!("{name}_nosuid"), flags & MNT_NOSUID != 0)
                .fact(&format!("{name}_nodev"), flags & MNT_NODEV != 0)
                .fact(&format!("{name}_mount_line"), line)
        }
        Err(err) => probe.fact(&format!("{name}_mount_error"), err),
    }
}

pub fn probes(scratch: &Scratch, report: &mut Report) {
    let mounts = run(Command::new("/sbin/mount")).stdout;
    let mut probe = Probe::info("fs/case-and-ownership");
    probe = observe(probe, "work", &scratch.work, &mounts);
    probe = observe(probe, "home", &scratch.home, &mounts);
    report.push(probe);
}
