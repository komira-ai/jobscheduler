//! The GPUs of the crun runtime: what the operator lists by flag (`--gpu-device`,
//! `--gpu-mount`, `--gpu-env`), checked once at start ([`GpuPassthrough::from_cli`]), and what
//! a run that booked GPUs is given when it starts.
//!
//! Nothing is discovered: the agent reads no CDI spec, runs no vendor hook (the container
//! runs as the job user, and such a hook needs root) and never guesses a driver's path. A run
//! that booked the GPU indices `[i, j, ...]` is given:
//!
//! - each `--gpu-device` without `{index}` (`/dev/nvidiactl`), and each one with it
//!   (`/dev/nvidia{index}`) for exactly `i, j, ...`: character devices at their own paths,
//!   with the type, numbers, owner and mode their node has when the run starts
//!   ([`read_node`]), and a device cgroup that allows those nodes and denies every other
//!   device (js-contain's `devices`);
//! - each `--gpu-mount SRC:DST`: the directory bound read-only, `nosuid` and `nodev`, not
//!   recursively (no mount below the directory on the host reaches the job), after its other
//!   mounts;
//! - each `--gpu-env NAME=VALUE`, set over the image's variable of that name (one entry for
//!   the name, however many the image has).
//!
//! A mount's destination is a place the runtime does not mount itself
//! ([`DESTINATION_OFF_LIMITS`]) and no other mount's, so what the job sees below it is the
//! image's root. When a run starts, the destination is walked in the image's root without
//! following a link: every part the image has must be a directory (a link, which the bind
//! would follow somewhere else in the container, a file and a part that cannot be looked up
//! are refused, naming `--gpu-mount`), and the parts it lacks are made by `crun`. Each driver
//! directory must still be a directory then.
//!
//! A run that booked no GPU is given none of it, so its bundle is the one an agent without
//! the flags writes. Which indices a run may book is the lifecycle's: an index the host did
//! not report is refused `limits-exceed-shape`, and one another run holds `no-capacity`,
//! before the runtime is asked.

use std::collections::BTreeSet;
use std::io;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};

use js_contain::container::{ContainerSpec, DeviceNode, HostMount};
use rustix::fs::{FileType, Mode, OFlags};

use super::spec::EXEC_ROOT;
use crate::Cli;
use crate::cli_runtime::SCRATCH_TARGET;

/// What a `--gpu-device` path that is one GPU's node holds where the GPU's index goes.
pub const INDEX: &str = "{index}";

/// Why a runtime given no device list refuses a run that booked a GPU.
pub const NO_GPUS: &str = "containers on this runtime cannot use a GPU";

/// The trees of the kernel's own file systems, which no mount comes from.
const KERNEL_TREES: [&str; 3] = ["/proc", "/sys", "/dev"];

/// The places no `--gpu-mount` goes to or below: the kernel's trees, the job's scratch, and
/// what the runtime mounts itself over the image (the job's overlay, its home, its tmpfs
/// mounts, which hold the run's files, and its name files). Below any of them the image is
/// not what the job sees, and a driver directory there would hide what the job needs.
pub const DESTINATION_OFF_LIMITS: [&str; 12] = [
    "/proc",
    "/sys",
    "/dev",
    SCRATCH_TARGET,
    EXEC_ROOT,
    "/root",
    "/tmp",
    "/var/tmp",
    "/run",
    "/etc/hosts",
    "/etc/hostname",
    "/etc/resolv.conf",
];

/// The prefix of the environment names that are the agent's own.
const RESERVED_PREFIX: &str = "JS_";

/// Why a device node cannot be given to a job.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The path cannot be looked at (it is not there, most often).
    #[error("{}: {error}", path.display())]
    Unreadable {
        /// The node's path.
        path: PathBuf,
        /// The system's reason.
        #[source]
        error: io::Error,
    },
    /// Something other than a character device is there.
    #[error("{} is not a character device", path.display())]
    NotCharacterDevice {
        /// The node's path.
        path: PathBuf,
    },
}

/// Reads the character device at a path: its numbers, owner and mode, as the host's `stat`
/// gives them, with the path it was asked for. Refuses anything that is not one.
pub type ReadNode = fn(&Path) -> Result<DeviceNode, NodeError>;

/// The host's [`ReadNode`]: the node at `path` itself (a link is not followed, and is
/// refused).
///
/// # Errors
/// The path cannot be looked at, or is not a character device.
pub fn read_node(path: &Path) -> Result<DeviceNode, NodeError> {
    let unreadable = |error| NodeError::Unreadable {
        path: path.to_owned(),
        error,
    };
    let meta = std::fs::symlink_metadata(path).map_err(unreadable)?;
    if !meta.file_type().is_char_device() {
        return Err(NodeError::NotCharacterDevice {
            path: path.to_owned(),
        });
    }
    Ok(DeviceNode {
        path: path.to_string_lossy().into_owned(),
        major: rustix::fs::major(meta.rdev()),
        minor: rustix::fs::minor(meta.rdev()),
        uid: meta.uid(),
        gid: meta.gid(),
        mode: meta.mode() & 0o7777,
    })
}

/// What is wrong with a `--gpu-device` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DeviceProblem {
    /// Not absolute, not below `/dev`, or with an empty, `.` or `..` part.
    #[error("is not an absolute path below /dev without empty, `.` or `..` parts")]
    NotBelowDev,
    /// `{index}` more than once.
    #[error("names {{index}} more than once")]
    IndexTwice,
    /// A brace that is not part of `{index}`.
    #[error("has a brace that is not part of {{index}}")]
    StrayBrace,
    /// The same path twice.
    #[error("is given twice")]
    Twice,
}

/// What is wrong with a `--gpu-mount SRC:DST`.
#[derive(Debug, thiserror::Error)]
pub enum MountProblem {
    /// No `:`.
    #[error("is not SRC:DST")]
    NotSourceAndTarget,
    /// The source is not an absolute path.
    #[error("the source {path} is not an absolute path")]
    SourceRelative {
        /// The source as given.
        path: String,
    },
    /// The source cannot be resolved (it is not there, most often).
    #[error("the source {path}: {error}")]
    SourceUnreadable {
        /// The source as given.
        path: String,
        /// The system's reason.
        #[source]
        error: io::Error,
    },
    /// The source resolves into a kernel tree.
    #[error("the source {path} is below {tree}")]
    SourceBelow {
        /// The source as it resolves.
        path: String,
        /// The tree.
        tree: &'static str,
    },
    /// The source is a file, or anything else that is not a directory.
    #[error(
        "the source {path} is not a directory (a file bound where the image has none would \
         be made in the image's root, which every job of the image shares)"
    )]
    SourceNotDirectory {
        /// The source as it resolves.
        path: String,
    },
    /// The destination is not an absolute, normalized path.
    #[error("the destination {target} is not an absolute path without empty, `.` or `..` parts")]
    TargetNotNormal {
        /// The destination.
        target: String,
    },
    /// The destination is at or below one of [`DESTINATION_OFF_LIMITS`].
    #[error("the destination {target} is below {tree}")]
    TargetBelow {
        /// The destination.
        target: String,
        /// The place it is at or below.
        tree: &'static str,
    },
    /// The destination holds one of [`DESTINATION_OFF_LIMITS`] (it is `/etc` or `/var`, say):
    /// the driver directory would hide what the runtime mounts there.
    #[error("the destination {target} holds {tree}, which the driver directory would hide")]
    TargetAbove {
        /// The destination.
        target: String,
        /// The place below it.
        tree: &'static str,
    },
    /// The destination is another mount's, or one of the two is below the other.
    #[error("the destination {target} overlaps {other}, another mount's")]
    TargetOverlaps {
        /// The destination.
        target: String,
        /// The other mount's destination.
        other: String,
    },
}

/// What is wrong with a `--gpu-env NAME=VALUE`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvProblem {
    /// No `=`.
    #[error("{text} is not NAME=VALUE")]
    NotNameAndValue {
        /// The value as given.
        text: String,
    },
    /// The name is not `[A-Z_][A-Z0-9_]*`.
    #[error("{name:?} is not a name of [A-Z_][A-Z0-9_]*")]
    BadName {
        /// The name.
        name: String,
    },
    /// The name starts with the agent's `JS_`.
    #[error("{name} is reserved: names that start with JS_ are the agent's")]
    Reserved {
        /// The name.
        name: String,
    },
    /// The name is given twice.
    #[error("{name} is given twice")]
    Twice {
        /// The name.
        name: String,
    },
}

/// Why the GPU flags cannot be used. Each text names the flag; the agent exits 2 with it.
#[derive(Debug, thiserror::Error)]
pub enum GpuFlagError {
    /// `--gpu-mount` or `--gpu-env` without `--gpu-device`.
    #[error("{flag}: needs --gpu-device")]
    NeedsDevice {
        /// The flag.
        flag: &'static str,
    },
    /// The host offers no GPU.
    #[error("--gpu-device: the host offers no GPU to give a job")]
    NoGpu,
    /// A `--gpu-device` path is malformed or given twice.
    #[error("--gpu-device: {path} {problem}")]
    DevicePath {
        /// The path as given.
        path: String,
        /// What is wrong with it.
        problem: DeviceProblem,
    },
    /// A node is missing or is not a character device.
    #[error("--gpu-device: {0}")]
    Node(#[source] NodeError),
    /// A `--gpu-mount` cannot be used.
    #[error("--gpu-mount: {value}: {problem}")]
    Mount {
        /// The value as given.
        value: String,
        /// What is wrong with it.
        #[source]
        problem: MountProblem,
    },
    /// A `--gpu-env` cannot be used.
    #[error("--gpu-env: {0}")]
    Env(#[source] EnvProblem),
}

/// One `--gpu-device`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DevicePath {
    /// A node every run that booked a GPU is given.
    Common(String),
    /// The node of one GPU: the path before and after [`INDEX`].
    PerGpu { head: String, tail: String },
}

/// Whether `path` is an absolute path below `under` (which ends with `/`, or is `/`) whose
/// parts are not empty, `.` or `..`.
fn normal_below(path: &str, under: &str) -> bool {
    path.strip_prefix(under)
        .is_some_and(|rest| rest.split('/').all(|p| !matches!(p, "" | "." | "..")))
}

impl DevicePath {
    /// The path `text`, checked: absolute and below `/dev` with no empty, `.` or `..` part,
    /// [`INDEX`] at most once and no other brace.
    fn parse(text: &str) -> Result<Self, DeviceProblem> {
        if !normal_below(text, "/dev/") {
            return Err(DeviceProblem::NotBelowDev);
        }
        let parts: Vec<&str> = text.split(INDEX).collect();
        if parts.iter().any(|part| part.contains(['{', '}'])) {
            return Err(DeviceProblem::StrayBrace);
        }
        match parts[..] {
            [whole] => Ok(Self::Common(whole.to_owned())),
            [head, tail] => Ok(Self::PerGpu {
                head: head.to_owned(),
                tail: tail.to_owned(),
            }),
            _ => Err(DeviceProblem::IndexTwice),
        }
    }

    /// The paths of this device for a run that booked `indices`: the common node once, or
    /// one GPU's node for each index, in decimal.
    fn paths(&self, indices: &[u32]) -> Vec<String> {
        match self {
            Self::Common(path) => vec![path.clone()],
            Self::PerGpu { head, tail } => indices
                .iter()
                .map(|index| format!("{head}{index}{tail}"))
                .collect(),
        }
    }
}

/// The checked GPU flags of a crun agent: the devices, the driver's mounts and the
/// variables a run that booked GPUs is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuPassthrough {
    devices: Vec<DevicePath>,
    mounts: Vec<HostMount>,
    env: Vec<(String, String)>,
}

impl GpuPassthrough {
    /// The GPU flags of `cli`, checked against the host, whose GPUs have the indices
    /// `reported`; `read` reads a node. `None` without `--gpu-device`: the runtime then
    /// runs no GPU job.
    ///
    /// # Errors
    /// See [`GpuFlagError`]: each names the flag at fault.
    pub fn from_cli(
        cli: &Cli,
        reported: &[u32],
        read: ReadNode,
    ) -> Result<Option<Self>, GpuFlagError> {
        if cli.gpu_devices.is_empty() {
            return match cli.gpu_flags().into_iter().find(|(_, given)| *given) {
                Some((flag, _)) => Err(GpuFlagError::NeedsDevice { flag }),
                None => Ok(None),
            };
        }
        if reported.is_empty() {
            return Err(GpuFlagError::NoGpu);
        }
        let mut devices = Vec::new();
        for text in &cli.gpu_devices {
            let refuse = |problem| GpuFlagError::DevicePath {
                path: text.clone(),
                problem,
            };
            let device = DevicePath::parse(text).map_err(refuse)?;
            if devices.contains(&device) {
                return Err(refuse(DeviceProblem::Twice));
            }
            for path in device.paths(reported) {
                read(Path::new(&path)).map_err(GpuFlagError::Node)?;
            }
            devices.push(device);
        }
        let mut mounts: Vec<HostMount> = Vec::new();
        for text in &cli.gpu_mounts {
            let refuse = |problem| GpuFlagError::Mount {
                value: text.clone(),
                problem,
            };
            let mount = mount(text).map_err(refuse)?;
            let overlaps = |other: &&HostMount| {
                [
                    (&mount.target, &other.target),
                    (&other.target, &mount.target),
                ]
                .iter()
                .any(|(path, tree)| below(path, tree))
            };
            if let Some(other) = mounts.iter().find(overlaps) {
                return Err(refuse(MountProblem::TargetOverlaps {
                    target: mount.target.clone(),
                    other: other.target.clone(),
                }));
            }
            mounts.push(mount);
        }
        let mut names = BTreeSet::new();
        let mut env: Vec<(String, String)> = Vec::new();
        for text in &cli.gpu_env {
            let (name, value) = variable(text).map_err(GpuFlagError::Env)?;
            if !names.insert(name.clone()) {
                return Err(GpuFlagError::Env(EnvProblem::Twice { name }));
            }
            env.push((name, value));
        }
        Ok(Some(Self {
            devices,
            mounts,
            env,
        }))
    }
}

/// Whether `path` is `tree` or below it.
fn below(path: &str, tree: &str) -> bool {
    path.strip_prefix(tree)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// One `--gpu-mount SRC:DST`, checked. The source is bound by the path it resolves to, and
/// not recursively.
fn mount(text: &str) -> Result<HostMount, MountProblem> {
    let Some((source, target)) = text.split_once(':') else {
        return Err(MountProblem::NotSourceAndTarget);
    };
    if !Path::new(source).is_absolute() {
        return Err(MountProblem::SourceRelative {
            path: source.to_owned(),
        });
    }
    let resolved =
        std::fs::canonicalize(source).map_err(|error| MountProblem::SourceUnreadable {
            path: source.to_owned(),
            error,
        })?;
    let shown = resolved.to_string_lossy().into_owned();
    if let Some(tree) = KERNEL_TREES.into_iter().find(|tree| below(&shown, tree)) {
        return Err(MountProblem::SourceBelow { path: shown, tree });
    }
    if !resolved.is_dir() {
        return Err(MountProblem::SourceNotDirectory { path: shown });
    }
    if !normal_below(target, "/") {
        return Err(MountProblem::TargetNotNormal {
            target: target.to_owned(),
        });
    }
    if let Some(tree) = DESTINATION_OFF_LIMITS
        .into_iter()
        .find(|tree| below(target, tree))
    {
        return Err(MountProblem::TargetBelow {
            target: target.to_owned(),
            tree,
        });
    }
    if let Some(tree) = DESTINATION_OFF_LIMITS
        .into_iter()
        .find(|tree| below(tree, target))
    {
        return Err(MountProblem::TargetAbove {
            target: target.to_owned(),
            tree,
        });
    }
    Ok(HostMount {
        target: target.to_owned(),
        source: resolved,
        read_only: true,
        recursive: false,
    })
}

/// One `--gpu-env NAME=VALUE`, checked.
fn variable(text: &str) -> Result<(String, String), EnvProblem> {
    let Some((name, value)) = text.split_once('=') else {
        return Err(EnvProblem::NotNameAndValue {
            text: text.to_owned(),
        });
    };
    let mut chars = name.chars();
    let first = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_uppercase());
    let rest = chars.all(|c| c == '_' || c.is_ascii_uppercase() || c.is_ascii_digit());
    let name = name.to_owned();
    if !(first && rest) {
        return Err(EnvProblem::BadName { name });
    }
    if name.starts_with(RESERVED_PREFIX) {
        return Err(EnvProblem::Reserved { name });
    }
    Ok((name, value.to_owned()))
}

/// What a crun runtime gives the runs that book GPUs: the checked flags, and how a node is
/// read when a run starts.
#[derive(Debug, Clone)]
pub struct GpuSettings {
    /// The checked flags.
    pub passthrough: GpuPassthrough,
    /// How a node is read when a run starts: [`read_node`] on a machine.
    pub read: ReadNode,
}

/// What is wrong, in the image, with a driver mount's destination.
#[derive(Debug, thiserror::Error)]
pub(super) enum TargetProblem {
    /// A part is a link: the bind would follow it to another place in the container.
    #[error("the image has a link at {at}, which a driver mount does not follow")]
    Link { at: String },
    /// A part is a file, or anything else that is not a directory.
    #[error("the image has something other than a directory at {at}")]
    NotDirectory { at: String },
    /// A part cannot be looked up.
    #[error("{at} cannot be looked up in the image: {error}")]
    Lookup {
        at: String,
        #[source]
        error: io::Error,
    },
}

/// Why a run that booked GPUs cannot be given them when it starts.
#[derive(Debug, thiserror::Error)]
pub(super) enum StartError {
    /// The runtime has no device list.
    #[error("containers on this runtime cannot use a GPU")]
    NoDeviceList,
    /// A node is gone, or is something else now.
    #[error("the GPU device {0}")]
    Node(#[source] NodeError),
    /// A driver directory is gone, or is something else now.
    #[error("--gpu-mount: the source {} is not a directory now: {why}", path.display())]
    Source { path: PathBuf, why: String },
    /// A destination cannot be bound over in the run's image.
    #[error("--gpu-mount: the destination {target}: {problem}")]
    Target {
        target: String,
        #[source]
        problem: TargetProblem,
    },
}

/// What one run is given: nothing, for a run that booked no GPU.
#[derive(Debug, Default)]
pub(super) struct Wiring {
    devices: Vec<DeviceNode>,
    mounts: Vec<HostMount>,
    env: Vec<(String, String)>,
}

/// What a run that booked `indices` is given by a runtime with `gpus` (none without): its
/// nodes, read now, and the driver mounts, whose sources must still be directories.
///
/// # Errors
/// See [`StartError`].
pub(super) fn wiring(gpus: Option<&GpuSettings>, indices: &[u32]) -> Result<Wiring, StartError> {
    if indices.is_empty() {
        return Ok(Wiring::default());
    }
    let gpus = gpus.ok_or(StartError::NoDeviceList)?;
    let mut paths: Vec<String> = Vec::new();
    for path in gpus
        .passthrough
        .devices
        .iter()
        .flat_map(|device| device.paths(indices))
    {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    let devices = paths
        .iter()
        .map(|path| (gpus.read)(Path::new(path)).map_err(StartError::Node))
        .collect::<Result<Vec<_>, _>>()?;
    for mount in &gpus.passthrough.mounts {
        let found = std::fs::symlink_metadata(&mount.source).map(|meta| meta.is_dir());
        let why = match found {
            Ok(true) => continue,
            Ok(false) => "something else is there".to_owned(),
            Err(error) => error.to_string(),
        };
        return Err(StartError::Source {
            path: mount.source.clone(),
            why,
        });
    }
    Ok(Wiring {
        devices,
        mounts: gpus.passthrough.mounts.clone(),
        env: gpus.passthrough.env.clone(),
    })
}

impl Wiring {
    /// Refuses a driver mount whose destination the image's root `rootfs` has as something
    /// other than directories all the way: a link, a file or a part that cannot be looked
    /// up. Nothing is followed and nothing on the host is looked at; a part the image does
    /// not have ends the walk (`crun` makes the rest).
    ///
    /// # Errors
    /// [`StartError::Target`], naming the destination and the part.
    pub(super) fn check_targets(&self, rootfs: &Path) -> Result<(), StartError> {
        for mount in &self.mounts {
            walk_in(rootfs, &mount.target).map_err(|problem| StartError::Target {
                target: mount.target.clone(),
                problem,
            })?;
        }
        Ok(())
    }

    /// Gives `container` (whose image config is applied) the nodes, the mounts after its
    /// own, and the variables: each replaces the image's of the same name, and the image's
    /// other entries of that name are dropped.
    pub(super) fn apply(self, container: &mut ContainerSpec) {
        container.devices = self.devices;
        container.mounts.extend(self.mounts);
        for (name, value) in self.env {
            let mut set = false;
            container.env.retain_mut(|(given, held)| {
                let first = *given == name && !set;
                set |= first;
                if first {
                    held.clone_from(&value);
                }
                first || *given != name
            });
            if !set {
                container.env.push((name, value));
            }
        }
    }
}

/// Walks `target` (an absolute, normalized path) in the root `rootfs`, one part at a time,
/// never following a link: every part that is there must be a directory.
fn walk_in(rootfs: &Path, target: &str) -> Result<(), TargetProblem> {
    let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let lookup = |at: &str| {
        let at = at.to_owned();
        move |errno: rustix::io::Errno| TargetProblem::Lookup {
            at,
            error: errno.into(),
        }
    };
    let mut dir =
        rustix::fs::open(rootfs, flags | OFlags::DIRECTORY, Mode::empty()).map_err(lookup("/"))?;
    let mut at = String::new();
    for part in target.trim_start_matches('/').split('/') {
        at = format!("{at}/{part}");
        let found = match rustix::fs::openat(&dir, part, flags, Mode::empty()) {
            Ok(found) => found,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(errno) => return Err(lookup(&at)(errno)),
        };
        let stat = rustix::fs::fstat(&found).map_err(lookup(&at))?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => dir = found,
            FileType::Symlink => return Err(TargetProblem::Link { at }),
            _ => return Err(TargetProblem::NotDirectory { at }),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "gpu_tests.rs"]
mod tests;
