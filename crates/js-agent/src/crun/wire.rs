//! What the agent opens a [`CrunRuntime`] from: the settings its flags made, and the
//! machine's cgroup and registries.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_contain::OciConfig;
use js_contain::container::ACTION_ID;
use js_contain::delegate::{self, PROC_SELF_CGROUP};
use js_contain::job_fs::MountCalls;
use js_contain::oci::RESOLV_CONF;
use js_contain::subids::IdFiles;
use js_oci::{Client, Policy};

use super::adopt::report_lines;
use super::fs::{FsSettings, MOUNTS_DIR, PathCalls};
use super::{
    CgroupSettings, CrunError, CrunRuntime, CrunSettings, GpuSettings, Hooks, HostPorts,
    config_dir, port_of, read_node,
};
use crate::cli_runtime::{CGROUP_ROOT, CrunPlan, Mapping};
use crate::crun_host::CrunHost;
use crate::egress::{Config, Egress};
use crate::image::RegistryImages;

/// The directories `crun`, run as the job user, finds `newuidmap` and `newgidmap` in.
pub const ID_TOOL_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The threads that pull images.
pub const PULL_THREADS: usize = 2;

/// The backend's configuration for the `crun` at `crun`, the job user `job_ids` and the
/// egress `egress`: every `crun` command runs as that user, finding the id tools in
/// [`ID_TOOL_PATH`]. Under `allow-list` or `open`, a container whose job asked for one of them
/// gets the configured resolvers as its only nameservers (the backend's loopback one when it
/// names none) and an init that holds the job until the default route is there
/// ([`Config::route_wait`]); a `none` job, and mode none, get neither (`Oci::create_loopback`).
#[must_use]
pub fn oci_config(crun: &Path, job_ids: (u32, u32), egress: &Config) -> OciConfig {
    OciConfig {
        runtime: crun.to_owned(),
        id_tool_path: Some(ID_TOOL_PATH.into()),
        job_user: Some(job_ids),
        resolv_conf: egress
            .resolv_conf()
            .unwrap_or_else(|| RESOLV_CONF.to_owned()),
        route_wait: egress.route_wait(),
        ..OciConfig::default()
    }
}

/// Where the agent's cgroups come from.
#[derive(Debug, Clone)]
pub enum CgroupSource {
    /// The agent runs in a delegated unit: `mount` is the cgroup
    /// version 2 mount and `own` the file that names the agent's cgroup.
    Delegate {
        /// The cgroup version 2 mount.
        mount: PathBuf,
        /// The file in the form of `/proc/self/cgroup`.
        own: PathBuf,
    },
    /// Cgroups that are set up already.
    Fixed(CgroupSettings),
}

impl CgroupSource {
    /// This machine's: the agent's own cgroup, under the cgroup mount.
    #[must_use]
    pub fn system() -> Self {
        Self::Delegate {
            mount: PathBuf::from(CGROUP_ROOT),
            own: PathBuf::from(PROC_SELF_CGROUP),
        }
    }

    fn settings(&self) -> Result<CgroupSettings, CgroupError> {
        match self {
            Self::Delegate { mount, own } => delegate_unit(mount, own),
            Self::Fixed(settings) => Ok(settings.clone()),
        }
    }
}

/// Why the agent's cgroups cannot be used. Each text names the fix.
#[derive(Debug, thiserror::Error)]
pub enum CgroupError {
    /// The file that names the agent's cgroup cannot be read.
    #[error("read {}: {why}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The reason.
        why: io::Error,
    },
    /// The unit's cgroup cannot be set up.
    #[error(transparent)]
    Delegate(#[from] delegate::DelegateError),
}

/// Sets up the agent's cgroups with [`delegate::delegate`] from the cgroup `own_file` names
/// (in the form of `/proc/self/cgroup`), below the mount `mount`. An agent in the leaf
/// `<unit>/supervisor` (`DelegateSubgroup=supervisor` in the unit) has the unit as its root;
/// an agent in the unit's own cgroup is moved into that leaf. Cpu, memory and pids are
/// enabled in the root and in `<root>/actions`, where the leases go.
fn delegate_unit(mount: &Path, own_file: &Path) -> Result<CgroupSettings, CgroupError> {
    let own = fs::read_to_string(own_file).map_err(|why| CgroupError::Read {
        path: own_file.to_owned(),
        why,
    })?;
    let done = delegate::delegate(mount, &own, 0, None)?;
    Ok(CgroupSettings {
        mount: mount.to_owned(),
        root: done.root,
        actions: done.actions,
    })
}

/// The files the backend reads its ids and id tools from.
#[derive(Debug, Clone)]
pub struct IdSources {
    /// The user database and subordinate id files.
    pub files: IdFiles,
    /// The directories `newuidmap` and `newgidmap` are found in.
    pub tool_path: OsString,
    /// Whether the store's directories are checked for being searchable by the job user.
    pub check_search: bool,
}

impl IdSources {
    /// This machine's.
    #[must_use]
    pub fn system() -> Self {
        Self {
            files: IdFiles::system(),
            tool_path: ID_TOOL_PATH.into(),
            check_search: true,
        }
    }
}

/// Where a runtime opened from a plan mounts what its runs' file systems are made of, and
/// with what: [`MOUNTS_DIR`] and the kernel's calls on a node.
#[derive(Clone)]
pub struct MountSeam {
    /// The directory of the runs' mounts.
    pub dir: PathBuf,
    /// The mounts' system calls.
    pub calls: Arc<dyn MountCalls + Send + Sync>,
    /// The calls that read a host path.
    pub paths: Arc<dyn PathCalls>,
}

impl Default for MountSeam {
    fn default() -> Self {
        Self {
            dir: PathBuf::from(MOUNTS_DIR),
            calls: Arc::new(js_contain::job_fs::Kernel),
            paths: Arc::new(super::fs::Kernel),
        }
    }
}

impl std::fmt::Debug for MountSeam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountSeam")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// What the runtime reads from the machine, which a test replaces.
#[derive(Debug, Clone)]
pub struct Seams {
    /// Where the agent's cgroups come from.
    pub cgroups: CgroupSource,
    /// The runtime's hooks.
    pub hooks: Hooks,
    /// The files and programs the start check reads.
    pub host: fn() -> CrunHost,
    /// The files the backend reads its ids from.
    pub ids: IdSources,
    /// The binary each container's supervisor runs (`__supervise-run`): this program, unless a
    /// test runs the agent inside a binary that does not answer the helper modes.
    pub daemon: PathBuf,
    /// Where the runs' mounts are made, and with what.
    pub mounts: MountSeam,
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            cgroups: CgroupSource::system(),
            hooks: Hooks::default(),
            host: CrunHost::system,
            ids: IdSources::system(),
            daemon: OciConfig::default().daemon,
            mounts: MountSeam::default(),
        }
    }
}

/// Why the runtime could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The agent's cgroups could not be set up.
    #[error("the cgroups: {0}")]
    Cgroups(#[from] CgroupError),
    /// The image source could not be made.
    #[error("the image source: {0}")]
    Images(#[from] js_oci::Error),
    /// The runtime could not open.
    #[error(transparent)]
    Open(#[from] CrunError),
}

/// A [`CrunRuntime`] for `plan`, the job users' mapping, and the lines the operator should
/// read: the warnings, then one line for each run the runtime adopted and each lease it
/// removed as it opened.
///
/// # Errors
/// See [`WireError`].
pub fn open(
    plan: &CrunPlan,
    host_id: &str,
    seams: &Seams,
) -> Result<(CrunRuntime, Mapping, Vec<String>), WireError> {
    let cgroups = seams.cgroups.settings()?;
    plan.delegated(&cgroups.actions);
    let policy = Policy::new(plan.image_allow.clone());
    let images = RegistryImages::new(Client::new(policy), std::env::consts::ARCH)?
        .keeping_configs(config_dir(&plan.store));
    // The egress was started once, when the agent started (`start_egress`): every
    // runtime opened from the plan (the rescue's too) shares it.
    let none = Config::none();
    let config = plan.egress.as_ref().map_or(&none, Egress::config);
    let mut oci = oci_config(&plan.crun, plan.job_ids, config);
    oci.id_files = seams.ids.files.clone();
    oci.id_tool_path = Some(seams.ids.tool_path.clone());
    oci.check_search = seams.ids.check_search;
    oci.daemon = seams.daemon.clone();
    // A lease's `/work` and home are made beside the run's directory in the scratch root, so
    // the run's disk quota bounds them with `/scratch`.
    oci.leases_in = Some(plan.scratch_root.clone());
    let settings = CrunSettings {
        oci,
        scratch: plan.store.clone(),
        cgroups,
        host_id: host_id.to_owned(),
        pull_threads: PULL_THREADS,
        files: plan.files.clone(),
        egress: plan.egress.as_ref().map(port_of),
        // A node is read from the host when a run starts, as the start check read it.
        gpus: plan.gpus.clone().map(|passthrough| GpuSettings {
            passthrough,
            read: read_node,
        }),
        image_budget: plan.image_budget,
        host_jobs: plan.host_jobs,
        host_ports: HostPorts {
            allowed: plan.host_ports.clone(),
            ..HostPorts::default()
        },
        // Only where the kernel has the system calls a job's layers and mounts need.
        fs: plan.layered.then(|| FsSettings {
            dir: seams.mounts.dir.clone(),
            roots: plan.mount_roots.clone(),
            calls: Arc::clone(&seams.mounts.calls),
            paths: Arc::clone(&seams.mounts.paths),
            image_allow: plan.image_allow.clone(),
        }),
    };
    let runtime = CrunRuntime::open_with(settings, Arc::new(images), seams.hooks)?
        .with_live_logs(plan.logs.live());
    let (uid, gid) = runtime.subordinate_ids();
    // One warning, when no registry is allowed; none otherwise.
    let mut lines = vec![
        "--image-allow: none given, so no image is pulled and a job whose image is not on \
         the host is refused"
            .to_owned();
        usize::from(plan.image_allow.is_empty())
    ];
    lines.extend(report_lines(runtime.swept()));
    Ok((runtime, Mapping::new(uid, gid), lines))
}

impl CrunRuntime {
    /// The first host uid and gid of the job user's subordinate ranges, which the jobs'
    /// ids are mapped onto.
    #[must_use]
    pub fn subordinate_ids(&self) -> (u32, u32) {
        let (uid, gid) = self.oci.container_ids();
        (uid - ACTION_ID, gid - ACTION_ID)
    }
}
