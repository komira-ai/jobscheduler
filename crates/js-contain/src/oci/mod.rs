//! The container runtime: each job in a fresh rootless sandbox made by `crun`, the only
//! container runtime this crate drives.
//!
//! Per job, from the daemon user's account, with nothing running as root and no
//! helper that unshares by itself (`crun` creates every namespace, so the only
//! AppArmor grant for user namespaces needed is the one `crun` ships with):
//!
//! 1. the image is fetched by an [`image::ImageSource`] (the caller's registry client) and
//!    unpacked once per digest into the node's image store ([`image`]), read-only
//!    afterwards;
//! 2. the lease's directories are given to the job's root by a helper container
//!    ([`helper`]);
//! 3. a bundle is written: `config.json` (`spec`) plus the lease's `hosts` and
//!    `hostname` files, with the capability set, masked paths and seccomp filter written
//!    out (`hardening`);
//! 4. `crun run` runs the bundle in the foreground (`crun`): its exit status is the
//!    job's, so there is no moment at which the container is stopped but its exit
//!    code is unknown;
//! 5. a helper container gives the directories back, the driver collects outputs from
//!    the upper layer, and the lease is removed.
//!
//! **What the container sees:** the unpacked image, read-only, as `/`; the input root
//! as an overlay at [`ContainerSpec::exec_root`](crate::container::ContainerSpec::exec_root) (lower: the lease's input root; upper: the lease's
//! upper directory, which outputs are read from); shared read-only trees
//! ([`SharedMount`](crate::container::SharedMount)) bound inside it; `/tmp`, `/var/tmp`, `/run` and
//! `/root` as the lease's own disk directories (`spec::WRITABLE`); `/dev`, `/proc`,
//! `/sys` and `/dev/shm` as for any container. The image is read-only; those four
//! directories are where a job writes besides the exec root.
//!
//! **Ids:** inside the container, id 0 is the first subordinate id of the daemon user
//! ([`DaemonIds`]), the daemon user itself is not mapped, so
//! the inodes it owns (the cache's files, the shared trees) are `nobody`'s to the
//! job. The node needs one range of at least 65,536 subordinate ids in
//! `/etc/subuid` and `/etc/subgid`, and every directory from `/` down to the scratch
//! directory must be searchable by others (the container's ids are not the daemon
//! user's): [`Oci::new`] checks both.
//!
//! **cgroups:** the container's cgroup is `<lease cgroup>/ctr`, made by `crun` in the
//! daemon's delegated subtree with the pids limit and `memory.oom.group=1`; memory and
//! CPU limits stay on the lease cgroup.

pub(crate) mod crun;
pub(crate) mod hardening;
pub mod helper;
pub mod image;
pub mod init;
mod lease;
pub(crate) mod spec;
pub mod words;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oci_spec::runtime::{LinuxIdMapping, LinuxIdMappingBuilder};

pub use crun::{EnvScrub, RunEnd};
pub use lease::{RESOLV_CONF, Reports};

/// The resolved seccomp section for `arch` (Go's name) and a job holding `caps`, as JSON.
/// Only the egress spike tests (`tests/egress_spike`) call this, so that they apply the
/// driver's own resolution of the pinned profile and not a copy of it.
///
/// # Errors
/// The text of the profile error.
#[doc(hidden)]
pub fn seccomp_section(arch: &str, caps: &[&str]) -> Result<serde_json::Value, String> {
    let section = hardening::seccomp(arch, caps).map_err(|e| e.to_string())?;
    serde_json::to_value(section).map_err(|e| e.to_string())
}

use crate::subids::{DaemonIds, IdFiles, SubidError};

/// Where the OCI backend finds its runtime and the ids it maps.
#[derive(Clone, Debug)]
pub struct OciConfig {
    /// The OCI runtime, `crun`.
    pub runtime: PathBuf,
    /// This daemon's own binary, which a helper container runs in the modes of
    /// [`helper::HelperModes`] (descriptor-relative ownership changes that never follow a
    /// symlink, and the unpack of an image's layers). By default the running executable.
    pub daemon: PathBuf,
    /// Where the container sees its init (absolute, inside `/dev`): also its name in the
    /// process table.
    pub init_dest: String,
    /// The arguments that select those modes of `daemon`, and where it is bound.
    pub helper_modes: helper::HelperModes,
    /// Applied to every command line the backend starts (`crun` itself): the caller
    /// removes from the environment what no container runtime should inherit, such as the
    /// node's own credentials. By default it changes nothing.
    pub scrub_env: EnvScrub,
    /// The user database and subordinate id files the daemon user's ranges are read from.
    pub id_files: IdFiles,
    /// Whether to refuse a scratch directory some ancestor of which others cannot
    /// search. On by default; a test that runs a stand-in runtime as one user (so no
    /// other id needs to reach the lease) in a home directory closed to others turns it off.
    pub check_search: bool,
    /// The directories `newuidmap` and `newgidmap` are looked for in (`crun` runs them
    /// by name to map the container's ids). By default the daemon's `PATH`.
    pub id_tool_path: Option<std::ffi::OsString>,
}

impl Default for OciConfig {
    fn default() -> Self {
        Self {
            runtime: PathBuf::from("/usr/bin/crun"),
            daemon: std::env::current_exe().unwrap_or_default(),
            id_files: IdFiles::system(),
            check_search: true,
            id_tool_path: std::env::var_os("PATH"),
            init_dest: init::DEFAULT_DEST.to_owned(),
            helper_modes: helper::HelperModes::default(),
            scrub_env: crun::no_scrub,
        }
    }
}

/// Which user namespace a container is made in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// The daemon user is id 0 and the subordinate range starts at id 1
    /// ([`DaemonIds::setup_mappings`]): the namespace in which the daemon user's files
    /// are given to the job's ids and back.
    Setup,
    /// A job's: id 0 is the first subordinate id ([`DaemonIds::action_mappings`]).
    Action,
}

/// The pair of uid and gid mappings of a container.
pub type Mappings = (Vec<LinuxIdMapping>, Vec<LinuxIdMapping>);

impl DaemonIds {
    /// How many ids a job's namespace maps (a full 16-bit range).
    pub const ACTION_SIZE: u32 = 65_536;

    /// A job's namespace: ids `0..65536` to the subordinate range, the daemon user
    /// unmapped.
    pub fn action_mappings(&self) -> Mappings {
        (
            vec![range(0, self.sub_uid, Self::ACTION_SIZE)],
            vec![range(0, self.sub_gid, Self::ACTION_SIZE)],
        )
    }

    /// The setup namespace: id 0 is the daemon user, ids `1..=65536` the subordinate range
    /// (so every id of a job's namespace has one here, id `n` there being `n + 1`).
    pub fn setup_mappings(&self) -> Mappings {
        (
            vec![
                range(0, self.uid, 1),
                range(1, self.sub_uid, Self::ACTION_SIZE),
            ],
            vec![
                range(0, self.gid, 1),
                range(1, self.sub_gid, Self::ACTION_SIZE),
            ],
        )
    }

    /// The mappings of the namespace `which`.
    pub fn mappings(&self, which: Mapping) -> Mappings {
        match which {
            Mapping::Setup => self.setup_mappings(),
            Mapping::Action => self.action_mappings(),
        }
    }
}

fn range(container: u32, host: u32, size: u32) -> LinuxIdMapping {
    // All three fields are set, so the builder cannot fail.
    LinuxIdMappingBuilder::default()
        .container_id(container)
        .host_id(host)
        .size(size)
        .build()
        .unwrap_or_default()
}

/// Why the OCI backend cannot start on this node.
#[derive(Debug, thiserror::Error)]
pub enum OciError {
    /// The daemon user's subordinate id ranges are not usable.
    #[error(transparent)]
    Subids(#[from] SubidError),
    /// A directory on the way to the scratch directory is closed to others.
    #[error(
        "{0} is not searchable by others: the OCI backend's container ids are not the \
         daemon user's, and need to reach the lease directories (chmod o+x it)"
    )]
    NotSearchable(PathBuf),
    /// The OCI runtime or the daemon's own binary is not a file.
    #[error(
        "{0} is not a file: the OCI backend needs the OCI runtime and this daemon's own binary"
    )]
    NoRuntime(PathBuf),
    /// `newuidmap` or `newgidmap`, which `crun` runs to map a container's subordinate
    /// ids, is not usable.
    #[error(
        "{tool}: {why}; the OCI backend maps subordinate ids with newuidmap and newgidmap \
         (package uidmap), found on PATH and setuid root or holding file capabilities"
    )]
    IdTool {
        /// The tool that is not usable.
        tool: &'static str,
        /// Why not.
        why: String,
    },
    /// A directory or file of the backend's own state could not be prepared.
    #[error("{0}")]
    Prepare(String),
    /// The machine's architecture has no seccomp profile.
    #[error("this machine's architecture ({0}) has no seccomp profile")]
    Arch(String),
}

/// What went wrong with one step of a lease the OCI backend runs: a file, the
/// configuration, or `crun`. Shown as text where the lease code takes a `String`.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// A file operation failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path the operation was about.
        path: PathBuf,
        /// What the operating system said.
        source: std::io::Error,
    },
    /// The bundle's configuration could not be built or written.
    #[error(transparent)]
    Spec(#[from] spec::SpecError),
    /// `crun` failed, in its own words.
    #[error("{0}")]
    Crun(String),
}

impl From<String> for JobError {
    fn from(error: String) -> Self {
        Self::Crun(error)
    }
}

impl From<JobError> for String {
    fn from(error: JobError) -> Self {
        error.to_string()
    }
}

/// `result`, with the path it was about named in its error.
pub fn at<T>(path: &Path, result: std::io::Result<T>) -> Result<T, JobError> {
    result.map_err(|source| JobError::Io {
        path: path.to_owned(),
        source,
    })
}

/// The OCI backend.
#[derive(Debug)]
pub struct Oci {
    crun: crun::Crun,
    daemon: PathBuf,
    modes: helper::HelperModes,
    init_dest: String,
    /// The container init (`init`), installed under the scratch directory.
    init: PathBuf,
    source: Arc<dyn image::ImageSource>,
    ids: DaemonIds,
    arch: &'static str,
    scratch: PathBuf,
    helper_root: PathBuf,
    images: image::Images,
    helpers: AtomicU64,
    /// The cgroup helper containers are made under: the lease cgroups' parent, so what a
    /// helper uses is bounded by the node's jobs budget and `crun` makes (and kills,
    /// on its removal) no cgroup of its own choosing.
    cgroup_parent: String,
}

impl Oci {
    /// The backend for the leases whose scratch directories are in `scratch`, fetching
    /// images with `source`.
    ///
    /// # Errors
    /// The daemon user has no suitable subordinate range, a directory on the way to
    /// `scratch` is closed to others, the runtime is missing, `newuidmap` or
    /// `newgidmap` is not usable, or the node's architecture has no seccomp profile.
    pub fn new(
        config: &OciConfig,
        source: Arc<dyn image::ImageSource>,
        scratch: &Path,
        cgroup_parent: &str,
    ) -> Result<Arc<Self>, OciError> {
        Self::on(
            config,
            source,
            scratch,
            cgroup_parent,
            std::env::consts::ARCH,
        )
    }

    /// [`Oci::new`] on a machine of architecture `rust_arch` (`std::env::consts::ARCH`).
    fn on(
        config: &OciConfig,
        source: Arc<dyn image::ImageSource>,
        scratch: &Path,
        cgroup_parent: &str,
        rust_arch: &str,
    ) -> Result<Arc<Self>, OciError> {
        let ids = crate::subids::daemon_ids(&config.id_files)?;
        for program in [&config.runtime, &config.daemon] {
            if !program.is_file() {
                return Err(OciError::NoRuntime(program.clone()));
            }
        }
        for tool in ["newuidmap", "newgidmap"] {
            id_tool(tool, config.id_tool_path.as_deref())?;
        }
        let arch = hardening::profile_arch(rust_arch)
            .ok_or_else(|| OciError::Arch(rust_arch.to_owned()))?;
        at(scratch, std::fs::create_dir_all(scratch)).map_err(prepare)?;
        if config.check_search {
            search_check(scratch)?;
        }
        let helper_root = scratch.join("oci-helper");
        at(
            &helper_root,
            helper::prepare_rootfs(&helper_root, &config.helper_modes.binary),
        )
        .map_err(prepare)?;
        let daemon =
            at(scratch, helper::install_daemon(scratch, &config.daemon)).map_err(prepare)?;
        let init = at(scratch, init::install(scratch)).map_err(prepare)?;
        let oci = Arc::new(Self {
            crun: crun::Crun::new(config.runtime.clone(), config.scrub_env),
            daemon,
            modes: config.helper_modes.clone(),
            init_dest: config.init_dest.clone(),
            init,
            source,
            ids,
            arch,
            scratch: scratch.to_owned(),
            helper_root,
            images: image::Images::new(scratch.join("oci-images")),
            helpers: AtomicU64::new(0),
            cgroup_parent: cgroup_parent.to_owned(),
        });
        let leftovers = oci.images.sweep(&oci);
        leftovers.map_err(OciError::Prepare)?;
        Ok(oci)
    }

    /// The source the image store fetches with.
    pub(crate) fn source(&self) -> &dyn image::ImageSource {
        &*self.source
    }

    /// The scratch directory of the lease `name`.
    fn lease(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }

    /// Writes the helper bundle for `job`, to run as the container `id`, under `dir` and
    /// returns its path.
    fn helper_bundle(
        &self,
        dir: &Path,
        mapping: Mapping,
        id: &str,
        job: &helper::Job,
    ) -> Result<PathBuf, JobError> {
        let bundle = dir.join(format!("helper-{}", self.next()));
        at(&bundle, std::fs::create_dir_all(&bundle))?;
        let cgroup = format!("{}/{id}", self.cgroup_parent);
        let config = helper::config(&self.helper_root, &self.ids, mapping, &cgroup, job);
        let file = bundle.join("config.json");
        at(&file, std::fs::write(&file, spec::render(&config)?))?;
        Ok(bundle)
    }

    /// A number unique to this node's run of the daemon.
    fn next(&self) -> u64 {
        self.helpers.fetch_add(1, Ordering::Relaxed)
    }

    /// The OCI id of a helper: unique per node and call.
    fn helper_id(&self, what: &str) -> String {
        format!("helper-{what}-{}-{}", std::process::id(), self.next())
    }

    /// A directory under the node's helper state for one job.
    fn job_dir(&self, what: &str) -> PathBuf {
        self.scratch
            .join("oci-helper-state")
            .join(self.helper_id(what))
    }

    /// Runs `job` in a helper container made in `dir` (a directory the bundle and the
    /// helper's state go in).
    async fn run_job(
        &self,
        dir: &Path,
        what: &str,
        mapping: Mapping,
        job: &helper::Job,
    ) -> Result<(), JobError> {
        at(dir, std::fs::create_dir_all(dir))?;
        let id = self.helper_id(what);
        let bundle = self.helper_bundle(dir, mapping, &id, job)?;
        let state = dir.join("helper-state");
        let result = self.crun.run_helper(&state, &id, &bundle).await;
        let _ = std::fs::remove_dir_all(&bundle);
        Ok(result?)
    }

    /// [`Oci::run_job`] in a directory of its own, removed after.
    async fn run_jobs(
        &self,
        what: &str,
        mapping: Mapping,
        jobs: &[helper::Job],
    ) -> Result<(), JobError> {
        let dir = self.job_dir(what);
        let mut result = Ok(());
        for job in jobs {
            result = self.run_job(&dir, what, mapping, job).await;
            if result.is_err() {
                break;
            }
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
        result
    }

    /// Unpacks the layer files `layers` of `blobs` into `stage/rootfs` in a container
    /// with a job's user namespace (`helper::unpack_job`).
    pub async fn unpack_layers(
        &self,
        stage: &Path,
        blobs: &Path,
        layers: &[String],
    ) -> Result<(), JobError> {
        let job = helper::unpack_job(&self.modes, &self.daemon, stage, blobs, layers);
        self.run_jobs("unpack", Mapping::Action, &[job]).await
    }

    /// Removes `dir` whatever ids own the files in it: the contents inside a helper
    /// container, then the (daemon-owned) directory. Blocking.
    pub fn remove_dir_blocking(&self, dir: &Path) -> Result<(), JobError> {
        let work = self.job_dir("rm");
        at(&work, std::fs::create_dir_all(&work))?;
        let id = self.helper_id("rm");
        let job = helper::remove_job(dir);
        let bundle = self.helper_bundle(&work, Mapping::Setup, &id, &job)?;
        let state = work.join("state");
        let ran = self.crun.run_helper_blocking(&state, &id, &bundle);
        let _ = std::fs::remove_dir_all(&work);
        ran?;
        at(dir, std::fs::remove_dir(dir))
    }

    /// [`Oci::remove_dir_blocking`] for the async threads.
    pub async fn remove_dir_async(&self, dir: &Path) -> Result<(), JobError> {
        let jobs = [helper::remove_job(dir)];
        self.run_jobs("rm", Mapping::Setup, &jobs).await?;
        at(dir, tokio::fs::remove_dir(dir).await)
    }
}

fn prepare(error: JobError) -> OciError {
    OciError::Prepare(error.to_string())
}

/// Checks that every directory from `/` to `dir` can be searched by others.
fn search_check(dir: &Path) -> Result<(), OciError> {
    use std::os::unix::fs::PermissionsExt as _;
    let absolute = at(dir, std::fs::canonicalize(dir)).map_err(prepare)?;
    for ancestor in absolute.ancestors() {
        let meta = at(ancestor, std::fs::metadata(ancestor)).map_err(prepare)?;
        if meta.permissions().mode() & 0o001 == 0 {
            return Err(OciError::NotSearchable(ancestor.to_owned()));
        }
    }
    Ok(())
}

/// Checks that `tool` is on `path`, runnable, and able to write a user namespace's id
/// maps: setuid, or holding file capabilities (as some distributions install it).
fn id_tool(tool: &'static str, path: Option<&std::ffi::OsStr>) -> Result<(), OciError> {
    use std::os::unix::fs::PermissionsExt as _;
    let refused = |why: String| OciError::IdTool { tool, why };
    let (found, meta) = std::env::split_paths(path.unwrap_or_default())
        .map(|dir| dir.join(tool))
        .find_map(|candidate| {
            let meta = std::fs::metadata(&candidate).ok()?;
            meta.is_file().then_some((candidate, meta))
        })
        .ok_or_else(|| refused("not found on PATH".to_owned()))?;
    let mode = meta.permissions().mode();
    // Both facts are read: either is enough.
    let setuid = mode & 0o4000 != 0;
    // The value is not wanted, only whether there is one: an empty buffer asks its size.
    let mut none: [u8; 0] = [];
    let capable = rustix::fs::getxattr(&found, "security.capability", &mut none).is_ok();
    if !(setuid | capable) {
        return Err(refused(format!(
            "{} is neither setuid nor holds file capabilities",
            found.display()
        )));
    }
    if mode & 0o111 == 0 {
        return Err(refused(format!("{} is not executable", found.display())));
    }
    Ok(())
}
