//! Where a running container is when its group below the lease is not there
//! ([`Runtime::whereabouts`](crate::lifecycle::Runtime::whereabouts)).
//!
//! The container's first process runs from soon after its supervisor said it was ready until
//! the container ends, and `crun` makes the container's group before it records the container
//! (so before `crun state` names the process) and removes the group only after that process
//! ended. A group that is not there is therefore one of three things: the container is not
//! made yet (`crun state` names no process), it ended and its group went with it (the process
//! `crun state` named is no longer in the agent's `/proc`), or its first process runs in
//! another group. Only the last leaves the agent blind to the container.

use std::path::Path;

use crate::cli_runtime::ProcRead;
use crate::lifecycle::Whereabouts;

/// Where the container whose group is `ctr` is, given what `crun state` named as its first
/// process (`pid`, an error when `crun` has no such container) and the agent's `/proc`
/// (`proc_read`).
///
/// The group is looked at after `crun state` named the process and before the process is read:
/// a process that is still there when it is read was there when the group was looked at, so a
/// group not there then is not the container's, and a container that ended in between is read
/// as gone (`NoProcess`), never as one that runs elsewhere.
pub(super) fn whereabouts_of(
    ctr: &Path,
    pid: Result<u32, String>,
    proc_read: ProcRead,
) -> Whereabouts {
    let Ok(pid) = pid else {
        return Whereabouts::NoProcess;
    };
    if ctr.is_dir() {
        return Whereabouts::Grouped(ctr.to_path_buf());
    }
    match proc_read(&format!("/proc/{pid}/cgroup")) {
        None => Whereabouts::NoProcess,
        Some(table) => Whereabouts::Elsewhere(group_of(&table)),
    }
}

/// The unified (`0::`) line's path of a `/proc/<pid>/cgroup`, or what the table says when it
/// has none, so that the finding names what the agent read.
fn group_of(table: &str) -> String {
    let unified = table.lines().find_map(|line| line.strip_prefix("0::"));
    match unified {
        Some(path) => path.to_owned(),
        None => format!("no unified group (read {:?})", table.trim()),
    }
}
