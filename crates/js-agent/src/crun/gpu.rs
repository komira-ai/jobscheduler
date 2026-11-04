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
//! - each `--gpu-mount SRC:DST`, bound read-only, `nosuid` and `nodev`, after its other
//!   mounts;
//! - each `--gpu-env NAME=VALUE`, set over the image's variable of that name.
//!
//! A run that booked no GPU is given none of it, so its bundle is the one an agent without
//! the flags writes. Which indices a run may book is the lifecycle's: an index the host did
//! not report is refused `limits-exceed-shape`, and one another run holds `no-capacity`,
//! before the runtime is asked.

use std::collections::BTreeSet;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::Path;

use js_contain::container::{ContainerSpec, DeviceNode, HostMount};
use rustix::fs::{FileType, Mode, OFlags, ResolveFlags};

use crate::Cli;
use crate::cli_runtime::SCRATCH_TARGET;

/// What a `--gpu-device` path that is one GPU's node holds where the GPU's index goes.
pub const INDEX: &str = "{index}";

/// Why a runtime given no device list refuses a run that booked a GPU.
pub const NO_GPUS: &str = "containers on this runtime cannot use a GPU";

/// The trees of the kernel's own file systems, which no mount comes from or goes to.
const KERNEL_TREES: [&str; 3] = ["/proc", "/sys", "/dev"];

/// The prefix of the environment names that are the agent's own.
const RESERVED_PREFIX: &str = "JS_";

/// Reads the character device at a path: its numbers, owner and mode, as the host's `stat`
/// gives them, with the path it was asked for. Refuses anything that is not one.
pub type ReadNode = fn(&Path) -> Result<DeviceNode, String>;

/// The host's [`ReadNode`]: the node at `path` itself (a link is not followed, and is
/// refused).
///
/// # Errors
/// The path cannot be looked at, or is not a character device.
pub fn read_node(path: &Path) -> Result<DeviceNode, String> {
    let shown = path.display();
    let meta = std::fs::symlink_metadata(path).map_err(|why| format!("{shown}: {why}"))?;
    if !meta.file_type().is_char_device() {
        return Err(format!("{shown} is not a character device"));
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

/// One `--gpu-device`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DevicePath {
    /// A node every run that booked a GPU is given.
    Common(String),
    /// The node of one GPU: the path before and after [`INDEX`].
    PerGpu { head: String, tail: String },
}

impl DevicePath {
    /// The path `text`, checked: absolute and below `/dev` with no empty, `.` or `..` part,
    /// [`INDEX`] at most once and no other brace.
    fn parse(text: &str) -> Result<Self, String> {
        let refuse = |why: &str| Err(format!("--gpu-device: {text} {why}"));
        let normal = text
            .strip_prefix("/dev/")
            .is_some_and(|rest| rest.split('/').all(|p| !matches!(p, "" | "." | "..")));
        if !normal {
            return refuse("is not an absolute path below /dev without empty, `.` or `..` parts");
        }
        let parts: Vec<&str> = text.split(INDEX).collect();
        if parts.iter().any(|part| part.contains(['{', '}'])) {
            return refuse("has a brace that is not part of {index}");
        }
        match parts[..] {
            [whole] => Ok(Self::Common(whole.to_owned())),
            [head, tail] => Ok(Self::PerGpu {
                head: head.to_owned(),
                tail: tail.to_owned(),
            }),
            _ => refuse("names {index} more than once"),
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
    /// The text names the flag at fault: `--gpu-mount` or `--gpu-env` without
    /// `--gpu-device`, a host that offers no GPU, a value that is malformed, a node that
    /// is missing or is not a character device (for every reported index, for a per-GPU
    /// one), a mount whose source is missing, is not a directory or is below `/proc`, `/sys`
    /// or `/dev`, or whose destination is below those or `/scratch` or is another's, and a
    /// name that is malformed, reserved or given twice.
    pub fn from_cli(cli: &Cli, reported: &[u32], read: ReadNode) -> Result<Option<Self>, String> {
        if cli.gpu_devices.is_empty() {
            return match cli.gpu_flags().into_iter().find(|(_, given)| *given) {
                Some((flag, _)) => Err(format!("{flag}: needs --gpu-device")),
                None => Ok(None),
            };
        }
        if reported.is_empty() {
            return Err("--gpu-device: the host offers no GPU to give a job".to_owned());
        }
        let mut devices = Vec::new();
        for text in &cli.gpu_devices {
            let device = DevicePath::parse(text)?;
            if devices.contains(&device) {
                return Err(format!("--gpu-device: {text} is given twice"));
            }
            for path in device.paths(reported) {
                read(Path::new(&path)).map_err(|why| format!("--gpu-device: {why}"))?;
            }
            devices.push(device);
        }
        let mut targets = BTreeSet::new();
        let mut mounts = Vec::new();
        for text in &cli.gpu_mounts {
            let mount = mount(text)?;
            if !targets.insert(mount.target.clone()) {
                return Err(format!(
                    "--gpu-mount: {text}: another mount has the destination {}",
                    mount.target
                ));
            }
            mounts.push(mount);
        }
        let mut env: Vec<(String, String)> = Vec::new();
        for text in &cli.gpu_env {
            let (name, value) = variable(text)?;
            if env.iter().any(|(given, _)| *given == name) {
                return Err(format!("--gpu-env: {name} is given twice"));
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

/// One `--gpu-mount SRC:DST`, checked. The source is bound by the path it resolves to.
fn mount(text: &str) -> Result<HostMount, String> {
    let refuse = |why: String| Err(format!("--gpu-mount: {text}: {why}"));
    let Some((source, target)) = text.split_once(':') else {
        return refuse("is not SRC:DST".to_owned());
    };
    if !Path::new(source).is_absolute() {
        return refuse(format!("the source {source} is not an absolute path"));
    }
    let resolved = match std::fs::canonicalize(source) {
        Ok(resolved) => resolved,
        Err(why) => return refuse(format!("the source {source}: {why}")),
    };
    let shown = resolved.to_string_lossy().into_owned();
    if let Some(tree) = KERNEL_TREES.iter().find(|tree| below(&shown, tree)) {
        return refuse(format!("the source {shown} is below {tree}"));
    }
    if !resolved.is_dir() {
        return refuse(format!(
            "the source {shown} is not a directory (a file bound where the image has none \
             would be made in the image's root, which every job of the image shares)"
        ));
    }
    let normal = target
        .strip_prefix('/')
        .is_some_and(|rest| rest.split('/').all(|p| !matches!(p, "" | "." | "..")));
    if !normal {
        return refuse(format!(
            "the destination {target} is not an absolute path without empty, `.` or `..` parts"
        ));
    }
    let off_limits = KERNEL_TREES.iter().chain([&SCRATCH_TARGET]);
    if let Some(tree) = off_limits.into_iter().find(|tree| below(target, tree)) {
        return refuse(format!("the destination {target} is below {tree}"));
    }
    Ok(HostMount {
        target: target.to_owned(),
        source: resolved,
        read_only: true,
    })
}

/// One `--gpu-env NAME=VALUE`, checked.
fn variable(text: &str) -> Result<(String, String), String> {
    let Some((name, value)) = text.split_once('=') else {
        return Err(format!("--gpu-env: {text} is not NAME=VALUE"));
    };
    let mut chars = name.chars();
    let first = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_uppercase());
    let rest = chars.all(|c| c == '_' || c.is_ascii_uppercase() || c.is_ascii_digit());
    if !(first && rest) {
        return Err(format!(
            "--gpu-env: {name:?} is not a name of [A-Z_][A-Z0-9_]*"
        ));
    }
    if name.starts_with(RESERVED_PREFIX) {
        return Err(format!(
            "--gpu-env: {name} is reserved: names that start with {RESERVED_PREFIX} are the agent's"
        ));
    }
    Ok((name.to_owned(), value.to_owned()))
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

/// What one run is given: nothing, for a run that booked no GPU.
#[derive(Debug, Default)]
pub(super) struct Wiring {
    devices: Vec<DeviceNode>,
    mounts: Vec<HostMount>,
    env: Vec<(String, String)>,
}

/// What a run that booked `indices` is given by a runtime with `gpus` (none without).
///
/// # Errors
/// The run booked a GPU and the runtime has no device list ([`NO_GPUS`]), or a node cannot
/// be read now; the text names the node.
pub(super) fn wiring(gpus: Option<&GpuSettings>, indices: &[u32]) -> Result<Wiring, String> {
    if indices.is_empty() {
        return Ok(Wiring::default());
    }
    let gpus = gpus.ok_or(NO_GPUS)?;
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
        .map(|path| (gpus.read)(Path::new(path)).map_err(|why| format!("the GPU device {why}")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Wiring {
        devices,
        mounts: gpus.passthrough.mounts.clone(),
        env: gpus.passthrough.env.clone(),
    })
}

impl Wiring {
    /// Refuses a driver mount whose destination is something other than a directory in the
    /// image's root `rootfs` (a file such as a library): `crun` could not bind a directory
    /// over it, and the start would fail later in the container. The destination is looked up
    /// as the container sees it (links resolved inside the root, never on the host); one the
    /// image does not have is fine (`crun` makes it).
    ///
    /// # Errors
    /// The text names `--gpu-mount` and the destination.
    pub(super) fn check_targets(&self, rootfs: &Path) -> Result<(), String> {
        for mount in &self.mounts {
            if let Ok(kind) = kind_in(rootfs, &mount.target)
                && kind != FileType::Directory
            {
                return Err(format!(
                    "--gpu-mount: the destination {} is not a directory in the image, so the \
                     driver directory cannot be bound over it",
                    mount.target
                ));
            }
        }
        Ok(())
    }

    /// Gives `container` (whose image config is applied) the nodes, the mounts after its
    /// own, and the variables, each over the image's of the same name.
    pub(super) fn apply(self, container: &mut ContainerSpec) {
        container.devices = self.devices;
        container.mounts.extend(self.mounts);
        for (name, value) in self.env {
            match container.env.iter_mut().find(|(given, _)| *given == name) {
                Some(slot) => slot.1 = value,
                None => container.env.push((name, value)),
            }
        }
    }
}

/// The type of what `target` (an absolute path in the container) is in the root `rootfs`,
/// resolved as if `rootfs` were `/` (`RESOLVE_IN_ROOT`: a link cannot leave it).
fn kind_in(rootfs: &Path, target: &str) -> std::io::Result<FileType> {
    let flags = OFlags::PATH | OFlags::CLOEXEC;
    let root = rustix::fs::open(rootfs, flags | OFlags::DIRECTORY, Mode::empty())?;
    let relative = target.trim_start_matches('/');
    let found = rustix::fs::openat2(&root, relative, flags, Mode::empty(), ResolveFlags::IN_ROOT)?;
    Ok(FileType::from_raw_mode(rustix::fs::fstat(&found)?.st_mode))
}

#[cfg(test)]
#[path = "gpu_tests.rs"]
mod tests;
