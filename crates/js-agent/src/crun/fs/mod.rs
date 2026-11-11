//! A job's file system on the `crun` runtime (`docs/design/job-rootfs.md`, sections 4.2 to
//! 4.7 and 8): [`Fs`], which holds the roots a job's host paths lie beneath (opened when
//! the agent starts), the directory of the runs' mounts (`<mounts>`, made a private mount
//! of its own when the agent starts) and the `js-paths` pool, and prepares each run's file
//! system off the lifecycle's worker:
//!
//! 1. each host path is opened beneath its root, and a mount's `sub_path` beneath it, on the
//!    pool ([`Fs::resolve`]), each lookup bounded by [`LOOKUP_MS`] on the lifecycle's clock;
//! 2. once they are open and the images are in the store, the assembly ([`assemble`]) runs
//!    on the pool ([`Fs::assemble`]): clones, overlays, targets, each mount listed in
//!    `<mounts>/<lease>/mounts` ([`list`]) before it is made;
//! 3. the container takes what was made ([`Fs::take`]); a run never made has it removed
//!    ([`Fs::discard`]); every end of a run unmounts it from the list ([`Fs::release`]);
//! 4. after a job that did not succeed, each of its sources is asked whether it still
//!    answers ([`Fs::check`]), within [`CHECK_MS`].
//!
//! The system calls are [`MountCalls`] (js-contain's) and [`PathCalls`], so what is built
//! from them is tested with fakes as an ordinary user.

pub(crate) mod assemble;
pub(crate) mod kernel;
mod list;
pub(crate) mod pool;

#[cfg(test)]
mod assemble_tests;
#[cfg(test)]
mod fs_tests;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;

use js_agent_wire::{Layer, refusal};
use js_contain::job_fs::{MountCalls, ResolveError, make_private_dir, open_root};

pub(crate) use assemble::{Assembled, Lower, Owners, Plan, Unassembled};
pub use kernel::{Kernel, PathCalls};
use pool::{Pending, Poll, Pool};

use crate::lifecycle::{FsPrepared, JobFs, MountRoots, NativeRefusal, RuntimeError};

/// Where the agent mounts what its runs' file systems are made of: outside the scratch
/// root, which every release sweeps at its start, on a `tmpfs` a reboot clears.
pub const MOUNTS_DIR: &str = "/run/js-agent/mounts";

/// How long the lookup of one host path may take, from when it is asked.
pub const LOOKUP_MS: u64 = 60_000;

/// How long the check of a job's sources after it ended may take.
pub const CHECK_MS: u64 = 5_000;

/// What a [`CrunRuntime`](super::CrunRuntime) prepares a job's file system with.
#[derive(Clone)]
pub struct FsSettings {
    /// The directory of the runs' mounts ([`MOUNTS_DIR`] on a node).
    pub dir: PathBuf,
    /// The roots a job's host paths may lie beneath (`--mount-root`, `--mount-root-ro`).
    pub roots: MountRoots,
    /// The mounts' system calls.
    pub calls: Arc<dyn MountCalls + Send + Sync>,
    /// The calls that read a host path.
    pub paths: Arc<dyn PathCalls>,
    /// The registries an image layer may be pulled from, as the job's image
    /// (`--image-allow`, `--image-allow-loopback`).
    pub image_allow: Vec<js_oci::Allow>,
}

impl std::fmt::Debug for FsSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsSettings")
            .field("dir", &self.dir)
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}

/// Why the file systems of the runs cannot be prepared on this node.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// The directory of the runs' mounts could not be made, or made a private mount.
    #[error("the directory of the runs' mounts {}: {why}", dir.display())]
    Dir {
        /// The directory.
        dir: PathBuf,
        /// Why.
        why: String,
    },
    /// A root could not be opened.
    #[error("the mount root {name}: {why}")]
    Root {
        /// The root's name.
        name: String,
        /// Why.
        why: ResolveError,
    },
    /// The pool's threads could not be made.
    #[error("the threads of the host paths: {0}")]
    Threads(#[from] io::Error),
}

/// One host path being looked up, or open.
#[derive(Debug)]
enum Lookup {
    /// Asked of the pool: the path, and the answer to come.
    Asked(String, Pending<Result<OwnedFd, NativeRefusal>>),
    /// Open.
    Open(OwnedFd),
}

/// How the assembly of a run stands.
#[derive(Debug)]
enum Assembly {
    /// Not asked yet: the host paths are being looked up, or are open.
    Unasked,
    /// Asked of the pool.
    Asked(Pending<Result<Assembled, Unassembled>>),
    /// Made, for the container to take.
    Made(Assembled),
}

/// What is being prepared for one run.
#[derive(Debug)]
struct Prep {
    /// The run's lease, which names its directory of mounts.
    name: String,
    /// The start's image, empty for a root of layers alone.
    image: String,
    /// The start's layers and mounts.
    fs: JobFs,
    lookups: Vec<Lookup>,
    assembly: Assembly,
}

/// The file systems of a runtime's runs; see the [module](self).
pub(crate) struct Fs {
    dir: PathBuf,
    roots: MountRoots,
    /// The roots, opened, by name.
    held: BTreeMap<String, Arc<OwnedFd>>,
    calls: Arc<dyn MountCalls + Send + Sync>,
    paths: Arc<dyn PathCalls>,
    image_allow: Vec<js_oci::Allow>,
    pool: Pool,
    /// By run id.
    preps: BTreeMap<String, Prep>,
}

impl std::fmt::Debug for Fs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fs")
            .field("dir", &self.dir)
            .field("runs", &self.preps.len())
            .finish_non_exhaustive()
    }
}

impl Fs {
    /// Makes the directory of the runs' mounts (mode `0711`) and a private mount of its own
    /// (bound onto itself only when it is not one already: an agent that restarts finds its
    /// own, with its adopted runs' mounts below it), opens every root, and starts the pool.
    ///
    /// # Errors
    /// See [`FsError`].
    pub(crate) fn open(settings: FsSettings) -> Result<Self, FsError> {
        let dir = settings.dir;
        let failed = |why: String| FsError::Dir {
            dir: dir.clone(),
            why,
        };
        list::make_dir(&dir)
            .and_then(|()| {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711))
            })
            .map_err(|why| failed(why.to_string()))?;
        make_private_dir(settings.calls.as_ref(), &dir).map_err(|why| failed(why.to_string()))?;
        let mut held = BTreeMap::new();
        for (name, path) in settings.roots.named() {
            let opened = open_root(path).map_err(|why| FsError::Root {
                name: name.to_owned(),
                why,
            })?;
            held.insert(name.to_owned(), Arc::new(opened));
        }
        Ok(Self {
            dir,
            roots: settings.roots,
            held,
            calls: settings.calls,
            paths: settings.paths,
            image_allow: settings.image_allow,
            pool: Pool::new(pool::THREADS)?,
            preps: BTreeMap::new(),
        })
    }

    /// Whether an image of the repository `location` may be pulled for a layer.
    pub(crate) fn allows(&self, location: &js_oci::Location) -> bool {
        self.image_allow.iter().any(|entry| entry.admits(location))
    }

    /// The directory of the mounts of the lease `name`.
    pub(crate) fn dir_of(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The names of the leases that have a directory of mounts.
    pub(crate) fn leases(&self) -> Vec<String> {
        let entries = std::fs::read_dir(&self.dir).into_iter().flatten().flatten();
        entries
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// Begins, or follows, the lookups of the host paths of the run `run_id` (lease `name`):
    /// each is asked of the pool at the first call, bounded by [`LOOKUP_MS`] from `now_ms`.
    /// [`FsPrepared::Ready`] once every one is open; refused `input-not-allowed` for a path
    /// beneath no root or one that leads out of it by a link, `input-unavailable` for one
    /// that is missing, is not a directory, cannot be searched or did not answer in time.
    pub(crate) fn resolve(
        &mut self,
        (run_id, name): (&str, &str),
        image: &str,
        fs: &JobFs,
        now_ms: u64,
    ) -> FsPrepared {
        let Some(prep) = self.preps.get_mut(run_id) else {
            let lookups = fs
                .host_paths()
                .into_iter()
                .map(|source| self.look_up(source.path, source.sub_path, now_ms))
                .collect();
            let prep = Prep {
                name: name.to_owned(),
                image: image.to_owned(),
                fs: fs.clone(),
                lookups,
                assembly: Assembly::Unasked,
            };
            self.preps.insert(run_id.to_owned(), prep);
            return FsPrepared::Pending;
        };
        // Every lookup is polled: one that failed is the answer at once, whatever another
        // that is held still waits for.
        let mut waiting = false;
        for lookup in &mut prep.lookups {
            let Lookup::Asked(path, pending) = lookup else {
                continue;
            };
            match pending.poll(now_ms) {
                Poll::Waiting => waiting = true,
                Poll::Answered(Ok(opened)) => *lookup = Lookup::Open(opened),
                Poll::Answered(Err(refused)) => return FsPrepared::Refused(refused),
                Poll::Unanswered(why) => {
                    return FsPrepared::Refused(NativeRefusal {
                        code: refusal::INPUT_UNAVAILABLE,
                        why: format!("the lookup of {path} {why} (bound {LOOKUP_MS} ms)"),
                    });
                }
            }
        }
        match waiting {
            true => FsPrepared::Pending,
            false => FsPrepared::Ready,
        }
    }

    /// Asks the pool to open `path` beneath its root, and `sub_path` beneath it.
    fn look_up(&self, path: &str, sub_path: Option<&str>, now_ms: u64) -> Lookup {
        // Every root is held from the start: the one a path lies beneath is open.
        let root = self.roots.beneath(path).and_then(|beneath| {
            let held = self.held.get(beneath.name);
            held.map(|held| (Arc::clone(held), beneath.rest))
        });
        let paths = Arc::clone(&self.paths);
        let (given, sub_path) = (path.to_owned(), sub_path.map(PathBuf::from));
        let deadline = now_ms.saturating_add(LOOKUP_MS);
        let pending = self.pool.run(deadline, move || {
            let Some((root, rest)) = root else {
                let why = format!("{given} lies beneath no root of this agent");
                return Err(not_allowed(why));
            };
            let opened = paths
                .open_beneath(root.as_fd(), &rest)
                .map_err(refusal_of)?;
            match sub_path {
                None => Ok(opened),
                Some(sub_path) => paths
                    .open_beneath(opened.as_fd(), &sub_path)
                    .map_err(refusal_of),
            }
        });
        Lookup::Asked(path.to_owned(), pending)
    }

    /// The image and the layers of the run `run_id` being prepared, for the runtime to pin
    /// the images of before it asks for the assembly; `None` for a run not being prepared,
    /// and for one whose assembly was asked for already.
    pub(crate) fn unassembled(&self, run_id: &str) -> Option<(&str, &str, &[Layer])> {
        let prep = self.preps.get(run_id)?;
        matches!(prep.assembly, Assembly::Unasked).then_some((
            prep.name.as_str(),
            prep.image.as_str(),
            prep.fs.layers.as_slice(),
        ))
    }

    /// Asks the pool for the assembly of the run `run_id` (its host paths open), over the
    /// image's tree `image` and the trees of its image layers (`trees`, base first, one for
    /// each image layer), in the lease directory `lease`, with `owners`.
    pub(crate) fn ask_assembly(
        &mut self,
        run_id: &str,
        (image, trees): (Option<PathBuf>, Vec<PathBuf>),
        lease: PathBuf,
        owners: Owners,
    ) -> Result<(), String> {
        let prep = self
            .preps
            .get_mut(run_id)
            .ok_or_else(|| format!("run {run_id} has no file system being prepared"))?;
        let mut sources = Vec::new();
        for lookup in std::mem::take(&mut prep.lookups) {
            match lookup {
                Lookup::Open(opened) => sources.push(opened),
                Lookup::Asked(path, _) => return Err(format!("{path} is not open yet")),
            }
        }
        let mut trees = trees.into_iter();
        let layers = prep
            .fs
            .layers
            .iter()
            .map(|layer| match layer {
                Layer::HostPath { .. } => Some(Lower::Source),
                Layer::Image { .. } => trees.next().map(Lower::Image),
            })
            .collect::<Option<Vec<Lower>>>()
            .ok_or_else(|| format!("run {run_id}: an image layer has no tree"))?;
        let dir = self.dir.join(&prep.name);
        list::make_dir(&dir).map_err(|why| format!("make {}: {why}", dir.display()))?;
        let plan = Plan {
            dir,
            lease,
            image,
            layers,
            mounts: prep.fs.mounts.clone(),
            sources,
            owners,
        };
        let (calls, paths) = (Arc::clone(&self.calls), Arc::clone(&self.paths));
        let pending = self.pool.run(u64::MAX, move || {
            assemble::assemble(calls.as_ref(), paths.as_ref(), &plan)
        });
        prep.assembly = Assembly::Asked(pending);
        Ok(())
    }

    /// How the assembly of the run `run_id` stands.
    pub(crate) fn assembled(&mut self, run_id: &str, now_ms: u64) -> FsPrepared {
        let Some(prep) = self.preps.get_mut(run_id) else {
            return failed(format!("run {run_id} has no file system being prepared"));
        };
        let Assembly::Asked(pending) = &prep.assembly else {
            return match prep.assembly {
                Assembly::Made(_) => FsPrepared::Ready,
                _ => failed(format!("the assembly of run {run_id} was not asked for")),
            };
        };
        match pending.poll(now_ms) {
            Poll::Waiting => FsPrepared::Pending,
            Poll::Answered(Ok(made)) => {
                prep.assembly = Assembly::Made(made);
                FsPrepared::Ready
            }
            Poll::Answered(Err(Unassembled::Refused(refused))) => FsPrepared::Refused(refused),
            Poll::Answered(Err(Unassembled::Failed(why))) => failed(why),
            Poll::Unanswered(why) => failed(format!("the assembly of run {run_id} {why}")),
        }
    }

    /// What the assembly of the run `run_id` made, for its container: `None` for a run
    /// whose file system was not assembled.
    pub(crate) fn take(&mut self, run_id: &str) -> Option<Assembled> {
        match self.preps.remove(run_id)?.assembly {
            Assembly::Made(made) => Some(made),
            _ => None,
        }
    }

    /// Forgets what is prepared for the run `run_id` (lease `name`), which will not be made,
    /// and removes what its assembly mounted.
    ///
    /// # Errors
    /// See [`Fs::release`].
    pub(crate) fn discard(&mut self, run_id: &str, name: &str) -> Result<(), String> {
        self.preps.remove(run_id);
        self.release(name)
    }

    /// Unmounts every mount of the lease `name`, deepest first, from its list, and removes
    /// its directory of mounts. A lease with none has nothing to remove.
    ///
    /// # Errors
    /// A mount could not be removed, or the list read: the directory and the list stay.
    pub(crate) fn release(&self, name: &str) -> Result<(), String> {
        list::release(self.calls.as_ref(), &self.dir_of(name))
    }

    /// The number of host paths the lease `name` cloned, from its list: those at
    /// `src/<i>`.
    pub(crate) fn sources_of(&self, name: &str) -> usize {
        let src = self.dir_of(name).join("src");
        let listed = list::points(&self.dir_of(name)).unwrap_or_default();
        listed
            .iter()
            .filter(|point| point.parent() == Some(&src))
            .count()
    }

    /// Asks the pool whether each of the `sources` clones of the lease `name` still answers
    /// ([`PathCalls::answers`]), bounded by [`CHECK_MS`] from `now_ms`: the answer is the
    /// first that is [unreadable](kernel::unreadable), `None` when all answer.
    pub(crate) fn check(&self, name: &str, sources: usize, now_ms: u64) -> Pending<Option<String>> {
        let src = self.dir_of(name).join("src");
        let paths = Arc::clone(&self.paths);
        self.pool.run(now_ms.saturating_add(CHECK_MS), move || {
            (0..sources).find_map(|i| {
                let point = src.join(i.to_string());
                let answer = paths.answers(&point);
                answer
                    .err()
                    .filter(kernel::unreadable)
                    .map(|why| format!("{}: {why}", point.display()))
            })
        })
    }
}

/// A failure of the runtime.
fn failed(why: String) -> FsPrepared {
    FsPrepared::Failed(RuntimeError::new(why))
}

/// The refusal `input-not-allowed` for `why`.
fn not_allowed(why: String) -> NativeRefusal {
    NativeRefusal {
        code: refusal::INPUT_NOT_ALLOWED,
        why,
    }
}

/// The refusal of a host path that could not be opened beneath its root: one that leads
/// out of it is not allowed; any other is unavailable.
fn refusal_of(why: ResolveError) -> NativeRefusal {
    match why {
        ResolveError::Escapes { .. } => not_allowed(why.to_string()),
        ResolveError::Unavailable { .. } => NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: why.to_string(),
        },
    }
}
