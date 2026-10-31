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
use js_contain::subids::IdFiles;
use js_oci::{Client, Policy};

use super::{CgroupSettings, CrunError, CrunRuntime, CrunSettings, Hooks, config_dir};
use crate::cli_runtime::{CGROUP_ROOT, CrunPlan, Mapping};
use crate::crun_host::CrunHost;
use crate::image::RegistryImages;

/// The directories `crun`, run as the job user, finds `newuidmap` and `newgidmap` in.
pub const ID_TOOL_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The threads that pull images.
pub const PULL_THREADS: usize = 2;

/// The backend's configuration for the `crun` at `crun` and the job user `job_ids`: every
/// `crun` command runs as that user, finding the id tools in [`ID_TOOL_PATH`].
#[must_use]
pub fn oci_config(crun: &Path, job_ids: (u32, u32)) -> OciConfig {
    OciConfig {
        runtime: crun.to_owned(),
        id_tool_path: Some(ID_TOOL_PATH.into()),
        job_user: Some(job_ids),
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
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            cgroups: CgroupSource::system(),
            hooks: Hooks::default(),
            host: CrunHost::system,
            ids: IdSources::system(),
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

/// A [`CrunRuntime`] for `plan`, the job users' mapping, and the warnings the operator
/// should read.
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
    let mut oci = oci_config(&plan.crun, plan.job_ids);
    oci.id_files = seams.ids.files.clone();
    oci.id_tool_path = Some(seams.ids.tool_path.clone());
    oci.check_search = seams.ids.check_search;
    let settings = CrunSettings {
        oci,
        scratch: plan.store.clone(),
        cgroups,
        host_id: host_id.to_owned(),
        pull_threads: PULL_THREADS,
        files: plan.files.clone(),
    };
    let runtime = CrunRuntime::open_with(settings, Arc::new(images), seams.hooks)?;
    let (uid, gid) = runtime.subordinate_ids();
    // One warning, when no registry is allowed; none otherwise.
    let warnings = vec![
        "--image-allow: none given, so no image is pulled and a job whose image is not on \
         the host is refused"
            .to_owned();
        usize::from(plan.image_allow.is_empty())
    ];
    Ok((runtime, Mapping::new(uid, gid), warnings))
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
