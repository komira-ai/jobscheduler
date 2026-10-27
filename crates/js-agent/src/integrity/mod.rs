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
//! finds nothing takes the baseline again, so it is always the host as it was after the
//! last job that left nothing; a check that finds something leaves it alone (the host is
//! tainted for good, and every later job is compared with the last clean state).
//!
//! # What is checked
//!
//! | code | what | kind |
//! |---|---|---|
//! | `process-as-job-user` | a process that was not there at the baseline, owned by the user the containers run as, outside the containers of the agent's other jobs | residue |
//! | `daemonised-child` | a process that was not there at the baseline and whose parent is `init` (pid 1), outside the containers of the agent's other jobs: what a child that outlived its parent looks like | residue |
//! | `new-mount` | a mount that was not there at the baseline (file system, mount point and source), other than those of the agent's other jobs' containers | residue |
//! | `user-changed`, `group-changed` | a line added to, removed from or changed in `/etc/passwd` or `/etc/group` | residue |
//! | `kernel-module-changed` | a module in `/proc/modules` that was not there at the baseline, or one that was and is gone (an unload is a finding too: a job holds no capability to change modules, and a host where an operator unloads modules while jobs run cannot be called clean) | residue |
//! | `watch-dir-changed` | the names, sizes, modification or status change times, inodes, owners or modes of the entries of a `--watch-dir` differ | residue |
//! | `processes-unreadable`, `mounts-unreadable`, `users-unreadable`, `groups-unreadable`, `modules-unreadable`, `watch-unreadable` | an input could not be read or understood | unchecked |
//! | `watch-over-bound` | a `--watch-dir` holds more than [`WATCH_ENTRY_CAP`] entries, so it cannot be digested | unchecked |
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
//! either. `--container-host` takes only a socket on this machine, for the same reason. An
//! engine with user-namespace remapping (`userns-remap`) is not supported: the container
//! user shows as another uid on the host, and `process-as-job-user` would miss it.
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
//! baseline a finding, children of the agent included.
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

/// Checks that the agent sees the host's processes and mounts: it must be in the pid and
/// mount namespaces of `init`. `link` reads a symbolic link (the namespace links of `/proc`).
/// An agent in a namespace of its own (in a container, or under a service manager's
/// `PrivatePIDs`, `PrivateMounts` or `ProtectSystem`) cannot see its jobs' processes or the
/// overlay mounts of their containers, and every check would pass. Reading `/proc/1/ns`
/// takes the privilege to trace `init` (root, or `CAP_SYS_PTRACE`): an agent without it
/// cannot tell and does not start.
///
/// # Errors
///
/// The text says which namespace differs, or which link could not be read.
pub fn require_host_namespaces(
    link: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    for kind in ["pid", "mnt"] {
        let read = |who: &str| {
            link(&format!("/proc/{who}/ns/{kind}"))
                .map_err(|why| format!("cannot tell whether the agent sees the host: {why}"))
        };
        let (own, init) = (read("self")?, read("1")?);
        if own != init {
            return Err(format!(
                "the agent is in a {kind} namespace of its own ({own}; init is in {init}): it cannot see the processes and mounts of its jobs"
            ));
        }
    }
    Ok(())
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

/// The post-job checks: a [`HostView`], the user the containers run as, the watched
/// directories, and the baseline.
pub struct Integrity {
    view: Box<dyn HostView>,
    uid: u32,
    watch: Vec<PathBuf>,
    baseline: Snapshot,
}

impl Integrity {
    /// The checks for a host read through `view`, whose containers run as the user `uid`,
    /// with the directories `watch` watched, and the baseline taken now.
    ///
    /// # Errors
    ///
    /// The host cannot be read whole, so there is nothing to compare with later: the text
    /// says which input failed.
    pub fn start(view: Box<dyn HostView>, uid: u32, watch: Vec<PathBuf>) -> Result<Self, String> {
        require_host_namespaces(&|path| view.link(path))?;
        let mut this = Self {
            view,
            uid,
            watch,
            baseline: Snapshot::default(),
        };
        this.baseline = this.observe().map_err(|problems| {
            format!(
                "cannot take the baseline of the host: {}",
                details(&problems)
            )
        })?;
        Ok(this)
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
            .filter(|process| !known.contains(&process.key()) && !mine(&process.cgroup))
            .collect();
        let as_job = strays.iter().filter(|stray| stray.uid == self.uid);
        let daemonised = strays.iter().filter(|stray| stray.ppid == 1);
        let new_mounts = now
            .mounts
            .difference(&self.baseline.mounts)
            .filter(|mount| !(mine(&mount.root) || mine(&mount.point) || mine(&mount.source)));
        let moved = now
            .watched
            .iter()
            .filter(|(dir, digest)| self.baseline.watched.get(*dir) != Some(*digest));
        [
            group(
                code::PROCESS_AS_JOB_USER,
                as_job.map(|stray| {
                    format!(
                        "pid {} of user {} is running after job {}",
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
