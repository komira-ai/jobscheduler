//! What the lease code asks of [`Oci`], answered with a bundle, `crun run` and helper
//! containers: whether the image is usable, who may write the lease's directories,
//! making and starting the container, how it ended, signalling it, removing what it left.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::crun::{RunEnd, log_path};
use super::image::{ImageCheckError, PinnedImage};
use super::logs::{self, Capture, Launch};
use super::words::{crun_words, init_words};
use super::{JobError, Mapping, Oci, at, helper, init, mounts, spec};
use crate::container::{CONTAINER_OWNER, ContainerSpec, Dirs, Ended, own_nproc_hard_limit};
use crate::running::Running;

/// The files in which `crun` and the container's init say why a container did not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reports {
    /// `crun`'s JSON log: read its error records with [`crun_words`].
    pub log: PathBuf,
    /// The init's report, written only when it could not start the program: read it
    /// with [`init_words`].
    pub init: PathBuf,
}

/// The mode of the lease directory and of the small files in it the container reads:
/// the container's ids are not the daemon user's, so they reach them as "others".
const OPEN_DIR: u32 = 0o755;
const OPEN_FILE: u32 = 0o644;

/// The `hosts` file of every job's container: loopback only, as `--network=none`.
const HOSTS: &str = "127.0.0.1 localhost\n::1 localhost\n";

/// The `resolv.conf` of every job's container: the one nameserver a namespace with
/// loopback only has, 127.0.0.1, which nothing listens on. A name that is not in `hosts`
/// then fails at once with a refused query, which glibc reports as `EAI_AGAIN` (a
/// transient failure, as a node without a reachable resolver gives), never as `EAI_NONAME`
/// (no such host) and never as a wait for a timeout. glibc fails at once on the refused
/// query; musl sends on an unconnected socket and would wait out its default 5 s twice per
/// lookup, which `timeout:1 attempts:1` bounds. The image's own file, if it ships one, is
/// covered.
pub const RESOLV_CONF: &str = "nameserver 127.0.0.1\noptions timeout:1 attempts:1\n";

/// Writes `bytes` to `path`, readable by others.
async fn write_open(path: &Path, bytes: &[u8]) -> Result<(), JobError> {
    at(path, tokio::fs::write(path, bytes).await)?;
    let mode = std::fs::Permissions::from_mode(OPEN_FILE);
    at(path, tokio::fs::set_permissions(path, mode).await)
}

/// Makes `path` (and its parents), searchable by others.
async fn open_dir(path: &Path) -> Result<(), JobError> {
    at(path, tokio::fs::create_dir_all(path).await)?;
    let mode = std::fs::Permissions::from_mode(OPEN_DIR);
    at(path, tokio::fs::set_permissions(path, mode).await)
}

/// Who owns a directory of a lease while its container may run, for a daemon that makes
/// the directories itself (a root daemon with a job user).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// The job user: the account `crun` runs as, which writes the lease's state and log.
    Job,
    /// The job's own id in the container ([`Oci::container_ids`]), which the job writes
    /// as.
    Container,
}

/// The directories of one lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseDirs {
    /// The lease directory, named by the container.
    pub dir: PathBuf,
    /// The input root, the overlay's lower layer.
    pub root: PathBuf,
    /// The overlay's upper layer.
    pub upper: PathBuf,
    /// The overlay's work directory.
    pub work: PathBuf,
    /// The directory behind the job's home directory.
    pub home: PathBuf,
}

impl LeaseDirs {
    /// Every directory to make, parents first, each `0755`, with its owner.
    pub fn to_make(&self) -> Vec<(PathBuf, Owner)> {
        let writable = self.dir.join(spec::WRITABLE_DIR);
        vec![
            (self.dir.clone(), Owner::Job),
            (self.root.clone(), Owner::Container),
            (self.upper.clone(), Owner::Container),
            (self.work.clone(), Owner::Container),
            (writable, Owner::Job),
            (self.home.clone(), Owner::Container),
        ]
    }
}

impl Oci {
    /// The directories of the lease `name`, none made.
    pub fn lease_dirs(&self, name: &str) -> LeaseDirs {
        let dir = self.lease(name);
        let home = spec::WRITABLE[0].0;
        LeaseDirs {
            root: dir.join("root"),
            upper: dir.join("upper"),
            work: dir.join("work"),
            home: dir.join(spec::WRITABLE_DIR).join(home),
            dir,
        }
    }

    /// The directory of `image` in the store (its unpacked root is in it), which a caller
    /// that finds the image unfit removes to have it fetched again.
    pub fn image_dir(&self, image: &PinnedImage) -> PathBuf {
        self.images.dir_of(&image.digest)
    }

    /// The job user's uid and gid: the owner of the lease directory ([`Owner::Job`]).
    pub fn job_ids(&self) -> (u32, u32) {
        (self.ids.uid, self.ids.gid)
    }

    /// The host uid and gid a job runs as: its id in the container ([`ACTION_ID`]) is
    /// that of the job user's subordinate range. The owner of what the job writes
    /// ([`Owner::Container`]).
    ///
    /// [`ACTION_ID`]: crate::container::ACTION_ID
    pub fn container_ids(&self) -> (u32, u32) {
        (
            self.ids.sub_uid + crate::container::ACTION_ID,
            self.ids.sub_gid + crate::container::ACTION_ID,
        )
    }

    /// The directories of `dirs` to hand over and the `rw` directory among them.
    async fn hand_over(&self, owner: &str, dirs: &Dirs<'_>) -> Result<(), JobError> {
        let rw = dirs.lease.join(spec::WRITABLE_DIR);
        // The directories are the daemon's to make and open until they are given to the
        // container; given back, they are the container's ids' until the clean.
        if owner == CONTAINER_OWNER {
            open_dir(dirs.lease).await?;
            open_dir(&rw).await?;
            for (name, _) in spec::WRITABLE {
                open_dir(&rw.join(name)).await?;
            }
        }
        let mut whole = vec![dirs.upper, dirs.work, rw.as_path()];
        // With the cache the input root's files are links to its entries: only its
        // directories are given. Without, everything.
        let only_dirs = dirs.helper.is_some().then_some(dirs.root);
        if only_dirs.is_none() {
            whole.insert(0, dirs.root);
        }
        let job = helper::give_job(&self.modes, &self.daemon, owner, only_dirs, &whole);
        self.run_jobs("give", Mapping::Setup, &[job]).await
    }

    /// Writes the bundle of `spec`: `config.json` and the `hosts`, `hostname` and `resolv.conf`
    /// files it binds.
    async fn write_bundle(&self, spec: &ContainerSpec) -> Result<(), JobError> {
        open_dir(&spec.dir).await?;
        let etc = spec.dir.join("etc");
        open_dir(&etc).await?;
        write_open(&etc.join("hosts"), HOSTS.as_bytes()).await?;
        let hostname = format!("{}\n", spec::HOSTNAME);
        write_open(&etc.join("hostname"), hostname.as_bytes()).await?;
        write_open(&etc.join("resolv.conf"), RESOLV_CONF.as_bytes()).await?;
        let rootfs = self.images.rootfs_of(&spec.image_digest);
        let placement = spec::Placement {
            rootfs: &rootfs,
            cgroup: &spec.cgroup_parent,
            ids: &self.ids,
            arch: self.arch,
            init: &self.init,
            init_dest: &self.init_dest,
        };
        // A rootless daemon cannot give a job a process limit above its own hard one.
        let mut job = spec.clone();
        job.limits = spec.limits.within_nproc(own_nproc_hard_limit());
        let bytes = spec::render(&spec::action_config(&job, &placement)?)?;
        let bundle = spec.dir.join("bundle");
        at(&bundle, tokio::fs::create_dir_all(&bundle).await)?;
        let path = bundle.join("config.json");
        at(&path, tokio::fs::write(&path, bytes).await)
    }
}

impl Oci {
    /// The image is unpacked, once per digest and off the job path; a lease finds it
    /// there with one `stat`.
    ///
    /// # Errors
    /// The image cannot be fetched or unpacked, or names an index.
    pub async fn check_pinned(&self, image: &PinnedImage) -> Result<(), ImageCheckError> {
        self.images.ensure(self, image).await
    }

    /// Removes `image` from the store (a root daemon removes the files of any owner) so that
    /// the next [`Oci::check_pinned`] fetches it again, waiting for a fetch of it under way.
    /// No container may run on it: its root is the lower layer of the mounts of one.
    ///
    /// # Errors
    /// The directory could not be removed.
    pub async fn remove_image(&self, image: &PinnedImage) -> std::io::Result<()> {
        self.images.remove(image).await
    }

    /// One helper container runs this daemon's give mode ([`helper::HelperModes`]) over the directories
    /// (the lease's `rw` directory, which holds the job's home directory, among them).
    ///
    /// # Errors
    /// The helper could not be made or failed, in `crun`'s words.
    pub async fn give(&self, owner: &str, dirs: &Dirs<'_>) -> Result<(), String> {
        Ok(self.hand_over(owner, dirs).await?)
    }

    /// Writes the bundle of `spec`.
    ///
    /// # Errors
    /// A file of the bundle could not be written.
    pub async fn create(&self, spec: &ContainerSpec) -> Result<(), String> {
        Ok(self.write_bundle(spec).await?)
    }

    /// `crun run` on a thread of its own; it ends when the container does. The
    /// container's stdout and stderr are pipes, drained into the lease's files and
    /// [`Running::tail`] as [`logs`](super::logs) says: the files are made here (in
    /// `spec.dir`, [`stdout_path`](super::stdout_path) and
    /// [`stderr_path`](super::stderr_path), new and empty), and the run is not over until
    /// the drain is (a bounded wait).
    ///
    /// # Errors
    /// The init's report file, a log file or the thread could not be made.
    pub fn start(&self, spec: &ContainerSpec) -> Result<Running, String> {
        let crun = self.crun.clone();
        let (dir, name) = (spec.dir.clone(), spec.name.clone());
        let bundle = dir.join("bundle");
        let thread = format!("crun-{name}");
        // The init's stdin: where it says why the program could not be started.
        let report = init::create_report(&dir)?;
        let capture = Arc::new(Capture::create(&dir, self.logs)?);
        let launch: Launch = Box::new(move |stdout, stderr| {
            Box::pin(async move { crun.run(&dir, &name, &bundle, report, stdout, stderr).await })
        });
        let run = logs::supervise(capture.clone(), launch);
        Ok(Running::spawn(thread, run)?.with_capture(capture))
    }

    /// `crun run`'s exit status is the job's, unless `crun` itself failed: told by an
    /// error record in its log.
    ///
    /// # Errors
    /// `crun` itself could not be run or did not finish.
    pub async fn ended(&self, spec: &ContainerSpec, running: &Running) -> Result<Ended, String> {
        match running.run_end() {
            RunEnd::Exited(0) => Ok(Ended::Exited(0)),
            RunEnd::Exited(code) => {
                let log = tokio::fs::read_to_string(log_path(&spec.dir)).await;
                let report = tokio::fs::read_to_string(init::report_path(&spec.dir)).await;
                if crun_words(&log.unwrap_or_default()).is_empty()
                    && init_words(&report.unwrap_or_default()).is_empty()
                {
                    Ok(Ended::Exited(code))
                } else {
                    Ok(Ended::NotRun(format!("exited {code}")))
                }
            }
            RunEnd::Failed(why) => Err(why),
        }
    }

    /// Where `crun`'s own words about the container of `spec` are: its JSON log, and the
    /// report the container's init writes when it cannot start the program.
    pub fn reports(&self, spec: &ContainerSpec) -> Reports {
        Reports {
            log: log_path(&spec.dir),
            init: init::report_path(&spec.dir),
        }
    }

    /// Reads back the mounts of the started container of `spec` and compares them with its
    /// bundle ([`mounts::problems`]): the replacement for the engine's read-back after a
    /// create. Blocking: it runs `crun state` and reads a file of `/proc`. The container
    /// must be running; one that is not yet gives an error, and the caller may ask again.
    ///
    /// # Errors
    /// `crun state` failed, the container's `mountinfo` or the bundle could not be read,
    /// or a mount the bundle flags lacks the flag.
    pub fn verify_mounts(&self, spec: &ContainerSpec) -> Result<(), String> {
        Ok(self.read_back(spec)?)
    }

    /// [`Oci::verify_mounts`] with its errors typed.
    fn read_back(&self, spec: &ContainerSpec) -> Result<(), JobError> {
        let pid = self.crun.pid(&spec.dir, &spec.name)?;
        let info = PathBuf::from(format!("/proc/{pid}/mountinfo"));
        let mountinfo = at(&info, std::fs::read_to_string(&info))?;
        let path = spec.dir.join("bundle/config.json");
        let bytes = at(&path, std::fs::read(&path))?;
        let config = serde_json::from_slice(&bytes).map_err(spec::SpecError::Json)?;
        let found = mounts::problems(&config, &mountinfo);
        if found.is_empty() {
            Ok(())
        } else {
            Err(JobError::Mounts(found))
        }
    }

    /// Sends `SIGTERM` to the container `name`.
    ///
    /// # Errors
    /// `crun kill` failed.
    pub async fn terminate(&self, name: &str) -> Result<(), String> {
        self.crun.kill(&self.lease(name), name, 15).await
    }

    /// `crun delete --force`: nothing to do when `crun run` ended normally (it deletes
    /// its container as it exits), and the removal of the container, its cgroup and
    /// its state when it did not. Always run rather than guessed from the state
    /// directory's layout, which is `crun`'s to change.
    ///
    /// # Errors
    /// `crun delete` failed and the state did not go away.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        self.crun.delete_blocking(&self.lease(name), name)
    }

    /// Removes `dir` whatever ids own what is in it ([`Oci::remove_dir_blocking`]).
    ///
    /// # Errors
    /// The helper or the removal failed.
    pub fn remove_dir(&self, dir: &Path) -> Result<(), String> {
        Ok(self.remove_dir_blocking(dir)?)
    }
}
