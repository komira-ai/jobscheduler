//! The post-job integrity checks (design 5.12): after every job, the agent compares the
//! host with what it was when it was last known clean, and a host that differs, or that the
//! agent cannot look at, is tainted (see [`crate::lifecycle::taint`]).
//!
//! [`Integrity`] is the [`CheckHook`] of an agent that runs jobs. It runs after the job's
//! container and scratch directory are gone and before the slot is freed, and it reads the
//! host through a [`HostView`]: [`ProcHost`] on Linux, [`FakeHost`] in a test.
//!
//! # The baseline
//!
//! [`Integrity::start`] takes the baseline when the agent starts; a host that cannot be
//! read whole then is no host to run jobs on, and the agent does not start. A check that
//! finds nothing takes the baseline again, and so does a job that starts on an idle host
//! (no other job's container, scratch directory or checks on it, and no container the agent
//! could not remove; see [`CheckHook::idle`]): what changed between jobs is the host's, not
//! a job's. So the baseline is the host as it was after the last job that left nothing, or
//! when the job started on an idle host. A check that finds something leaves it alone (the
//! host is tainted for good, and every later job is compared with the last clean state).
//!
//! # What is checked
//!
//! | code | what | kind |
//! |---|---|---|
//! | `process-as-job-user` | a process that was not there at the baseline whose host uid is in the range the jobs' user namespaces are mapped onto (`--container-subuid`: 65536 uids, so any process of any job's user, root included), outside the containers of the agent's other jobs and not of the uid of a slot whose job is still running; one of the finished job's own slot's uid is named as that slot's leftover | residue |
//! | `daemonised-child` | a process that was not there at the baseline and whose parent is `init` (pid 1), outside the containers of the agent's other jobs: what a child that outlived its parent looks like | residue |
//! | `new-mount` | a mount that was not there at the baseline (file system, mount point and source), other than those of the agent's other jobs' containers | residue |
//! | `user-changed`, `group-changed` | a line added to, removed from or changed in `/etc/passwd` or `/etc/group` | residue |
//! | `kernel-module-changed` | a module in `/proc/modules` that was not there at the baseline, or one that was and is gone (an unload is a finding too: a job holds no capability to change modules, and a host where an operator unloads modules while jobs run cannot be called clean) | residue |
//! | `watch-dir-changed` | the names, sizes, modification or status change times, inodes, owners or modes of the entries of a `--watch-dir` differ | residue |
//! | `processes-unreadable`, `mounts-unreadable`, `users-unreadable`, `groups-unreadable`, `modules-unreadable`, `watch-unreadable` | an input could not be read or understood | unchecked |
//! | `watch-over-bound` | a `--watch-dir` holds more than [`WATCH_ENTRY_CAP`] entries, so it cannot be digested | unchecked |
//!
//! A new process and a new mount count only when they are not the host's own (see
//! [What the host does while jobs run](#what-the-host-does-while-jobs-run)).
//!
//! A check that cannot run is a finding, never a skipped check. When several things are
//! found, the code is the first's (in the order of the table) and the detail holds all of
//! them, cut to [`CHECK_DETAIL_MAX`](crate::lifecycle::CHECK_DETAIL_MAX) bytes.
//!
//! The container cleanup failures of design 5.12 (a container or scratch directory that
//! could not be removed) are the lifecycle's own, and taint the host there.
//!
//! # What the agent must be able to see
//!
//! The checks read this machine's `/proc`, mounts and users, so they say something about a
//! job only when the job runs here and the agent sees what it does.
//! [`Integrity::start`] therefore refuses a host on which the agent is not in the pid and
//! mount namespaces of `init` (compared through `/proc/self/ns` and `/proc/1/ns`; see
//! [`require_host_namespaces`]), which is what an agent in a container, or under a service
//! manager's `PrivatePIDs`, `PrivateMounts` or `ProtectSystem`, looks like; and an agent
//! that cannot read those links (they need root, or `CAP_SYS_PTRACE`) does not start
//! either. Matching links do not prove it: in a container, or under `PrivatePIDs` with a
//! `/proc` of its own, `/proc/1` is that namespace's own `init` and the links match while
//! the jobs run on the host through the engine's socket. So the agent also requires what
//! only a host's `/proc` shows, when it starts: pid 2 is `kthreadd` with parent 0
//! (`/proc/2/stat`), and pid 1 is in the root control group or the init scope
//! (`/proc/1/cgroup`, `0::/` or `0::/init.scope`), not in a container's. And after each
//! container is started, the process the engine reports for it (`State.Pid`) must be in the
//! agent's `/proc`: when it is not, the container is removed, the start fails and the host
//! is tainted as unchecked (see [`CliRuntime`](crate::cli_runtime::CliRuntime)). A
//! container that has already exited when the agent looks has no process left to find and
//! is not checked this way. `--container-host` takes only a socket on this machine, for the
//! same reason.
//!
//! Every job runs in a user namespace mapped onto a subordinate range of the host, so the
//! processes of a job have host uids in that range and no other. These requirements hold
//! with such an engine: the agent still has to be in the pid and mount namespaces of
//! `init` (the engine's daemon is, too, and it reports `State.Pid` as a host pid), and the
//! user namespace of the containers is a child of the host's, which root reads the
//! `/proc/<pid>/ns` links of. The checks use the range the runtime resolved when it opened
//! (they never read `/etc/subuid` again, so an edit of it while the agent runs changes
//! nothing), and do not start without one. Docker's daemon is asked again before each
//! container is created whether it still remaps onto that range (see
//! [`CliRuntime`](crate::cli_runtime::CliRuntime)).
//!
//! # What is not checked
//!
//! - **A process of another user** that started during the job, unless its parent is
//!   `init`: a host where operators log in and services restart while jobs run has such
//!   processes, and the agent does not call every one of them the job's. A kernel thread
//!   (its parent is `kthreadd`, pid 2) is not a daemonised child.
//! - **Files outside the watched directories**, and the contents of any file. With no
//!   `--watch-dir` that is every file but `/etc/passwd` and `/etc/group` (no cron, systemd
//!   units, `authorized_keys`, sudoers, shadow or `ld.so.preload`), and the agent warns at
//!   start; the directories to watch are listed in [`crate::SUGGESTED_WATCH_DIRS`]. A
//!   watched directory is listed one level deep, and a file is judged by its metadata: a
//!   rewrite of a file in the directory with its modification time put back is still seen,
//!   because the status change time (which a job cannot set) moves. In a subdirectory only
//!   a name added, removed or renamed moves the subdirectory's own modification time; a
//!   write to a file that already exists in it changes nothing the listing reads, and is
//!   not seen (watch the subdirectory itself).
//! - **What a job leaves in a read-write mount root.** The mount roots and the scratch root
//!   change with every job and are excluded from the checks, and the content of a mount
//!   root is never examined: a binary left there, a poisoned cache or a changed script passes
//!   to the next job that mounts the same root.
//! - **The emptiness of a container's control group.** The container is removed, but the
//!   control group of its processes is not read to see that nothing is left in it; a
//!   process that left the group is seen only by the process checks above.
//! - **A job that the agent did not see end, partly.** The baseline is taken when the
//!   agent starts. From before the first step of a job's cleanup until its checks have run
//!   the state root holds a record that the checks are owed (see
//!   [`StateRoot::owe`](crate::lifecycle::StateRoot::owe)); an agent that starts and finds
//!   one taints the host, because what the job left behind may already be part of the new
//!   baseline. A job whose container was still running when the agent stopped is adopted
//!   and checked when it ends, but what it did before the stop is part of the baseline the
//!   new process took.
//! - **A mount's options**, remounts, and mounts that disappeared: only a new mount is a
//!   finding, and two identical mounts count as one. Modules compare by name only.
//! - **What a job does inside its container** (a container can fill its own file system),
//!   and anything a process does without leaving the process table, the mounts, the
//!   databases, the modules or a watched directory behind. This is a post-job check, not an
//!   audit: eBPF and the audit subsystem are not used.
//!
//! # What the host does while jobs run
//!
//! A host starts processes and makes mounts of its own while jobs run, and none of them is
//! a job's: a job cannot place a process outside its container's control group, nor make a
//! mount on the host. They are not counted:
//!
//! - **A process in a service of the system slice** (`/system.slice/<unit>.service`, as
//!   timers start them: a disk alarm every minute, statistics, package upgrades, firmware
//!   refreshes), **in the user slice** (`/user.slice/...`: a login's session and its
//!   `systemd --user`), **or in a Podman container's monitor scope**
//!   (`libpod-conmon-<id>.scope`, which outlives its container for a moment). The path of
//!   the unified hierarchy is read; a path that names a container (`libpod-<id>`,
//!   `docker-<id>`, `crio-`, `cri-containerd-`, `/docker/`, `libpod_parent`,
//!   `machine.slice`) always counts, wherever it is, and so does a scope under a system
//!   service. A job's process left in its own container's control group counts, and so
//!   does a process in the agent's own control group (read from `/proc/self/cgroup` when
//!   the agent starts; an agent that cannot read it does not start) or below it, other than
//!   a Podman monitor's scope there.
//! - **The direct runner's layout.** A container is the group `<actions>/<lease>/ctr`; the
//!   agent's own group is beside `actions/`, not above it, so a process left there would
//!   look like a system service's. A path with a `ctr` just below a `lease-<n>` group, or
//!   with a group named `actions` in it, names a container and counts. The runner's own
//!   processes (`crun run`, `kill`, `delete`, the keeper) are children of the agent, alive
//!   under a parent that is not `init`, in the agent's group exactly: they are not counted.
//! - **A runtime that cannot see a container's group** is blind: see
//!   [`Runtime::places_in_cgroup`](crate::lifecycle::Runtime::places_in_cgroup); the
//!   lifecycle taints the host as unchecked when it sees such a run, still running, with no group.
//! - **A network namespace Podman pins** (`nsfs` at `/run/netns/<name>`: one per container,
//!   a neighbour's included, named without the container id), **the bind of `/run/netns`
//!   onto itself** (`tmpfs`, root `/netns`) that the first container after a boot makes,
//!   **and a user's run directory** (`tmpfs` at `/run/user/<uid>`), made by a login.
//!
//! What is not on this list still counts: Kubernetes pods, a container engine's other
//! mounts, and a service started in a scope. A host with such churn can still be tainted
//! when it overlaps a job's end.
//!
//! # Jobs that run side by side
//!
//! The agent runs several jobs at once, so the host at the end of one job holds the
//! containers of the others. [`Finished::live`] lists their ids; a process whose control
//! group path holds one of them, and a mount whose root, mount point or source does, are
//! not counted against the finished job. An engine that names its mounts or places its
//! helper processes without the container id does not match, and the neighbour then shows
//! as a finding: Podman's overlay layers, and with Docker the overlay mount (named by its
//! layer id) and the container's `containerd-shim` process (whose parent is `init` and
//! whose control group is the containerd service's), so two jobs at once on a Docker or
//! Podman host can taint it when the first of them ends. Run one job at a time there, or
//! accept that such a host is tainted. This is stated from how the engines work, not from
//! a measurement on a host.
//!
//! A container user that is the agent's own user (`--container-user` for an agent that
//! does not run as root) makes every process of the agent's user that is not in the
//! baseline, and not in the host's own control groups, a finding: a child of the agent
//! that left its service's control group included.
//!
//! # macOS
//!
//! The checks read `/proc`, which a Mac does not have, and there is no backend for a Mac.
//! A container job is Linux-only for that reason: the agent refuses `--container-bin` on a
//! Mac when it starts, so a Mac agent runs no job and no check. This is stated, not
//! hidden: `js-agent` does not pretend that a Mac host was checked.

mod proc;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::cli_runtime::{IdRange, JobUsers};
use crate::lifecycle::{CheckFinding, CheckHook, Finished};

pub use proc::{DirLister, Entry, Listing, PROCESS_CAP, ProcHost, SystemDirs, WATCH_ENTRY_CAP};
pub use view::{FakeHost, HostView, Mount, Process, Watched, input};

/// The codes of the findings of the checks.
pub mod code {
    /// A process of the container user outlived the job.
    pub const PROCESS_AS_JOB_USER: &str = "process-as-job-user";
    /// A process whose parent is `init` outlived the job.
    pub const DAEMONISED_CHILD: &str = "daemonised-child";
    /// A mount that was not there before.
    pub const NEW_MOUNT: &str = "new-mount";
    /// The user database changed.
    pub const USER_CHANGED: &str = "user-changed";
    /// The group database changed.
    pub const GROUP_CHANGED: &str = "group-changed";
    /// A kernel module was loaded or unloaded.
    pub const MODULE_CHANGED: &str = "kernel-module-changed";
    /// A watched directory changed.
    pub const WATCH_DIR_CHANGED: &str = "watch-dir-changed";
    /// The process table could not be read.
    pub const PROCESSES_UNREADABLE: &str = "processes-unreadable";
    /// The mounts could not be read.
    pub const MOUNTS_UNREADABLE: &str = "mounts-unreadable";
    /// The user database could not be read.
    pub const USERS_UNREADABLE: &str = "users-unreadable";
    /// The group database could not be read.
    pub const GROUPS_UNREADABLE: &str = "groups-unreadable";
    /// The kernel modules could not be read.
    pub const MODULES_UNREADABLE: &str = "modules-unreadable";
    /// A watched directory could not be listed.
    pub const WATCH_UNREADABLE: &str = "watch-unreadable";
    /// A watched directory holds more entries than the checks read.
    pub const WATCH_OVER_BOUND: &str = "watch-over-bound";
}

/// One thing a check noticed: its code and what it saw.
struct Problem {
    code: &'static str,
    detail: String,
    /// The check could not run, as opposed to having found something.
    unrun: bool,
}

impl Problem {
    fn found(code: &'static str, detail: String) -> Self {
        Self {
            code,
            detail,
            unrun: false,
        }
    }

    fn unrun(code: &'static str, detail: String) -> Self {
        Self {
            code,
            detail,
            unrun: true,
        }
    }
}

/// The control groups of `init` that only a host's `/proc` shows: the root, or the scope a
/// service manager gives `init`.
const INIT_CGROUPS: [&str; 2] = ["/", "/init.scope"];

/// Checks that the agent sees the host's processes and mounts. `link` reads a symbolic
/// link (the namespace links of `/proc`) and `read` a file.
///
/// The agent must be in the pid and mount namespaces of `init`. An agent in a namespace of
/// its own (in a container, or under a service manager's `PrivatePIDs`, `PrivateMounts` or
/// `ProtectSystem`) cannot see its jobs' processes or the overlay mounts of their
/// containers, and every check would pass. Reading `/proc/1/ns` takes the privilege to
/// trace `init` (root, or `CAP_SYS_PTRACE`): an agent without it cannot tell and does not
/// start.
///
/// The namespace links are not enough: in a container, and under `PrivatePIDs` with a
/// `/proc` of its own, `/proc/1` is that namespace's own `init` and the links match. So
/// two more things that only a host's `/proc` shows are required: pid 2 is `kthreadd`
/// with parent 0 (`/proc/2/stat`), and pid 1 is in the root control group or the init
/// scope (`/proc/1/cgroup`), not in a container's.
///
/// # Errors
///
/// The text says which namespace differs, which link could not be read, or which of the
/// two files shows a `/proc` that is not the host's.
pub fn require_host_namespaces(
    link: &dyn Fn(&str) -> Result<String, String>,
    read: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    for kind in ["pid", "mnt"] {
        let namespace = |who: &str| {
            link(&format!("/proc/{who}/ns/{kind}"))
                .map_err(|why| format!("cannot tell whether the agent sees the host: {why}"))
        };
        let (own, init) = (namespace("self")?, namespace("1")?);
        if own != init {
            return Err(format!(
                "the agent is in a {kind} namespace of its own ({own}; init is in {init}): it cannot see the processes and mounts of its jobs"
            ));
        }
    }
    let not_the_hosts = |why: String| format!("the agent's /proc is not the host's: {why}");
    kthreadd_is_pid_2(read).map_err(not_the_hosts)?;
    init_is_in_the_root_cgroup(read).map_err(not_the_hosts)
}

/// Pid 2 is `kthreadd`, whose parent is 0: what a host's `/proc` shows and a container's,
/// which holds no kernel threads, does not.
fn kthreadd_is_pid_2(read: &dyn Fn(&str) -> Result<String, String>) -> Result<(), String> {
    let stat = read("/proc/2/stat").map_err(|why| format!("pid 2 cannot be read ({why})"))?;
    match stat_of(&stat) {
        Some(("kthreadd", 0)) => Ok(()),
        Some((name, parent)) => Err(format!(
            "pid 2 is {name:?} with parent {parent}, not kthreadd with parent 0"
        )),
        None => Err("the stat entry of pid 2 cannot be understood".to_owned()),
    }
}

/// The name and the parent in the text of a `stat` file. The name is in parentheses and may
/// hold spaces and parentheses, so the fields are counted from the last parenthesis.
fn stat_of(stat: &str) -> Option<(&str, u32)> {
    stat.find('(')
        .zip(stat.rfind(')'))
        .and_then(|(open, close)| {
            let name = stat.get(open + 1..close);
            let parent = stat
                .get(close + 1..)
                .and_then(|rest| rest.split_whitespace().nth(1))
                .and_then(|field| field.parse().ok());
            name.zip(parent)
        })
}

/// Every control group of pid 1 is the root or the init scope (`0::/` or `0::/init.scope`
/// on a unified hierarchy). A container's pid 1 is in a control group of the container's.
fn init_is_in_the_root_cgroup(read: &dyn Fn(&str) -> Result<String, String>) -> Result<(), String> {
    let text = read("/proc/1/cgroup")
        .map_err(|why| format!("pid 1's control groups cannot be read ({why})"))?;
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let outside = lines.iter().find(|line| {
        let path = line.splitn(3, ':').nth(2);
        !path.is_some_and(|path| INIT_CGROUPS.contains(&path))
    });
    match (lines.is_empty(), outside) {
        (true, _) => Err("pid 1 has no control group".to_owned()),
        (false, Some(line)) => Err(format!(
            "pid 1 is in the control group {line:?}, not the root or the init scope"
        )),
        (false, None) => Ok(()),
    }
}

/// The host at one moment.
#[derive(Default)]
struct Snapshot {
    processes: Vec<Process>,
    mounts: BTreeSet<Mount>,
    users: BTreeSet<String>,
    groups: BTreeSet<String>,
    modules: BTreeSet<String>,
    watched: BTreeMap<PathBuf, String>,
}

impl Snapshot {
    /// The snapshot without what belongs to the containers still on the host beside `job`:
    /// a baseline that held a neighbour's processes and mounts would hide what is left of
    /// them once their container is gone.
    fn without_live(mut self, job: &Finished<'_>) -> Self {
        self.processes
            .retain(|process| !is_live(job, &process.cgroup));
        self.mounts.retain(|mount| {
            !(is_live(job, &mount.root)
                || is_live(job, &mount.point)
                || is_live(job, &mount.source))
        });
        self
    }
}

/// Whether `text` (a control group path, a mount's root, point or source) names one of the
/// containers still on the host beside the finished job.
fn is_live(job: &Finished<'_>, text: &str) -> bool {
    job.live.iter().any(|id| text.contains(&id.0))
}

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
/// The agent's own control group (`own`, its unified path) and every group below it are
/// not the host's: a child of the agent that outlives it is in the agent's service, and is
/// judged like any other process. A Podman monitor's scope is the host's even there.
fn hosts_own_process(cgroup: &str, own: Option<&str>) -> bool {
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
    let agents = own.is_some_and(|own| {
        path == own
            || path
                .strip_prefix(own)
                .is_some_and(|rest| rest.starts_with('/'))
    });
    if agents {
        return false;
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

/// The path of the unified hierarchy (`0::`) in the control groups `cgroup` (the lines of
/// `/proc/<pid>/cgroup`), if there is one.
fn unified(cgroup: &str) -> Option<&str> {
    cgroup
        .split_whitespace()
        .find_map(|line| line.strip_prefix("0::"))
}

/// The name of the runner's group that holds every lease's group (js-contain's `actions/`).
const ACTIONS: &str = "actions";

/// Whether the control groups `units` (the names in a unified path) hold a container's: a
/// group an engine makes for one (`libpod-<id>`, `docker-<id>`, ...), the direct runner's
/// `<lease>/ctr` (a `ctr` just below a `lease-<n>` group, however deep the path goes on, and
/// wherever the lease group sits), or any group in or below the runner's `actions` group,
/// which holds nothing but leases. A name that only looks like these (`actions-old`,
/// `lease-1/ctrl`, `xlease-1/ctr`) is not one.
fn names_a_container(units: &[&str]) -> bool {
    const ENGINES: [&str; 4] = ["libpod-", "docker-", "crio-", "cri-containerd-"];
    let engine = |unit: &&str| {
        ENGINES.iter().any(|prefix| unit.starts_with(prefix))
            || ["docker", "libpod_parent", "machine.slice"].contains(unit)
    };
    units.iter().any(engine)
        || units.contains(&ACTIONS)
        || units
            .windows(2)
            .any(|pair| pair[0].starts_with("lease-") && pair[1] == "ctr")
}

/// Whether `process` is one of the direct runner's own: a child of the agent (its parent is
/// alive and is not `init`) in the agent's own control group exactly. The runner's `crun
/// run`, `crun kill` and `crun delete` and the keeper are started by the agent and stay in
/// its group; no job process can be in it, since a container's processes are in its own
/// group and cannot leave it. A process the agent no longer holds (parent `init`) is not
/// the runner's, nor is one in a group below the agent's.
fn runners_own(process: &Process, own: Option<&str>) -> bool {
    process.ppid != 1 && unified(&process.cgroup).is_some_and(|path| Some(path) == own)
}

/// Whether `mount` is one the host makes while jobs run, which no job can have left: the
/// network namespaces Podman pins under `/run/netns` (one per container, the neighbours'
/// included, whose names hold no container id), the bind of `/run/netns` onto itself that
/// the first container after a boot makes, and the run directory of a user who logs in
/// (`/run/user/<uid>`).
fn hosts_own_mount(mount: &Mount) -> bool {
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

/// The post-job checks: a [`HostView`], the user the containers run as, the watched
/// directories, and the baseline.
pub struct Integrity {
    view: Box<dyn HostView>,
    users: IdRange,
    /// The container uid of slot 0, when the containers run as the slots' users.
    first_slot_uid: Option<u32>,
    watch: Vec<PathBuf>,
    baseline: Snapshot,
    /// The agent's own control group (its unified path), which is never the host's.
    own: Option<String>,
}

impl Integrity {
    /// The checks for a host read through `view`, whose containers' processes are those of
    /// `users`, with the directories `watch` watched, and the baseline taken now.
    ///
    /// # Errors
    ///
    /// The host cannot be read whole, so there is nothing to compare with later, or the
    /// agent's own control group (`/proc/self/cgroup`) cannot be read: the text says which
    /// input failed. A mapped range that `view` shows is not acceptable is one too.
    pub fn start(
        view: Box<dyn HostView>,
        users: JobUsers,
        watch: Vec<PathBuf>,
    ) -> Result<Self, String> {
        require_host_namespaces(&|path| view.link(path), &|path| view.file(path))?;
        let users = users.range().map_err(|why| why.to_string())?;
        let own = view
            .file("/proc/self/cgroup")
            .map_err(|why| format!("cannot read the agent's own control group: {why}"))?;
        let own = unified(&own)
            .filter(|path| !path.trim_matches('/').is_empty())
            .map(|path| path.trim_end_matches('/').to_owned());
        let mut this = Self {
            view,
            users,
            first_slot_uid: None,
            watch,
            baseline: Snapshot::default(),
            own,
        };
        this.baseline = this.observe().map_err(|problems| {
            format!(
                "cannot take the baseline of the host: {}",
                details(&problems)
            )
        })?;
        Ok(this)
    }

    /// Tells the checks that the containers run as the slots' users, the container uid of
    /// slot `k` being `first_uid + k` (see
    /// [`Settings::slot_ids`](crate::cli_runtime::Settings::slot_ids)), mapped onto the
    /// host uid `start + first_uid + k` of the range. After a run in slot `k`, a leftover
    /// at that host uid is named as the slot's leftover; a leftover at the uid of another
    /// slot is a finding like any other, unless that slot's run is live
    /// ([`Finished::live_slots`]).
    #[must_use]
    pub fn with_slots(self, first_uid: u32) -> Self {
        Self {
            first_slot_uid: Some(first_uid),
            ..self
        }
    }

    /// The host uid of slot `slot`, when the checks know the slots' users.
    fn slot_host_uid(&self, slot: u32) -> Option<u32> {
        self.first_slot_uid.map(|first| {
            self.users
                .start()
                .saturating_add(first)
                .saturating_add(slot)
        })
    }

    /// The host now, or why it cannot be read whole: every input is tried, so the reasons
    /// are all given.
    fn observe(&self) -> Result<Snapshot, Vec<Problem>> {
        let mut problems = Vec::new();
        let processes = keep(
            &mut problems,
            code::PROCESSES_UNREADABLE,
            self.view.processes(),
        );
        let mounts = keep(&mut problems, code::MOUNTS_UNREADABLE, self.view.mounts());
        let users = keep(&mut problems, code::USERS_UNREADABLE, self.view.users());
        let groups = keep(&mut problems, code::GROUPS_UNREADABLE, self.view.groups());
        let modules = keep(&mut problems, code::MODULES_UNREADABLE, self.view.modules());
        let mut watched = BTreeMap::new();
        for dir in &self.watch {
            match self.view.watched(dir) {
                Ok(Watched::Digest(digest)) => {
                    watched.insert(dir.clone(), digest);
                }
                Ok(Watched::TooMany) => problems.push(Problem::unrun(
                    code::WATCH_OVER_BOUND,
                    format!("{}: more than {WATCH_ENTRY_CAP} entries", dir.display()),
                )),
                Err(why) => problems.push(Problem::unrun(code::WATCH_UNREADABLE, why)),
            }
        }
        match problems.is_empty() {
            true => Ok(Snapshot {
                processes,
                mounts: mounts.into_iter().collect(),
                users,
                groups,
                modules,
                watched,
            }),
            false => Err(problems),
        }
    }

    /// What differs between the baseline and `now`, in the order of the table of the module.
    fn compare(&self, now: &Snapshot, job: &Finished<'_>) -> Vec<Problem> {
        let mine = |text: &str| is_live(job, text);
        let known: BTreeSet<(u32, u64)> =
            self.baseline.processes.iter().map(Process::key).collect();
        let strays: Vec<&Process> = now
            .processes
            .iter()
            .filter(|process| {
                !known.contains(&process.key())
                    && !mine(&process.cgroup)
                    && !hosts_own_process(&process.cgroup, self.own.as_deref())
                    && !runners_own(process, self.own.as_deref())
            })
            .collect();
        let own = job.slot.and_then(|slot| self.slot_host_uid(slot));
        let live_uids: BTreeSet<u32> = job
            .live_slots
            .iter()
            .filter_map(|slot| self.slot_host_uid(*slot))
            .collect();
        let as_job = strays
            .iter()
            .filter(|stray| self.users.contains(stray.uid) && !live_uids.contains(&stray.uid));
        let daemonised = strays.iter().filter(|stray| stray.ppid == 1);
        let new_mounts = now
            .mounts
            .difference(&self.baseline.mounts)
            .filter(|mount| {
                !(mine(&mount.root)
                    || mine(&mount.point)
                    || mine(&mount.source)
                    || hosts_own_mount(mount))
            });
        let moved = now
            .watched
            .iter()
            .filter(|(dir, digest)| self.baseline.watched.get(*dir) != Some(*digest));
        [
            group(
                code::PROCESS_AS_JOB_USER,
                as_job.map(|stray| {
                    let slot = job
                        .slot
                        .filter(|_| own == Some(stray.uid))
                        .map_or(String::new(), |slot| format!(" (leftover of slot {slot})"));
                    format!(
                        "pid {} of user {} is running after job {}{slot}",
                        stray.pid, stray.uid, job.run_id
                    )
                }),
            ),
            group(
                code::DAEMONISED_CHILD,
                daemonised.map(|stray| {
                    format!(
                        "pid {} was left behind by job {} (parent init)",
                        stray.pid, job.run_id
                    )
                }),
            ),
            group(
                code::NEW_MOUNT,
                new_mounts.map(|mount| {
                    format!("{} ({}) from {}", mount.point, mount.fstype, mount.source)
                }),
            ),
            group(
                code::USER_CHANGED,
                changed(&self.baseline.users, &now.users),
            ),
            group(
                code::GROUP_CHANGED,
                changed(&self.baseline.groups, &now.groups),
            ),
            group(
                code::MODULE_CHANGED,
                now.modules
                    .difference(&self.baseline.modules)
                    .map(|name| format!("loaded {name}"))
                    .chain(
                        self.baseline
                            .modules
                            .difference(&now.modules)
                            .map(|name| format!("unloaded {name}")),
                    ),
            ),
            group(
                code::WATCH_DIR_CHANGED,
                moved.map(|(dir, _)| format!("{} differs from the baseline", dir.display())),
            ),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// `read`, or an empty one and the reason in `problems` when the input could not be read.
fn keep<T: Default>(problems: &mut Vec<Problem>, code: &'static str, read: Result<T, String>) -> T {
    read.unwrap_or_else(|why| {
        problems.push(Problem::unrun(code, why));
        T::default()
    })
}

/// The names in the lines that are in one of `before` and `after` and not in the other.
fn changed(before: &BTreeSet<String>, after: &BTreeSet<String>) -> Vec<String> {
    let names: BTreeSet<&str> = before
        .symmetric_difference(after)
        .map(|line| line.split(':').next().unwrap_or_default())
        .collect();
    names.into_iter().map(str::to_owned).collect()
}

/// One problem of `code` for the things `seen` names, or none for no things.
fn group(code: &'static str, seen: impl IntoIterator<Item = String>) -> Option<Problem> {
    let seen: Vec<String> = seen.into_iter().collect();
    (!seen.is_empty()).then(|| Problem::found(code, seen.join(", ")))
}

/// The details of `problems`, one after the other.
fn details(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(|problem| format!("{}: {}", problem.code, problem.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

impl CheckHook for Integrity {
    /// Takes the baseline again: no job is on the host, so what changed since the last
    /// clean check (an operator's login, a timer's service, a package upgrade) is the
    /// host's. A host that cannot be read keeps the baseline it had; the check after the job
    /// finds it unreadable.
    fn idle(&mut self) {
        if let Ok(now) = self.observe() {
            self.baseline = now;
        }
    }

    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        let problems = match self.observe() {
            Err(unrun) => unrun,
            Ok(now) => {
                let found = self.compare(&now, job);
                if found.is_empty() {
                    self.baseline = now.without_live(job);
                }
                found
            }
        };
        match problems.first() {
            None => Ok(()),
            Some(first) if first.unrun => {
                Err(CheckFinding::unchecked(first.code, &details(&problems)))
            }
            Some(first) => Err(CheckFinding::residue(first.code, &details(&problems))),
        }
    }
}
