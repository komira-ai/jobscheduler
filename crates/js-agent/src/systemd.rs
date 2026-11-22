//! What the agent and its update hook need to know of the host's systemd: its version, and the
//! first version that starts a service's processes in the unit's own cgroup.
//!
//! From systemd 258 on, a service's `ExecStartPre=` and `ExecStart=` processes are cloned
//! straight into the unit's own cgroup and move into its `DelegateSubgroup=` only after; 255 to
//! 257 cloned them into the subgroup (the processes spawned once the main process runs, such as
//! `ExecStopPost=`, go to `<unit>/.control` on every version). The kernel refuses a process in a
//! cgroup that has controllers enabled for its children (cgroup v2's no-internal-process rule),
//! and systemd cannot switch them off while a child cgroup uses them. So a unit whose cgroup
//! still holds lease cgroups with their controllers, the jobs a hand-over or a crash left running
//! under `KillMode=process`, cannot be started on 258 or newer until they end: every start fails
//! with `Device or resource busy` and the result `resources`.

/// The first systemd that clones a service's first process into the unit's own cgroup.
pub const OWN_CGROUP_START: u32 = 258;

/// The version `systemctl --version` printed: the number after `systemd` on its first line
/// (`systemd 259 (259.5-0ubuntu3.4)`); `None` for anything else.
#[must_use]
pub fn version(printed: &str) -> Option<u32> {
    let mut words = printed.lines().next()?.split_whitespace();
    if words.next()? != "systemd" {
        return None;
    }
    words.next()?.parse().ok()
}

#[cfg(test)]
#[path = "systemd_tests.rs"]
mod tests;
