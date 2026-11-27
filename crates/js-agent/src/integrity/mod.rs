//! The post-job integrity checks (design 5.12): after every job, the agent compares the
//! host with what it was when it was last known clean. A job's process that is still running
//! taints the host, and so does a host whose process table the agent cannot read (see
//! [`crate::lifecycle::taint`]); every other change is the host's own doing, and is logged
//! as one line (a [`CheckNote`]) without a taint.
//!
//! [`Integrity`] is the [`CheckHook`] of an agent that runs jobs. It runs after the job's
//! container and scratch directory are gone and before the slot is freed, and it reads the
//! host through a [`HostView`]: [`ProcHost`] on Linux, [`FakeHost`] in a test.
//!
//! # What taints, and what does not
//!
//! A check taints only for something a contained job could have left behind that the sandbox
//! cannot prevent and that would affect a later job. A package install by the host's updater
//! while jobs run must never taint (it changes `/etc/passwd` and `/etc/group`, may start
//! services, mount file systems and load kernel modules), and a taint whose cause is gone
//! clears itself when the agent checks again. Every taint says what changed in one line.
//!
//! A contained job runs in a user namespace of its own, with a mount namespace and a pid
//! namespace of its own, no capability on the host and a seccomp profile that refuses kernel
//! module calls. What that leaves it:
//!
//! - **A process of the job users' range that outlives the job.** The sandbox is meant to
//!   end every process of a job with its container; one still running is a sandbox failure,
//!   and it shares its uids and mount roots with the later jobs. This taints
//!   (`process-as-job-user`), and the taint ends when the process does.
//! - **Nothing else on the host.** The orphans of a contained job are reparented to the init
//!   of its own pid namespace and die with it, so a process whose parent is the host's `init`
//!   is the runtime's or the host's. Every mount a contained job makes is in its own mount
//!   namespace, and the mounts the agent makes for a job that fail to unmount already taint
//!   as `cleanup-failed`. A job cannot write the host's `/etc` (the host's root owns it and
//!   nothing mounts it writable for an unmapped id), so the user and group databases are
//!   the host's. `init_module`, `finit_module` and `delete_module` fail for a job (the
//!   profile, no capability, and the check that the caller is in the init namespace), the
//!   kernel only loads in-tree modules on a process's behalf, and no job unloads one. A
//!   `--watch-dir` may not hold or lie in a mount root, the scratch root or the state root,
//!   so no contained job can write one. A host job (`isolation=host`) has none of these
//!   walls and taints `host-job` whatever the checks find.
//!
//! # The baseline
//!
//! [`Integrity::start`] takes the baseline when the agent starts; a host that cannot be
//! read whole then is no host to run jobs on, and the agent does not start. A check that
//! finds no leftover takes the baseline again, and so does a job that starts on an idle host
//! (no other job's container, scratch directory or checks on it, and no container the agent
//! could not remove; see [`CheckHook::idle`]): what changed between jobs is the host's, not
//! a job's. So the baseline is the host as it was after the last job that left nothing, or
//! when the job started on an idle host. A check that finds a leftover keeps the baseline's
//! processes of the job users' range (the host is tainted, and every later check is compared
//! with them) and takes everything else that only the notes look at: the users, the groups,
//! the modules, the mounts, the watched directories and the other processes, without the
//! neighbours' containers. So a noted change is logged once, by the check that sees it, and
//! not again after every later check. A check that cannot run (the process table cannot be
//! read) leaves the baseline alone. An input other than the process table that cannot be
//! read is a note, and its value in the baseline stays what it was: it is taken as
//! unchanged.
//!
//! The re-checks of a taint that may clear itself (see [`crate::lifecycle::taint`]) are
//! these same checks, run on an idle host with nothing live: one that passes takes the
//! baseline again, as after any job that leaves nothing. The re-check of a taint read from
//! the marker at start ([`CheckHook::run_restored`]) is the same check with one change: the
//! baseline exempts no process, because this process took it with whatever the tainting job
//! left. A process of the job users' range is a finding even if the baseline holds it (the
//! agent's own process, read from `/proc/self/stat` when the agent starts, is never one).
//! A marker written by an earlier version for one of the checks that now only note
//! (`daemonised-child`, `new-mount`, `user-changed`, `group-changed`,
//! `kernel-module-changed`, `watch-dir-changed`) is re-checked the same way, and passes
//! unless a job's process is left.
//!
//! # What is checked
//!
//! | code | what | kind |
//! |---|---|---|
//! | `process-as-job-user` | a process that was not there at the baseline whose host uid is in the range the jobs' user namespaces are mapped onto (`--container-subuid`: 65536 uids, so any process of any job's user, root included), outside the containers of the agent's other jobs and not of the uid of a slot whose job is still running; one of the finished job's own slot's uid is named as that slot's leftover | residue |
//! | `daemonised-child` | a process that was not there at the baseline and whose parent is `init` (pid 1), outside the containers of the agent's other jobs, and in a control group a job can be in (the agent's own group or below it, a group that names a container, or no unified path; see [What the host does while jobs run](#what-the-host-does-while-jobs-run)) | note |
//! | `new-mount` | a mount that was not there at the baseline (file system, mount point and source), other than those of the agent's other jobs' containers and those below the directory where the runtime mounts another held job's file system | note |
//! | `user-changed`, `group-changed` | a line added to, removed from or changed in `/etc/passwd` or `/etc/group`: the detail names `added <name>`, `removed <name>` or `changed <name>` | note |
//! | `kernel-module-changed` | a module in `/proc/modules` that was not there at the baseline (`loaded <name>`), or one that was and is gone (`unloaded <name>`) | note |
//! | `watch-dir-changed` | the names, sizes, modification or status change times, inodes, owners or modes of the entries of a `--watch-dir` differ | note |
//! | `processes-unreadable` | the process table could not be read or understood, so the check cannot run | unchecked |
//! | `mounts-unreadable`, `users-unreadable`, `groups-unreadable`, `modules-unreadable`, `watch-unreadable`, `watch-over-bound` | an input of a check that does not taint could not be read or understood (or a `--watch-dir` holds more than [`WATCH_ENTRY_CAP`] entries, so it cannot be digested); the check takes the baseline's value for it | note |
//!
//! A new process and a new mount count only when they are not the host's own (see
//! [What the host does while jobs run](#what-the-host-does-while-jobs-run)).
//!
//! A process of the job users' range counts only when it is still there after the settle
//! wait ([`Integrity::with_settle`], `--settle-ms`): the check looks again after the wait and
//! judges what of the first look is still there. A process that is ending is gone by then; a
//! job's leftover is not. The notes are taken from the host as the check last looked at it.
//! After a job's cleanup the lifecycle asks the check through [`CheckHook::poll`], which
//! does not wait: the first look answers the wait, the job waits in its cleanup while the
//! worker goes on with other work, and the lifecycle asks again once the wait has passed,
//! for the second look. The other checks ([`CheckHook::run`] and
//! [`CheckHook::run_restored`]: the re-check of a taint, a whole-node lease's checks, the
//! checks owed at start) wait on the caller's thread ([`HostView::settle`]).
//!
//! The finding names the process: `pid <pid> (<comm>, state <state>, cgroup <cgroup>, owner
//! <owner>, cmd "<command line>") is running after job <run>`, where the owner is `name
//! (uid)` when `/etc/passwd` has a line for that uid and the uid alone when it has not, and
//! the command line is read from `/proc/<pid>/cmdline` (its control characters as spaces,
//! cut to 80 bytes with `...`) and left out when it cannot be read or is empty. The
//! `daemonised-child` note names a process the same way.
//!
//! A check that cannot run is a finding, never a skipped check; the one that can taint is
//! the process table's. A finding's detail holds the processes it names, cut to
//! [`CHECK_DETAIL_MAX`](crate::lifecycle::CHECK_DETAIL_MAX) bytes. The notes are one per
//! code, with every name joined by `, `, in the order of the table after the notes of the inputs that could not be read, and are given to
//! [`CheckHook::notes`] after the check.
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
//! - **A process of another user** that started during the job: a host where operators log
//!   in and services restart while jobs run has such processes, and the agent does not call
//!   them the job's. One whose parent is `init` and that is in a group a job can be in is a
//!   `daemonised-child` note. A kernel thread (its parent is `kthreadd`, pid 2) is not a
//!   daemonised child.
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
//!   [`StateRoot::owe`](crate::lifecycle::StateRoot::owe)). An agent that starts and finds
//!   one runs the check itself, with the rules that need no baseline (what the job left
//!   behind may already be part of the new baseline): a process of the job users' range
//!   outside a live container taints the host (`check-owed`), and so does a process table
//!   that cannot be read; a clean host removes the records. Only processes are judged that
//!   way, as for every check. A job whose container was still running when the agent stopped is adopted
//!   and checked when it ends, but what it did before the stop is part of the baseline the
//!   new process took.
//! - **A mount's options**, remounts, and mounts that disappeared: only a new mount is
//!   noted, and two identical mounts count as one. Modules compare by name only.
//! - **What the host's own changes mean.** A mount, an account, a module, a daemonised
//!   process or a watched file that changed is logged and never taints: a contained job
//!   cannot cause any of them (see [What taints, and what does not](#what-taints-and-what-does-not)),
//!   so the agent cannot tell a host's change from a changed host and does not try. A host
//!   job taints `host-job` whatever the checks find.
//! - **What a job does inside its container** (a container can fill its own file system),
//!   and anything a process does without leaving the process table, the mounts, the
//!   databases, the modules or a watched directory behind. This is a post-job check, not an
//!   audit: eBPF and the audit subsystem are not used.
//!
//! # What the host does while jobs run
//!
//! A host starts processes and makes mounts of its own while jobs run, and none of them is
//! a job's: a job cannot place a process outside its container's control group, nor make a
//! mount on the host. They are not counted (a process is no `daemonised-child` note, and a
//! mount no `new-mount` note):
//!
//! - **A process with parent `init` outside every group a job can be in** is not a
//!   `daemonised-child`. A contained job cannot place a process in a control group other
//!   than its container's (the control group file system is read-only in the container, and
//!   it runs in a user namespace without capabilities), so a transient process of the host
//!   with parent `init` in the root group, `init.scope`, a scope of the system slice or
//!   another slice is not a job's. The groups a job can
//!   be in are the agent's own group and below it, a group that names a container (below), and
//!   a process with no unified path, which is judged conservatively. `process-as-job-user` is
//!   not narrowed this way: a uid of the job users' range is a job's wherever the process is.
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
//! - **The direct runner's layout.** A container is the group
//!   `<actions>/js-<agent tag>-<run id>/ctr` (the lease's group is named for the container,
//!   and the run handle is that name); the agent's own group is beside `actions/`, not above
//!   it, so a process left there would look like a system service's. A path with a group
//!   named `actions` in it names a container and counts, whatever is below; the runtime
//!   refuses to open over a leases' group of another name, so this name is the leases'. The runner's own
//!   processes (`crun run`, `kill`, `delete`, the keeper, and the supervisors) are children
//!   of the agent, alive under a parent that is not `init`, in the agent's group exactly:
//!   they are not counted. After a restart a supervisor's parent is `init`, but it was there
//!   at the new agent's baseline, so it is not new.
//!   The helper containers (`actions/helpers/...`) are not counted either: a background
//!   pull runs one while another job's checks run, and it is gone when its runtime call
//!   returns, so a helper that never ends is not found here. A live container is named by
//!   a whole path component, the handle or an engine's `libpod-<id>.scope` form of it, never
//!   by a part of a longer name (`run-3-1` and `run-3-12`).
//! - **A runtime that cannot see a container's group** is blind: see
//!   [`Runtime::places_in_cgroup`](crate::lifecycle::Runtime::places_in_cgroup); the
//!   lifecycle taints the host as unchecked when it sees such a run, still running, with no group for the settle wait (`--settle-ms`).
//! - **A network namespace Podman pins** (`nsfs` at `/run/netns/<name>`: one per container,
//!   a neighbour's included, named without the container id), **the bind of `/run/netns`
//!   onto itself** (`tmpfs`, root `/netns`) that the first container after a boot makes,
//!   **and a user's run directory** (`tmpfs` at `/run/user/<uid>`), made by a login.
//!
//! What is not on this list still counts as a note: Kubernetes pods, a container engine's
//! other mounts, and a service started in a scope. A host with such churn logs a line for
//! each; it is not tainted by them.
//!
//! # Jobs that run side by side
//!
//! The agent runs several jobs at once, so the host at the end of one job holds the
//! containers of the others. [`Finished::live`] lists their ids, a run whose create is still
//! under way off the worker among them (its container may already run); a process whose control
//! group path holds one of them, and a mount whose root, mount point or source does, are
//! not counted against the finished job. An engine that names its mounts or places its
//! helper processes without the container id does not match, and the neighbour then shows
//! as a note: Podman's overlay layers, and with Docker the overlay mount (named by its
//! layer id), so two jobs at once on a Docker or Podman host can log a `new-mount` note when
//! the first of them ends. The notes taint nothing. This is stated from how the engines work,
//! not from a measurement on a host.
//!
//! Under the direct runner every job runs as the one uid of the job users' range (there are
//! no slots to tell apart), so a process's uid does not say whose it is: its lease's group
//! does. A process of the job user in the group of a run in [`Finished::live`] is that run's,
//! and one outside every such group is a `process-as-job-user` of the finished job.
//!
//! The `crun` runtime mounts what a job's file system is made of (its layers, the clones of
//! its host paths) below a directory of the job's own, `<mounts>/<lease>`, before the job
//! has a container, and unmounts it after the container is gone.
//! [`Finished::held_mounts`] lists those directories of the agent's other jobs; a mount
//! whose point lies below one is not counted against the finished job. The finished job's
//! own are gone by its check, so one left there is a `new-mount` note.
//!
//! A container user that is the agent's own user (`--container-user` for an agent that
//! does not run as root) makes every process of the agent's user that is not in the
//! baseline, and not in the host's own control groups, a `process-as-job-user` finding: a
//! child of the agent that left its service's control group included.
//!
//! # macOS
//!
//! The checks read `/proc`, which a Mac does not have, and there is no backend for a Mac.
//! A container job is Linux-only for that reason: the agent refuses `--container-bin` on a
//! Mac when it starts, so a Mac agent runs no job and no check. This is stated, not
//! hidden: `js-agent` does not pretend that a Mac host was checked.

mod compare;
mod modules;
mod note;
mod own;
mod proc;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use crate::cli_runtime::{IdRange, JobUsers};
use crate::lifecycle::{CheckFinding, CheckHook, CheckNote, Checked, Finished};

use own::{is_live, under_held, unified};
pub use proc::{DirLister, Entry, Listing, PROCESS_CAP, ProcHost, SystemDirs, WATCH_ENTRY_CAP};
pub use view::{FakeHost, HostView, Mount, Process, Watched, input};

/// The codes of the findings and notes of the checks.
pub mod code {
    /// A process of the container user outlived the job.
    pub const PROCESS_AS_JOB_USER: &str = "process-as-job-user";
    /// A process whose parent is `init` is new on the host (a note).
    pub const DAEMONISED_CHILD: &str = "daemonised-child";
    /// A mount that was not there before (a note).
    pub const NEW_MOUNT: &str = "new-mount";
    /// The user database changed (a note).
    pub const USER_CHANGED: &str = "user-changed";
    /// The group database changed (a note).
    pub const GROUP_CHANGED: &str = "group-changed";
    /// A kernel module was loaded or unloaded (a note).
    pub const MODULE_CHANGED: &str = "kernel-module-changed";
    /// A watched directory changed (a note).
    pub const WATCH_DIR_CHANGED: &str = "watch-dir-changed";
    /// The process table could not be read (the unchecked finding).
    pub const PROCESSES_UNREADABLE: &str = "processes-unreadable";
    /// The mounts could not be read (a note).
    pub const MOUNTS_UNREADABLE: &str = "mounts-unreadable";
    /// The user database could not be read (a note).
    pub const USERS_UNREADABLE: &str = "users-unreadable";
    /// The group database could not be read (a note).
    pub const GROUPS_UNREADABLE: &str = "groups-unreadable";
    /// The kernel modules could not be read (a note).
    pub const MODULES_UNREADABLE: &str = "modules-unreadable";
    /// A watched directory could not be listed (a note).
    pub const WATCH_UNREADABLE: &str = "watch-unreadable";
    /// A watched directory holds more entries than the checks read (a note).
    pub const WATCH_OVER_BOUND: &str = "watch-over-bound";
}

/// An input of the checks that could not be read: the code of its `*-unreadable` finding (the
/// process table's) or note, and the reason.
struct Problem {
    code: &'static str,
    detail: String,
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
                || is_live(job, &mount.source)
                || under_held(job, &mount.point))
        });
        self
    }
}

/// The post-job checks: a [`HostView`], the user the containers run as, the watched
/// directories, the baseline, and the notes of the last check.
pub struct Integrity {
    view: Box<dyn HostView>,
    users: IdRange,
    /// The container uid of slot 0, when the containers run as the slots' users.
    first_slot_uid: Option<u32>,
    watch: Vec<PathBuf>,
    baseline: Snapshot,
    /// The agent's own control group (its unified path), which is never the host's.
    own: Option<String>,
    /// The agent's own process id, read from `/proc/self/stat` when the agent starts (none
    /// when it cannot be read). The agent is never a job's leftover: a re-check of a taint
    /// read from the marker judges processes the baseline holds, the agent's among them.
    agent_pid: Option<u32>,
    /// How long a process the checks would call a job's leftover must go on before it is one
    /// (`--settle-ms`); zero looks once.
    settle: Duration,
    /// What the last check saw change that does not taint the host, until
    /// [`CheckHook::notes`] drains it.
    notes: Vec<CheckNote>,
    /// The first look of a [`CheckHook::poll`] that waits, until the next call takes the
    /// second.
    pending: Option<compare::Again>,
}

impl Integrity {
    /// The checks for a host read through `view`, whose containers' processes are those of
    /// `users`, with the directories `watch` watched, and the baseline taken now.
    ///
    /// # Errors
    ///
    /// The host cannot be read whole (every input, the process table, the mounts, the users,
    /// the groups, the modules and each watched directory, must be readable now, though a
    /// later check may do without all but the process table), so there is nothing to compare
    /// with later, or the agent's own control group (`/proc/self/cgroup`) cannot be read: the
    /// text says which input failed. A mapped range that `view` shows is not acceptable is
    /// one too.
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
        let agent_pid = view.file("/proc/self/stat").ok().and_then(|stat| {
            stat.split_whitespace()
                .next()
                .and_then(|pid| pid.parse().ok())
        });
        let mut this = Self {
            view,
            users,
            first_slot_uid: None,
            watch,
            baseline: Snapshot::default(),
            own,
            agent_pid,
            settle: Duration::ZERO,
            notes: Vec::new(),
            pending: None,
        };
        let reading = this.observe();
        if !reading.unread.is_empty() {
            return Err(format!(
                "cannot take the baseline of the host: {}",
                details(&reading.unread)
            ));
        }
        this.baseline = reading.snapshot;
        Ok(this)
    }

    /// The same checks, which call a process of the job users' range that the baseline did
    /// not have a job's leftover (`process-as-job-user`) only when it is still there `wait`
    /// after they first saw it: the checks look again then, and keep of the first look what
    /// is still there. A host's own process that lives for a moment (the service manager's
    /// fork of a timer's unit, a process that is ending) is gone by then; a job's leftover is
    /// not. With no wait (what [`Integrity::start`] gives) they look once.
    #[must_use]
    pub fn with_settle(self, wait: Duration) -> Self {
        Self {
            settle: wait,
            ..self
        }
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
}

/// The details of `problems`, one after the other, each with its code.
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
    /// host's. A host whose process table cannot be read keeps the baseline it had; the check
    /// after the job finds it unreadable. An input of the others that cannot be read keeps
    /// its value in the baseline.
    fn idle(&mut self) {
        if let Ok((now, _)) = self.observe().seen() {
            self.baseline = now;
        }
    }

    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        self.judge(job, false)
    }

    fn run_restored(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        self.judge(job, true)
    }

    /// The check of [`CheckHook::run`] without the wait on the caller's thread: a first
    /// look that sees a process that would be a leftover answers [`Checked::Wait`] with the
    /// settle wait, and the next call for the same run looks again and judges.
    fn poll(&mut self, job: &Finished<'_>) -> Checked {
        self.poll_after(job)
    }

    /// The baseline-free rule of [`CheckHook::run_restored`] judges every process of the job
    /// users' range outside a live container, whatever the baseline holds.
    fn checks_owed_at_start(&self) -> bool {
        true
    }

    fn notes(&mut self) -> Vec<CheckNote> {
        std::mem::take(&mut self.notes)
    }
}
