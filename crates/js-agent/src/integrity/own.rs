//! What the checks count as the host's own and what as a live container's: the control
//! group and mount rules that tell a process or mount of the host, or of a container of
//! another job still running, from one a finished job could have left. A contained job can
//! place no process outside its container's control group and make no mount on the host, so
//! what is the host's here is never judged against a job. Each rule is stated where it is
//! written; the module doc of [`super`] says how the checks use them.

use std::path::Path;

use super::{Mount, Process};
use crate::lifecycle::Finished;

/// Whether the mount point `point` lies below a directory where the runtime mounts the file
/// system of another job the agent holds ([`Finished::held_mounts`]), by whole components.
pub(super) fn under_held(job: &Finished<'_>, point: &str) -> bool {
    let point = Path::new(point);
    job.held_mounts
        .iter()
        .any(|dir| point != dir && point.starts_with(dir))
}

/// Whether `text` (a control group path, the lines of `/proc/<pid>/cgroup`, or a mount's
/// root, point or source) names one of the containers still on the host beside the
/// finished job: some component of it, between `/` or white space, is the container's
/// handle, or is an engine's name for it (`libpod-<id>.scope`, `docker-<id>.scope`, ...).
/// A handle that is only part of a longer name does not match: run ids are `run-<n>-<m>`,
/// so `run-3-1` is the start of the live `run-3-12`'s name.
pub(super) fn is_live(job: &Finished<'_>, text: &str) -> bool {
    text.split(|c: char| c == '/' || c.is_whitespace())
        .any(|part| job.live.iter().any(|id| names_container(part, &id.0)))
}

/// Whether the path component `part` is the container `id`: its handle, or an engine's
/// control group name for it, with or without the `.scope`.
fn names_container(part: &str, id: &str) -> bool {
    part == id
        || ENGINES.iter().any(|prefix| {
            part.strip_prefix(prefix)
                .is_some_and(|rest| rest.strip_suffix(".scope").unwrap_or(rest) == id)
        })
}

/// The prefixes of the control group names container engines give a container.
const ENGINES: [&str; 4] = ["libpod-", "docker-", "crio-", "cri-containerd-"];

/// Whether a process in the control groups `cgroup` (the lines of `/proc/<pid>/cgroup`) is
/// the host's own, which no job can have left: one in a service of the system slice, in the
/// user slice (logins, `systemd --user`), or in the scope of a Podman container's monitor
/// (`libpod-conmon-<id>.scope`, which outlives its container for a moment). Only the path
/// of the unified hierarchy (`0::`) is read; a process without one is not the host's.
///
/// A container's processes are in a control group that names the container
/// (`libpod-<id>.scope`, `docker-<id>.scope`, `/docker/<id>`), and a contained job cannot
/// move a process out of it: the control group file system is read-only in the container.
/// A path that holds such a name is never the host's, wherever it is, so a container placed
/// under the system or the user slice still counts.
///
/// A process with parent `init` that is not in a group a job can be in ([`job_can_be_in`])
/// is not a `daemonised-child` either; that is judged separately, after this.
///
/// The agent's own control group (`own`, its unified path) and every group below it are
/// not the host's: a child of the agent that outlives it is in the agent's service, and is
/// judged like any other process. A Podman monitor's scope is the host's even there.
pub(super) fn hosts_own_process(cgroup: &str, own: Option<&str>) -> bool {
    let Some(path) = unified(cgroup) else {
        return false;
    };
    let units: Vec<&str> = path.split('/').filter(|unit| !unit.is_empty()).collect();
    // A Podman monitor's scope is the host's wherever it is, the agent's own group included
    // (containers parented under the agent's group put their monitors there too): a job
    // cannot place a process in it.
    let monitor = units
        .last()
        .is_some_and(|unit| unit.starts_with("libpod-conmon-") && unit.ends_with(".scope"));
    if monitor {
        return true;
    }
    if in_or_below(path, own) {
        return false;
    }
    if runners_helper(&units) {
        return true;
    }
    if names_a_container(&units) {
        return false;
    }
    match units.as_slice() {
        ["user.slice", ..] => true,
        ["system.slice", unit, rest @ ..] => {
            unit.ends_with(".service") && !rest.iter().any(|unit| unit.ends_with(".scope"))
        }
        _ => false,
    }
}

/// Whether the unified path `path` is the agent's own control group `own` or lies below it.
fn in_or_below(path: &str, own: Option<&str>) -> bool {
    own.is_some_and(|own| {
        path == own
            || path
                .strip_prefix(own)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Whether a job's process can be in the control groups `cgroup` (the lines of
/// `/proc/<pid>/cgroup`): the agent's own group (`own`) or one below it, a group that names
/// a container ([`names_a_container`]), or a process with no unified path, which is judged
/// conservatively. A contained job cannot place a process in any other group: the control
/// group file system is read-only in its container and it holds no capability, so a process
/// elsewhere (the root group, `init.scope`, a scope of the system slice, another slice) is
/// the host's, whatever its parent.
pub(super) fn job_can_be_in(cgroup: &str, own: Option<&str>) -> bool {
    let Some(path) = unified(cgroup) else {
        return true;
    };
    let units: Vec<&str> = path.split('/').filter(|unit| !unit.is_empty()).collect();
    in_or_below(path, own) || names_a_container(&units)
}

/// The path of the unified hierarchy (`0::`) in the control groups `cgroup` (the lines of
/// `/proc/<pid>/cgroup`), if there is one.
pub(super) fn unified(cgroup: &str) -> Option<&str> {
    cgroup
        .split_whitespace()
        .find_map(|line| line.strip_prefix("0::"))
}

/// The name of the runner's group that holds every lease's group (js-contain's `actions/`).
const ACTIONS: &str = "actions";

/// The group below `actions/` that holds the helper containers (unpack and removal).
const HELPERS: &str = "helpers";

/// Whether the control groups `units` (the names in a unified path) hold a container's: a
/// group an engine makes for one (`libpod-<id>`, `docker-<id>`, ...), or any group in or
/// below the direct runner's `actions` group, which holds nothing but leases (a lease's group
/// is named for its container, `js-<agent tag>-<run id>`, and the container's is `ctr`
/// below it) and the helper containers' group. A name that only looks like `actions`
/// (`actions-old`) is not one.
fn names_a_container(units: &[&str]) -> bool {
    let engine = |unit: &&str| {
        ENGINES.iter().any(|prefix| unit.starts_with(prefix))
            || ["docker", "libpod_parent", "machine.slice"].contains(unit)
    };
    units.iter().any(engine) || units.contains(&ACTIONS)
}

/// Whether the control groups `units` are those of a helper container of the runner:
/// `actions/helpers` and below, where `actions` is the real actions group, not one made
/// inside a lease (no `ctr`, and no lease group `js-<16 hex digits>-<run id>`, before it).
/// The helpers (the image unpacker, the removal of a lease's tree) run inside a runtime
/// call, but a background pull runs one while the lifecycle checks another job, so a helper
/// can be alive at a check; it is the runner's, not a stray. A helper that never ends is
/// therefore not found here: the runtime's own call ends it, not the post-job checks.
///
/// The invariant behind the exemption: code a job controls never runs as the job user (the
/// uid of the agent's helpers and of `crun`) on the host. It runs at `sub_uid + 1000`, in a
/// user namespace, with no capabilities, so it cannot place a process in `actions/helpers`
/// or write a cgroup there.
fn runners_helper(units: &[&str]) -> bool {
    let inside_a_lease = |unit: &&str| *unit == "ctr" || is_lease_group(unit);
    units
        .iter()
        .position(|unit| *unit == ACTIONS)
        .is_some_and(|at| {
            units.get(at + 1) == Some(&HELPERS) && !units[..at].iter().any(inside_a_lease)
        })
}

/// Whether `unit` is the name of a lease's group: `js-`, the agent's tag of 16 hexadecimal
/// digits, `-` and the run id.
fn is_lease_group(unit: &str) -> bool {
    unit.strip_prefix("js-")
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(tag, run)| {
            tag.len() == 16 && tag.bytes().all(|b| b.is_ascii_hexdigit()) && !run.is_empty()
        })
}

/// Whether `process` is one of the direct runner's own: it is in the agent's own control
/// group exactly, and its parent is not `init` (the process table does not say whose
/// child it is, so any live parent will do). The runner's `crun run`, `crun kill` and
/// `crun delete` and the keeper are started by the agent and stay in its group; no job
/// process can be in it, since a container's processes are in its own group and cannot
/// leave it. A process whose parent is `init` (the agent no longer holds it) is not the
/// runner's, nor is one in a group below the agent's.
pub(super) fn runners_own(process: &Process, own: Option<&str>) -> bool {
    process.ppid != 1 && unified(&process.cgroup).is_some_and(|path| Some(path) == own)
}

/// Whether `mount` is one the host makes while jobs run, which no job can have left: the
/// network namespaces Podman pins under `/run/netns` (one per container, the neighbours'
/// included, whose names hold no container id), the bind of `/run/netns` onto itself that
/// the first container after a boot makes, and the run directory of a user who logs in
/// (`/run/user/<uid>`).
pub(super) fn hosts_own_mount(mount: &Mount) -> bool {
    let one_name = |rest: &str| !rest.is_empty() && !rest.contains('/');
    match mount.fstype.as_str() {
        "nsfs" => mount
            .point
            .strip_prefix("/run/netns/")
            .is_some_and(one_name),
        "tmpfs" => {
            (mount.point == "/run/netns" && mount.root == "/netns")
                || mount
                    .point
                    .strip_prefix("/run/user/")
                    .is_some_and(|uid| one_name(uid) && uid.bytes().all(|b| b.is_ascii_digit()))
        }
        _ => false,
    }
}
