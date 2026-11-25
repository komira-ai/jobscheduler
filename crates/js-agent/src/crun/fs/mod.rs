//! A job's file system on the `crun` runtime (`docs/design/job-rootfs.md`, sections 4.2 to
//! 4.7 and 8): [`Fs`], which holds the roots a job's host paths lie beneath (opened when
//! the agent starts), the directory of the runs' mounts (`<mounts>`, made a private mount
//! of its own when the agent starts) and two pools of threads (`js-lookups`, `LOOKUP_THREADS`
//! threads, for the lookups of host paths; `js-paths`, `pool::THREADS` threads, for the
//! assemblies and the checks, so a start's lookups never wait behind another's assembly or
//! check), and prepares each run's file system off the lifecycle's worker:
//!
//! 1. each host path is opened beneath its root, and a mount's `sub_path` beneath it, on the
//!    lookups' pool ([`Fs::resolve`]), each lookup bounded by [`LOOKUP_MS`] on the lifecycle's
//!    clock; a start with none is ready at the first call, and a later call that adds host paths
//!    keeps the lookups already begun and asks only the new ones. Each lookup is stamped on the
//!    runtime's wall clock ([`Fs::with_clock`]) when it is asked, when a thread begins it and
//!    when it ends ([`LookupStamps`]), and the run keeps the stamps of the one that ended last;
//! 2. once they are open and the images are in the store, the assembly ([`assemble`]) runs
//!    on the other pool ([`Fs::ask_assembly`]): clones, overlays, targets, each mount listed
//!    in `<mounts>/<lease>/mounts` ([`list`]) before it is made;
//! 3. the container takes what was made ([`Fs::take`]); a run never made has it removed
//!    ([`Fs::discard`]); every end of a run unmounts it from the list ([`Fs::release`]);
//! 4. after a job that did not succeed, each of its sources is asked whether it still
//!    answers ([`Fs::check`], on the other pool), within [`CHECK_MS`].
//!
//! The system calls are [`MountCalls`] (js-contain's) and [`PathCalls`], so what is built
//! from them is tested with fakes as an ordinary user.

pub(crate) mod assemble;
pub(crate) mod kernel;
mod list;

#[cfg(test)]
mod assemble_tests;
#[cfg(test)]
mod fs_tests;
#[cfg(test)]
mod lookups_tests;
#[cfg(test)]
mod stamps_tests;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;

use js_agent_wire::{Layer, limits, refusal};
use js_contain::job_fs::{MountCalls, ResolveError, make_private_dir, open_root};

use crate::pool::{self, Pending, Poll, Pool};
pub(crate) use assemble::{Assembled, Lower, Owners, Plan, Unassembled};
pub use kernel::{Kernel, PathCalls};

use crate::lifecycle::{
    FsPrepared, HostSource, JobFs, LookupStamps, MountRoots, NativeRefusal, RuntimeError,
};

/// The wall clock the lookups are stamped on, in milliseconds since the Unix epoch: read on
/// the worker when a lookup is asked and on the pool's thread when it begins and ends.
pub(crate) type WallClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// What a thread of the lookups' pool answers: the open path, or why it was not opened,
/// with the lookup's stamps.
type Looked = (Result<OwnedFd, NativeRefusal>, LookupStamps);

/// Where the agent mounts what its runs' file systems are made of: outside the scratch
/// root, which every release sweeps at its start, on a `tmpfs` a reboot clears.
pub const MOUNTS_DIR: &str = "/run/js-agent/mounts";

/// How long the lookup of one host path may take, from when it is asked.
pub const LOOKUP_MS: u64 = 60_000;

/// The threads of the lookups' pool (`js-lookups`): as many as the host paths one start may
/// name, every layer ([`limits::MAX_LAYERS`]) and every mount ([`limits::MAX_MOUNTS`]) a host
/// path, so the lookups of one start never wait for a thread behind one another. Fixed, as
/// every pool's: a provider that hangs holds at most these threads, and none of another pool.
pub(crate) const LOOKUP_THREADS: usize = limits::MAX_LAYERS + limits::MAX_MOUNTS;

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
    /// The threads of a pool could not be made.
    #[error("the threads of the host paths: {0}")]
    Threads(#[from] io::Error),
}

/// One host path being looked up, or open.
#[derive(Debug)]
struct Lookup {
    /// The host path as the start named it.
    path: String,
    /// The directory below it that is opened as well (a mount's `sub_path`).
    sub_path: Option<String>,
    state: Opened,
}

/// How a [`Lookup`] stands.
#[derive(Debug)]
enum Opened {
    /// Asked of the lookups' pool: the answer to come.
    Asked(Pending<Looked>),
    /// Open, with when it was asked, begun and ended.
    Open(OwnedFd, LookupStamps),
}

impl Lookup {
    /// Whether this is the lookup of `source`.
    fn is_of(&self, source: &HostSource<'_>) -> bool {
        self.path == source.path && self.sub_path.as_deref() == source.sub_path
    }
}

/// How the assembly of a run stands.
#[derive(Debug)]
enum Assembly {
    /// Not asked yet: the host paths are being looked up, or are open.
    Unasked,
    /// Asked of the assemblies' pool.
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
    /// The stamps of the lookup that ended last, kept when the assembly takes the open paths;
    /// none for a run with no host path.
    waited: Option<LookupStamps>,
    assembly: Assembly,
}

/// What a thread of the removal pool unmounts a lease's file system with ([`Fs::releaser`]).
pub(crate) struct Releaser {
    calls: Arc<dyn MountCalls + Send + Sync>,
    /// Where the leases' directories of mounts are.
    dir: PathBuf,
}

impl Releaser {
    /// [`Fs::release`] of the lease `name`.
    ///
    /// # Errors
    /// A mount could not be removed, or the list read: the directory and the list stay.
    pub(crate) fn release(&self, name: &str) -> Result<(), String> {
        list::release(self.calls.as_ref(), &self.dir.join(name))
    }
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
    /// The threads of the lookups of host paths, which no assembly or check holds.
    lookups: Pool,
    /// The threads of the assemblies and the checks: an assembly holds one for as long as the
    /// mounts of its layers answer, which a hung file system makes for ever.
    pool: Pool,
    /// By run id.
    preps: BTreeMap<String, Prep>,
    /// What the lookups are stamped on.
    now_ms: WallClock,
}

impl Prep {
    /// Polls every lookup: one that failed is the answer at once, whatever another that is
    /// held still waits for.
    fn poll(&mut self, now_ms: u64) -> FsPrepared {
        let mut waiting = false;
        for lookup in &mut self.lookups {
            let Opened::Asked(pending) = &lookup.state else {
                continue;
            };
            match pending.poll(now_ms) {
                Poll::Waiting => waiting = true,
                Poll::Answered((Ok(opened), stamps)) => {
                    lookup.state = Opened::Open(opened, stamps);
                }
                Poll::Answered((Err(refused), _)) => return FsPrepared::Refused(refused),
                Poll::Unanswered(why) => {
                    return FsPrepared::Refused(NativeRefusal {
                        code: refusal::INPUT_UNAVAILABLE,
                        why: format!("the lookup of {} {why} (bound {LOOKUP_MS} ms)", lookup.path),
                    });
                }
            }
        }
        match waiting {
            true => FsPrepared::Pending,
            false => FsPrepared::Ready,
        }
    }
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
    /// Makes the directory of the runs' mounts and its parent (mode `0711`, the parent too when
    /// another part of the agent made it closed: nothing below it is searchable otherwise) and a private mount of its own
    /// (bound onto itself only when it is not one already: an agent that restarts finds its
    /// own, with its adopted runs' mounts below it), opens every root, and starts the two pools.
    ///
    /// # Errors
    /// See [`FsError`].
    pub(crate) fn open(settings: FsSettings) -> Result<Self, FsError> {
        let dir = settings.dir;
        let failed = |why: String| FsError::Dir {
            dir: dir.clone(),
            why,
        };
        list::make_searchable(&dir).map_err(|why| failed(why.to_string()))?;
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
            lookups: Pool::named(LOOKUP_THREADS, "js-lookups")?,
            pool: Pool::named(pool::THREADS, "js-paths")?,
            preps: BTreeMap::new(),
            now_ms: Arc::new(super::settings::system_now_ms),
        })
    }

    /// Stamps the lookups asked from now on with `now_ms` instead of the system's wall clock:
    /// the runtime's own clock ([`Hooks::now_ms`](super::Hooks::now_ms)), or a test's.
    #[must_use]
    pub(crate) fn with_clock(mut self, now_ms: WallClock) -> Self {
        self.now_ms = now_ms;
        self
    }

    /// Rings `wake` whenever a lookup, an assembly or a check asked from now on has answered.
    pub(crate) fn set_wake(&mut self, wake: crate::lifecycle::Wake) {
        self.lookups.set_wake(wake.clone());
        self.pool.set_wake(wake);
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
    /// each is asked of the lookups' pool at the first call, bounded by [`LOOKUP_MS`] from
    /// `now_ms`, and the first call polls them as a later one does, so a start with no host path
    /// is [`FsPrepared::Ready`] at once. [`FsPrepared::Ready`] once every one is open; refused
    /// `input-not-allowed` for a path beneath no root or one that leads out of it by a link,
    /// `input-unavailable` for one that is missing, is not a directory, cannot be searched or
    /// did not answer in time.
    ///
    /// A later call may bring a `fs` that adds host paths (the lifecycle asks first without
    /// the layers and mounts a provider makes, then with the paths the providers answered).
    /// Each lookup already begun whose path and `sub_path` a host path of the new `fs` names
    /// is kept (one lookup for one host path), a host path that matches none is asked, a
    /// lookup that matches none is dropped, and the lookups follow the order of
    /// [`JobFs::host_paths`]. The new `fs` and `image` are the ones the assembly is made
    /// from. A call with the `fs` of the run's preparation only polls.
    pub(crate) fn resolve(
        &mut self,
        (run_id, name): (&str, &str),
        image: &str,
        fs: &JobFs,
        now_ms: u64,
    ) -> FsPrepared {
        let mut prep = self.preps.remove(run_id).unwrap_or_else(|| Prep {
            name: name.to_owned(),
            image: String::new(),
            fs: JobFs::default(),
            lookups: Vec::new(),
            waited: None,
            assembly: Assembly::Unasked,
        });
        // The image of the last call is the assembly's, whether or not its fs changed.
        image.clone_into(&mut prep.image);
        if prep.fs != *fs {
            prep.lookups = self.reconcile(std::mem::take(&mut prep.lookups), fs, now_ms);
            prep.fs = fs.clone();
        }
        let answer = prep.poll(now_ms);
        self.preps.insert(run_id.to_owned(), prep);
        answer
    }

    /// The lookups for the host paths of `fs`, in their order: those of `held` that are
    /// still wanted (each matches at most one host path), and a new one for each other.
    fn reconcile(&self, held: Vec<Lookup>, fs: &JobFs, now_ms: u64) -> Vec<Lookup> {
        let mut held: Vec<Option<Lookup>> = held.into_iter().map(Some).collect();
        let wanted = fs.host_paths();
        let mut lookups = Vec::with_capacity(wanted.len());
        for source in wanted {
            let kept = held
                .iter_mut()
                .find(|slot| slot.as_ref().is_some_and(|lookup| lookup.is_of(&source)))
                .and_then(Option::take);
            lookups.push(kept.unwrap_or_else(|| self.look_up(source, now_ms)));
        }
        lookups
    }

    /// Asks the lookups' pool to open `path` beneath its root, and `sub_path` beneath it,
    /// stamped when it is asked, when a thread begins it and when it ends.
    fn look_up(&self, source: HostSource<'_>, now_ms: u64) -> Lookup {
        let (path, sub_path) = (source.path, source.sub_path);
        // Every root is held from the start: the one a path lies beneath is open.
        let root = self.roots.beneath(path).and_then(|beneath| {
            let held = self.held.get(beneath.name);
            held.map(|held| (Arc::clone(held), beneath.name.to_owned(), beneath.rest))
        });
        let paths = Arc::clone(&self.paths);
        let sub_path_text = sub_path.map(str::to_owned);
        let (given, sub_path) = (path.to_owned(), sub_path.map(PathBuf::from));
        let deadline = now_ms.saturating_add(LOOKUP_MS);
        let clock = Arc::clone(&self.now_ms);
        let queued_at_ms = clock();
        let pending = self.lookups.run(deadline, move || {
            let started_at_ms = clock();
            let opened = open_source(paths.as_ref(), root, (&given, sub_path));
            let stamps = LookupStamps {
                queued_at_ms,
                started_at_ms,
                ended_at_ms: clock(),
            };
            (opened, stamps)
        });
        Lookup {
            path: path.to_owned(),
            sub_path: sub_path_text,
            state: Opened::Asked(pending),
        }
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

    /// Asks the assemblies' pool for the assembly of the run `run_id` (its host paths open),
    /// over the image's tree `image` and the trees of its image layers (`trees`, base first,
    /// one for each image layer), in the lease directory `lease`, with `owners`.
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
        let mut waited: Option<LookupStamps> = None;
        for lookup in std::mem::take(&mut prep.lookups) {
            match lookup.state {
                Opened::Open(opened, stamps) => {
                    sources.push(opened);
                    // The one that ended last; of two that ended at once, the earlier listed.
                    if waited.is_none_or(|last| stamps.ended_at_ms > last.ended_at_ms) {
                        waited = Some(stamps);
                    }
                }
                Opened::Asked(_) => return Err(format!("{} is not open yet", lookup.path)),
            }
        }
        prep.waited = waited;
        let mut trees = trees.into_iter();
        let layers = prep
            .fs
            .layers
            .iter()
            .map(|layer| match layer {
                // A layer a provider makes was replaced by its host path before the lookups
                // (`lifecycle`), so it is a source like any host path.
                Layer::HostPath { .. } | Layer::Provider { .. } => Some(Lower::Source),
                Layer::Image { .. } => trees.next().map(Lower::Image),
            })
            .collect::<Option<Vec<Lower>>>()
            .ok_or_else(|| format!("run {run_id}: an image layer has no tree"))?;
        let dir = self.dir.join(&prep.name);
        list::make_searchable(&dir).map_err(|why| format!("make {}: {why}", dir.display()))?;
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

    /// What the assembly of the run `run_id` made, for its container, with the stamps of the
    /// lookup of a host path it waited on (the one that ended last; none without host paths):
    /// `None` for a run whose file system was not assembled.
    pub(crate) fn take(&mut self, run_id: &str) -> Option<(Assembled, Option<LookupStamps>)> {
        let prep = self.preps.remove(run_id)?;
        match prep.assembly {
            Assembly::Made(made) => Some((made, prep.waited)),
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
        self.releaser().release(name)
    }

    /// What unmounts the mounts of a lease off the worker ([`Releaser::release`]): the calls
    /// and the directory of the mounts, and nothing else of the file systems.
    pub(crate) fn releaser(&self) -> Releaser {
        Releaser {
            calls: Arc::clone(&self.calls),
            dir: self.dir.clone(),
        }
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

    /// Asks the assemblies' pool whether each of the `sources` clones of the lease `name`
    /// still answers ([`PathCalls::answers`]), bounded by [`CHECK_MS`] from `now_ms`:
    /// the answer is the first that is [unreadable](kernel::unreadable), `None` when all answer.
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

/// A root this agent holds: the root, opened, its name, and the rest of a path below it.
type HeldRoot = (Arc<OwnedFd>, String, PathBuf);

/// What a thread of the lookups' pool does: opens the host path `given` beneath `root` (none
/// when it lies beneath no root of this agent), and `sub_path` beneath it, with `paths`.
fn open_source(
    paths: &dyn PathCalls,
    root: Option<HeldRoot>,
    (given, sub_path): (&str, Option<PathBuf>),
) -> Result<OwnedFd, NativeRefusal> {
    let Some((root, name, rest)) = root else {
        let why = format!("{given} lies beneath no root of this agent");
        return Err(not_allowed(why));
    };
    let host_path = || format!("the host path {given} (root {name})");
    let opened = paths
        .open_beneath(root.as_fd(), &rest)
        .map_err(|why| refusal_of(&host_path(), why))?;
    match sub_path {
        None => Ok(opened),
        Some(sub_path) => paths
            .open_beneath(opened.as_fd(), &sub_path)
            .map_err(|why| {
                let named = format!("the sub_path {} of {}", sub_path.display(), host_path());
                refusal_of(&named, why)
            }),
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

/// The refusal of `what` (the host path as the start named it, with its root, or a
/// `sub_path` of it) that could not be opened: one that leads out of the directory it is
/// opened beneath is not allowed; any other is unavailable. The text names `what` in full
/// before the kernel's answer for the part below the root, so a finding says which path of
/// which root was missing, not only the part below it.
fn refusal_of(what: &str, why: ResolveError) -> NativeRefusal {
    match why {
        ResolveError::Escapes { .. } => not_allowed(format!("{what}: {why}")),
        ResolveError::Unavailable { .. } => NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: format!("{what}: {why}"),
        },
    }
}
