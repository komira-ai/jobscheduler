//! What the caller tells the runtime about one job's container: [`ContainerSpec`], the
//! [`ContainerLimits`] it gets, and how it [`Ended`]. The OCI backend ([`crate::oci`])
//! turns the spec into a bundle for `crun`.

use std::path::{Path, PathBuf};

/// The uid and gid a job runs as inside its container: a fixed unprivileged id, as an
/// job runs on a worker that does not give it root. Rootless, the namespace maps
/// container ids `0..65536` to the daemon user's subordinate range, so this id is
/// subordinate id 1000 of that range, and permission checks are real: a mode-0 file is
/// refused, a directory the job does not own cannot be written.
pub const ACTION_ID: u32 = 1000;

/// The owner, in the setup namespace ([`crate::oci::Mapping::Setup`]), of a lease's overlay
/// directories while its container may run: [`ACTION_ID`] there, which is
/// `ACTION_ID + 1` (the setup namespace maps the daemon user to 0 and the range from 1).
/// The container cannot write under a directory whose owner it does not map, or does not
/// run as, so without this the job could write nothing under the exec root (the
/// overlay refuses with `EACCES`).
pub const CONTAINER_OWNER: &str = "1001:1001";

/// The owner, in the same namespace, of the overlay directories once the container has
/// stopped: id 0 there is the daemon's own user, so the driver reads every output (an
/// job's `0600` file or `0700` directory among them) and removes the scratch itself.
pub const DAEMON_OWNER: &str = "0:0";

/// The per-container limits every job's container is created with. Each is written
/// into the bundle explicitly, so no default of the runtime changes what a job gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerLimits {
    /// The container's `pids.max`, its tasks (threads included).
    pub pids: u64,
    /// The size of the container's `/dev/shm` tmpfs, in MiB.
    pub shm_mib: u64,
    /// `RLIMIT_NOFILE`, soft and hard. Rootless, it cannot exceed the daemon's own hard
    /// limit.
    pub nofile: u64,
    /// `RLIMIT_NPROC`, soft and hard. A rootless daemon cannot set it above its own hard
    /// limit ([`own_nproc_hard_limit`]; [`ContainerLimits::within_nproc`]). The kernel
    /// counts a user namespace's processes against its creator's, and every container's
    /// namespace is made by the daemon user, so what bounds all the node's jobs together is
    /// the daemon's own limit; the `pids` of each lease is the per-job bound.
    pub nproc: u64,
    /// The size of the container's `/tmp` tmpfs, in MiB.
    pub tmp_mib: u64,
}

/// The `RLIMIT_NPROC` a job is given unless the caller says otherwise: the kernel's
/// largest pid (`PID_MAX_LIMIT` on a 64-bit machine), so that the lease's `pids.max` is
/// the only bound on a job's processes. A rootless daemon cannot raise a hard limit above
/// its own ([`own_nproc_hard_limit`]), so [`ContainerLimits::within_nproc`] cuts this to
/// what the daemon may give.
pub const NPROC_HIGH: u64 = 4_194_304;

impl ContainerLimits {
    /// 8192 pids, a 64 MiB `/dev/shm`, 65,536 open files, [`NPROC_HIGH`] processes and a
    /// 64 MiB `/tmp`.
    pub const DEFAULT: Self = Self {
        pids: 8192,
        shm_mib: 64,
        nofile: 65_536,
        nproc: NPROC_HIGH,
        tmp_mib: 64,
    };

    /// `self` with `nproc` no higher than `ceiling`.
    #[must_use]
    pub const fn within_nproc(self, ceiling: u64) -> Self {
        if self.nproc > ceiling {
            Self {
                nproc: ceiling,
                ..self
            }
        } else {
            self
        }
    }
}

/// The hard `RLIMIT_NPROC` of this process: the most a rootless daemon can give a job.
/// Raising a hard limit takes `CAP_SYS_RESOURCE` in the initial user namespace
/// (`setrlimit(2)`), which a rootless daemon does not have, so a container asking for more
/// fails to start in `crun`'s `setrlimit`. `u64::MAX` when there is no limit.
#[must_use]
pub fn own_nproc_hard_limit() -> u64 {
    rustix::process::getrlimit(rustix::process::Resource::Nproc)
        .maximum
        .unwrap_or(u64::MAX)
}

/// The container to run for one job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerSpec {
    /// The container's name, unique on the node (the lease's directory is named by it).
    pub name: String,
    /// `<repo>@sha256:<digest>`.
    pub image: String,
    /// `sha256:<hex>`: the image's digest alone.
    pub image_digest: String,
    /// The lease's scratch directory.
    pub dir: PathBuf,
    /// The lease cgroup, relative to the cgroup mount.
    pub cgroup_parent: String,
    /// Where the input root appears inside the container: an absolute path. The
    /// overlay is mounted there, the shared trees and the working directory are under it.
    pub exec_root: String,
    /// The input root on the host: the overlay's read-only lower layer.
    pub input_root: PathBuf,
    /// The overlay's upper layer: everything the job writes under the exec root.
    pub upper: PathBuf,
    /// The overlay's work directory (empty, on the upper layer's filesystem).
    pub overlay_work: PathBuf,
    /// The working directory, relative to the exec root.
    pub working_directory: String,
    /// The program's environment, in order: the container gets these variables only.
    pub env: Vec<(String, String)>,
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// The limits written into the bundle.
    pub limits: ContainerLimits,
    /// Host directories mounted read-only inside the exec root (shared trees).
    pub shared: Vec<SharedMount>,
    /// Host directories bound at absolute places in the container, after the exec root
    /// and the shared trees: the run's scratch directory, the `--mount-root` mounts, the
    /// run's files.
    pub mounts: Vec<HostMount>,
}

/// A host directory bound at an absolute place in the container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostMount {
    /// Where it appears: an absolute, normalized path.
    pub target: String,
    /// The directory on the host.
    pub source: PathBuf,
    /// Whether the job sees it read-only.
    pub read_only: bool,
}

/// A host directory mounted read-only inside the exec root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedMount {
    /// Where it appears, from the input root: `/`-separated, no leading slash, never
    /// empty, never containing `:` or `,`. The input root has an empty directory there.
    pub path: String,
    /// The tree on the host. Read-only to the job; it must outlive the job.
    pub source: PathBuf,
}

/// The directories a lease hands between the host process and the job's root.
pub struct Dirs<'a> {
    /// The lease's scratch directory.
    pub lease: &'a Path,
    /// The input root (the overlay's lower layer).
    pub root: &'a Path,
    /// The overlay's upper layer.
    pub upper: &'a Path,
    /// The overlay's work directory.
    pub work: &'a Path,
    /// With the input cache, the daemon's binary: the input root is then given by its
    /// directories alone (the give mode of the host binary), never its files.
    pub helper: Option<&'a Path>,
}

/// How a started container ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It ran and exited with this code.
    Exited(i32),
    /// It did not run to an exit: `crun` reported an error (`exited <code>`).
    NotRun(String),
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Catches `CONTAINER_OWNER` drifting from the id the job runs as: a lease's
    /// directories given to another id than the process's are unwritable to the job.
    #[test]
    fn the_container_owner_is_the_action_id_in_the_setup_namespace() {
        let setup = ACTION_ID + 1;
        assert_eq!(CONTAINER_OWNER, format!("{setup}:{setup}"));
    }

    pub(crate) fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "lease-1-2".to_owned(),
            image: format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            dir: PathBuf::from("/scratch/lease-1-2"),
            cgroup_parent: "/js.slice/actions/lease-1-2".to_owned(),
            input_root: PathBuf::from("/scratch/lease-1-2/root"),
            upper: PathBuf::from("/scratch/lease-1-2/upper"),
            overlay_work: PathBuf::from("/scratch/lease-1-2/work"),
            exec_root: "/job/root".to_owned(),
            working_directory: "pkg".to_owned(),
            env: vec![("PATH".to_owned(), "/bin".to_owned())],
            argv: vec!["tool".to_owned(), "--flag".to_owned(), "a b".to_owned()],
            limits: ContainerLimits {
                pids: 101,
                shm_mib: 102,
                nofile: 103,
                nproc: 104,
                tmp_mib: 106,
            },
            shared: Vec::new(),
            mounts: vec![
                HostMount {
                    target: "/scratch".to_owned(),
                    source: PathBuf::from("/scratch/lease-1-2/job"),
                    read_only: false,
                },
                HostMount {
                    target: "/workspace".to_owned(),
                    source: PathBuf::from("/srv/mount-root"),
                    read_only: false,
                },
                HostMount {
                    target: "/run/js-files".to_owned(),
                    source: PathBuf::from("/run/js-agent/files/1-2"),
                    read_only: true,
                },
            ],
        }
    }
}
