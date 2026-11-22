//! The process identity on Linux: the process table as `/proc` shows it.

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// The state letter and the start time of the process `pid`, from `/proc`.
pub(crate) fn stat_of(pid: u32) -> Option<(char, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    super::parse_stat(&text)
}

/// Whether the process `pid` runs as the user `uid` (the owner of its `/proc` entry).
pub(crate) fn runs_as(pid: u32, uid: u32) -> bool {
    std::fs::metadata(format!("/proc/{pid}")).is_ok_and(|meta| meta.uid() == uid)
}

/// Whether the command line of the process `pid` names `lease` as the lease.
pub(crate) fn supervises(pid: u32, lease: &Path) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|cmdline| super::names_lease(cmdline.split(|byte| *byte == 0), lease))
}
