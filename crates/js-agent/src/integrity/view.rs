//! What the post-job checks read of the host, and an in-memory host a test changes.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// A process, as the process table shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// The process id.
    pub pid: u32,
    /// The parent's process id; 1 for a process that was adopted by `init`.
    pub ppid: u32,
    /// The real user id.
    pub uid: u32,
    /// When the process started, in clock ticks since boot: with the pid it names one
    /// process for as long as the host runs, since a pid is reused.
    pub start: u64,
    /// The control groups of the process, as one line of text. A container's processes are
    /// in a group whose path holds the container's id.
    pub cgroup: String,
}

impl Process {
    /// What names this process on this boot.
    pub fn key(&self) -> (u32, u64) {
        (self.pid, self.start)
    }
}

/// A mount, without the numbers the kernel gives it (which differ for every mount): what is
/// mounted where.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mount {
    /// The part of the file system that is mounted.
    pub root: String,
    /// Where, as the kernel prints it (a space is `\040`).
    pub point: String,
    /// The type of the file system.
    pub fstype: String,
    /// What it is mounted from.
    pub source: String,
}

/// What a watched directory looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watched {
    /// A digest of the names, sizes, modification and status change times, inodes, owners
    /// and modes of its entries.
    Digest(String),
    /// It holds more entries than the checks read, so there is no digest.
    TooMany,
}

/// The host as the post-job checks see it. Each input can fail on its own; a failure is
/// the text of the reason, and the check that needed it is a finding.
pub trait HostView {
    /// The process table.
    ///
    /// # Errors
    ///
    /// It cannot be read whole.
    fn processes(&self) -> Result<Vec<Process>, String>;

    /// The mounts of the agent's mount namespace.
    ///
    /// # Errors
    ///
    /// They cannot be read whole.
    fn mounts(&self) -> Result<Vec<Mount>, String>;

    /// The entries of the user database, as lines.
    ///
    /// # Errors
    ///
    /// It cannot be read.
    fn users(&self) -> Result<BTreeSet<String>, String>;

    /// The entries of the group database, as lines.
    ///
    /// # Errors
    ///
    /// It cannot be read.
    fn groups(&self) -> Result<BTreeSet<String>, String>;

    /// The names of the loaded kernel modules.
    ///
    /// # Errors
    ///
    /// They cannot be read.
    fn modules(&self) -> Result<BTreeSet<String>, String>;

    /// A digest of the directory `dir`, or the news that it holds too many entries.
    ///
    /// # Errors
    ///
    /// It cannot be listed.
    fn watched(&self, dir: &Path) -> Result<Watched, String>;

    /// The target of the symbolic link at `path` (the namespace links of `/proc`).
    ///
    /// # Errors
    ///
    /// It cannot be read.
    fn link(&self, path: &str) -> Result<String, String>;

    /// The text of the file at `path` (`/proc/2/stat` and `/proc/1/cgroup`, which tell a
    /// host's `/proc` from a container's).
    ///
    /// # Errors
    ///
    /// It cannot be read.
    fn file(&self, path: &str) -> Result<String, String>;
}

/// The inputs of a [`FakeHost`], for [`FakeHost::fail`].
pub mod input {
    /// The process table.
    pub const PROCESSES: &str = "processes";
    /// The mounts.
    pub const MOUNTS: &str = "mounts";
    /// The user database.
    pub const USERS: &str = "users";
    /// The group database.
    pub const GROUPS: &str = "groups";
    /// The kernel modules.
    pub const MODULES: &str = "modules";
    /// Every watched directory.
    pub const WATCH: &str = "watch";
}

#[derive(Default)]
struct FakeState {
    processes: Vec<Process>,
    mounts: Vec<Mount>,
    users: BTreeSet<String>,
    groups: BTreeSet<String>,
    modules: BTreeSet<String>,
    watched: BTreeMap<PathBuf, Watched>,
    links: BTreeMap<String, String>,
    files: BTreeMap<String, String>,
    failing: BTreeSet<&'static str>,
}

/// A host in memory: a test changes it between the baseline and the check, the way a job
/// would change a real one. Clones share one host.
#[derive(Clone, Default)]
pub struct FakeHost {
    state: Rc<RefCell<FakeState>>,
}

impl FakeHost {
    /// A quiet host: `init`, the agent, two mounts, `root`, a module, and nothing watched.
    pub fn quiet() -> Self {
        let host = Self::default();
        {
            let mut state = host.state.borrow_mut();
            state.processes = vec![
                Process {
                    pid: 1,
                    ppid: 0,
                    uid: 0,
                    start: 1,
                    cgroup: "0::/init.scope".to_owned(),
                },
                Process {
                    pid: 100,
                    ppid: 1,
                    uid: 1000,
                    start: 50,
                    cgroup: "0::/system.slice/agent.service".to_owned(),
                },
            ];
            state.mounts = vec![
                Mount {
                    root: "/".to_owned(),
                    point: "/".to_owned(),
                    fstype: "ext4".to_owned(),
                    source: "/dev/sda1".to_owned(),
                },
                Mount {
                    root: "/".to_owned(),
                    point: "/proc".to_owned(),
                    fstype: "proc".to_owned(),
                    source: "proc".to_owned(),
                },
            ];
            state.users = BTreeSet::from(["root:x:0:0:root:/root:/bin/sh".to_owned()]);
            state.groups = BTreeSet::from(["root:x:0:".to_owned()]);
            state.modules = BTreeSet::from(["ext4".to_owned()]);
            state.links = ["pid", "mnt"]
                .into_iter()
                .flat_map(|kind| ["self", "1"].map(|who| (namespace_path(who, kind), shared(kind))))
                .collect();
            state.files = BTreeMap::from([
                (
                    "/proc/2/stat".to_owned(),
                    "2 (kthreadd) S 0 0 0 0".to_owned(),
                ),
                ("/proc/1/cgroup".to_owned(), "0::/init.scope\n".to_owned()),
            ]);
        }
        host
    }

    /// The file at `path` reads as `text` from now on.
    pub fn set_file(&self, path: &str, text: &str) {
        self.state
            .borrow_mut()
            .files
            .insert(path.to_owned(), text.to_owned());
    }

    /// The file at `path` cannot be read from now on.
    pub fn remove_file(&self, path: &str) {
        self.state.borrow_mut().files.remove(path);
    }

    /// A process of the agent's own is in the namespace `target` from now on:
    /// `/proc/<who>/ns/<kind>` reads as `target`.
    pub fn move_to_namespace(&self, who: &str, kind: &str, target: &str) {
        self.state
            .borrow_mut()
            .links
            .insert(namespace_path(who, kind), target.to_owned());
    }

    /// The link at `path` cannot be read from now on.
    pub fn remove_link(&self, path: &str) {
        self.state.borrow_mut().links.remove(path);
    }

    /// A user line is removed from the user database.
    pub fn remove_user(&self, line: &str) {
        self.state.borrow_mut().users.remove(line);
    }

    /// A line is removed from the group database.
    pub fn remove_group(&self, line: &str) {
        self.state.borrow_mut().groups.remove(line);
    }

    /// A kernel module is unloaded.
    pub fn unload_module(&self, name: &str) {
        self.state.borrow_mut().modules.remove(name);
    }

    /// A process starts.
    pub fn start_process(&self, process: Process) {
        self.state.borrow_mut().processes.push(process);
    }

    /// The process `pid` ends.
    pub fn end_process(&self, pid: u32) {
        self.state.borrow_mut().processes.retain(|p| p.pid != pid);
    }

    /// A file system is mounted.
    pub fn mount(&self, mount: Mount) {
        self.state.borrow_mut().mounts.push(mount);
    }

    /// The file system mounted at `point` is unmounted.
    pub fn unmount(&self, point: &str) {
        self.state.borrow_mut().mounts.retain(|m| m.point != point);
    }

    /// A line is added to the user database.
    pub fn add_user(&self, line: &str) {
        self.state.borrow_mut().users.insert(line.to_owned());
    }

    /// A line is added to the group database.
    pub fn add_group(&self, line: &str) {
        self.state.borrow_mut().groups.insert(line.to_owned());
    }

    /// A kernel module is loaded.
    pub fn load_module(&self, name: &str) {
        self.state.borrow_mut().modules.insert(name.to_owned());
    }

    /// The directory `dir` looks like `watched` from now on.
    pub fn watch(&self, dir: &Path, watched: Watched) {
        self.state
            .borrow_mut()
            .watched
            .insert(dir.to_path_buf(), watched);
    }

    /// The input `which` (one of [`input`]) cannot be read from now on.
    pub fn fail(&self, which: &'static str) {
        self.state.borrow_mut().failing.insert(which);
    }

    /// The input `which` can be read again.
    pub fn heal(&self, which: &'static str) {
        self.state.borrow_mut().failing.remove(which);
    }

    /// Whether the input `which` works: the reason it does not, if a test broke it.
    fn works(&self, which: &'static str) -> Result<(), String> {
        match self.state.borrow().failing.contains(which) {
            true => Err(format!("{which}: cannot be read")),
            false => Ok(()),
        }
    }
}

impl HostView for FakeHost {
    fn processes(&self) -> Result<Vec<Process>, String> {
        self.works(input::PROCESSES)
            .map(|()| self.state.borrow().processes.clone())
    }

    fn mounts(&self) -> Result<Vec<Mount>, String> {
        self.works(input::MOUNTS)
            .map(|()| self.state.borrow().mounts.clone())
    }

    fn users(&self) -> Result<BTreeSet<String>, String> {
        self.works(input::USERS)
            .map(|()| self.state.borrow().users.clone())
    }

    fn groups(&self) -> Result<BTreeSet<String>, String> {
        self.works(input::GROUPS)
            .map(|()| self.state.borrow().groups.clone())
    }

    fn modules(&self) -> Result<BTreeSet<String>, String> {
        self.works(input::MODULES)
            .map(|()| self.state.borrow().modules.clone())
    }

    fn watched(&self, dir: &Path) -> Result<Watched, String> {
        self.works(input::WATCH).and_then(|()| {
            let watched = self.state.borrow().watched.get(dir).cloned();
            watched.ok_or_else(|| format!("{}: no such directory", dir.display()))
        })
    }

    fn link(&self, path: &str) -> Result<String, String> {
        let target = self.state.borrow().links.get(path).cloned();
        target.ok_or_else(|| format!("{path}: No such file or directory"))
    }

    fn file(&self, path: &str) -> Result<String, String> {
        let text = self.state.borrow().files.get(path).cloned();
        text.ok_or_else(|| format!("{path}: No such file or directory"))
    }
}

/// The path of the namespace link of kind `kind` of `who` (`self` or a pid).
fn namespace_path(who: &str, kind: &str) -> String {
    format!("/proc/{who}/ns/{kind}")
}

/// The namespace a quiet host's agent shares with `init`.
fn shared(kind: &str) -> String {
    format!("{kind}:[4026531836]")
}
