//! What the agent opens a [`CrunRuntime`] from: the settings its flags made, and the
//! machine's cgroup and registries.

use std::path::Path;
use std::sync::Arc;

use js_contain::OciConfig;
use js_contain::container::ACTION_ID;
use js_contain::delegate::{self, PROC_SELF_CGROUP};
use js_oci::{Client, Policy};

use super::{CgroupSettings, CrunRuntime, CrunSettings, Hooks, config_dir};
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

/// What the runtime does that a test replaces: setting up the agent's cgroups, and the
/// runtime's own hooks.
#[derive(Clone, Copy)]
pub struct Seams {
    /// Sets up the agent's delegated cgroup and the cgroup the leases are made in.
    pub cgroups: fn() -> Result<CgroupSettings, String>,
    /// The runtime's hooks.
    pub hooks: Hooks,
    /// The files and programs the start check reads.
    pub host: fn() -> CrunHost,
    /// Changes the backend's configuration before it is used (the files its ids are read
    /// from, say).
    pub configure: fn(&mut OciConfig),
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            cgroups: delegated,
            hooks: Hooks::default(),
            host: CrunHost::system,
            configure: |_| {},
        }
    }
}

impl std::fmt::Debug for Seams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seams").finish_non_exhaustive()
    }
}

/// The agent's own cgroup, set up for its leases ([`delegate::delegate`]).
fn delegated() -> Result<CgroupSettings, String> {
    let own = std::fs::read_to_string(PROC_SELF_CGROUP)
        .map_err(|why| format!("read {PROC_SELF_CGROUP}: {why}"))?;
    let mount = Path::new(CGROUP_ROOT);
    let done = delegate::delegate(mount, &own, 0, None).map_err(|why| why.to_string())?;
    Ok(CgroupSettings {
        mount: mount.to_owned(),
        root: done.root,
        actions: done.actions,
    })
}

/// A [`CrunRuntime`] for `plan`, and the warnings the operator should read.
///
/// # Errors
/// The cgroups cannot be set up or the runtime cannot open; the text says why.
pub fn open(
    plan: &CrunPlan,
    host_id: &str,
    seams: &Seams,
) -> Result<(CrunRuntime, Mapping, Vec<String>), String> {
    let cgroups = (seams.cgroups)()?;
    plan.delegated(&cgroups.actions);
    let policy = Policy::new(plan.image_allow.clone());
    let images = RegistryImages::new(Client::new(policy), std::env::consts::ARCH)
        .map_err(|why| why.to_string())?
        .keeping_configs(config_dir(&plan.store));
    let mut oci = oci_config(&plan.crun, plan.job_ids);
    (seams.configure)(&mut oci);
    let settings = CrunSettings {
        oci,
        scratch: plan.store.clone(),
        cgroups,
        host_id: host_id.to_owned(),
        pull_threads: PULL_THREADS,
        files: plan.files.clone(),
    };
    let runtime = CrunRuntime::open_with(settings, Arc::new(images), seams.hooks)
        .map_err(|why| why.to_string())?;
    let (uid, gid) = runtime.subordinate_ids();
    let warnings = match plan.image_allow.is_empty() {
        true => vec![
            "--image-allow: none given, so no image is pulled and a job whose image is not \
             on the host is refused"
                .to_owned(),
        ],
        false => Vec::new(),
    };
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
