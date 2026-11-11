//! The `crun` runtime's side of a job's layers and mounts (`docs/design/job-rootfs.md`):
//! the calls of the [`Runtime`](crate::lifecycle::Runtime) trait over [`super::fs::Fs`], the
//! images an assembly pins (the image as the lease's own pin, each image layer as
//! `<lease>.l<k>`, `k` its place among the layers), what a container is given of what was
//! assembled, the removal of a lease's mounts on every path that ends it, and the check of
//! a job's sources after it ended without success.

use std::path::{Path, PathBuf};

use js_agent_wire::{Layer, limits, refusal};
use js_contain::container::ContainerSpec;
use js_contain::oci::Oci;
use js_contain::oci::image::published_root;

use super::CrunRuntime;
use super::fs::pool::{Pending, Poll};
use super::fs::{Assembled, Fs, Owners};
use super::ledger::{Failed, Reason, Removed, SweepReport};
use super::spec::pinned;
use crate::cli_runtime::container_name;
use crate::lifecycle::{Exit, FsPrepared, JobFs, RuntimeError, Status};

/// The name of the pin of the image of a lease's layer `k`.
pub(super) fn layer_pin(name: &str, k: usize) -> String {
    format!("{name}.l{k}")
}

/// What a run's sources hold for the check after it ended.
#[derive(Debug)]
pub(super) struct RunFs {
    /// How many host paths the run cloned.
    sources: usize,
    check: Check,
}

/// The check of a run's sources after it ended.
#[derive(Debug)]
enum Check {
    /// Not asked: the run has not ended, or ended with success.
    Unasked,
    /// Asked of the pool.
    Asked(Pending<Option<String>>),
    /// Answered: the source that did not answer, if one did not.
    Done(Option<String>),
}

impl RunFs {
    /// The sources of a run that cloned `sources` host paths.
    pub(super) fn new(sources: usize) -> Self {
        Self {
            sources,
            check: Check::Unasked,
        }
    }
}

/// Pins `image` under the name `pin` in the store `oci` (whose directory is `store`) and
/// gives its tree there, which stays until the pin is removed. An image the store no longer
/// holds (evicted since it was fetched) fails: its fetch is not made again here.
fn pin_tree((oci, store): (&Oci, &Path), image: &str, pin: &str) -> Result<PathBuf, String> {
    let pinned = pinned(image)?;
    oci.pin_image(&pinned, pin).map_err(|why| why.to_string())?;
    let tree = published_root(store, &pinned.digest);
    match tree.is_dir() {
        true => Ok(tree),
        false => Err(format!("the image {image} is no longer in the store")),
    }
}

/// The failure of a call for the run `run_id` on a runtime that gives it no file system.
fn no_fs(run_id: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "run {run_id}: this runtime gives no layers or mounts"
    ))
}

impl CrunRuntime {
    /// Whether this runtime gives a job layers and mounts: it was opened with what they are
    /// made with (only where the kernel has the system calls), with its leases beside the
    /// runs (where the lifecycle puts them under the run's quota).
    pub(super) fn gives_layers(&self) -> bool {
        self.leases_beside && self.fs.is_some()
    }

    /// The lease of the run `run_id`, and the file systems, for a call on them.
    fn fs_of(&mut self, run_id: &str) -> Result<(String, &mut Fs), RuntimeError> {
        let name = container_name(&self.host_id, run_id).map_err(|_| no_fs(run_id))?;
        let fs = self.fs.as_mut().ok_or_else(|| no_fs(run_id))?;
        Ok((name, fs))
    }

    /// [`Runtime::resolve_fs`](crate::lifecycle::Runtime::resolve_fs).
    pub(super) fn resolve(
        &mut self,
        run_id: &str,
        image: &str,
        job: &JobFs,
        now: u64,
    ) -> FsPrepared {
        match self.fs_of(run_id) {
            Ok((name, fs)) => fs.resolve((run_id, &name), image, job, now),
            Err(why) => FsPrepared::Failed(why),
        }
    }

    /// [`Runtime::assemble_fs`](crate::lifecycle::Runtime::assemble_fs): at the first call,
    /// the images are pinned for the lease and the assembly asked for.
    pub(super) fn assemble(&mut self, run_id: &str, now: u64) -> FsPrepared {
        if let Err(why) = self.ask_assembly(run_id) {
            return FsPrepared::Failed(RuntimeError::new(why));
        }
        match self.fs_of(run_id) {
            Ok((_, fs)) => fs.assembled(run_id, now),
            Err(why) => FsPrepared::Failed(why),
        }
    }

    /// Pins the images of the run `run_id`, whose host paths are open, and asks for its
    /// assembly; nothing for a run whose assembly was asked for, or that is not prepared.
    fn ask_assembly(&mut self, run_id: &str) -> Result<(), String> {
        let owners = Owners {
            root: self.subordinate_ids(),
            job: self.oci.container_ids(),
            user: self.oci.job_ids(),
        };
        let Self { fs, oci, store, .. } = self;
        let Some(fs) = fs.as_mut() else {
            return Ok(());
        };
        let Some((name, image, layers)) = fs.unassembled(run_id) else {
            return Ok(());
        };
        let (name, image, layers) = (name.to_owned(), image.to_owned(), layers.to_vec());
        let image = match image.is_empty() {
            true => None,
            false => Some(pin_tree((oci, store), &image, &name)?),
        };
        let mut trees = Vec::new();
        for (k, layer) in layers.iter().enumerate() {
            if let Layer::Image { image } = layer {
                trees.push(pin_tree((oci, store), image, &layer_pin(&name, k))?);
            }
        }
        let lease = oci.lease_dirs(&name).dir;
        fs.ask_assembly(run_id, (image, trees), lease, owners)
    }

    /// [`Runtime::discard_fs`](crate::lifecycle::Runtime::discard_fs): unmounts what was
    /// assembled for a run that was never made (or whose removal unmounted it already), and
    /// removes the pins its assembly took.
    pub(super) fn discard(&mut self, run_id: &str) -> Result<(), RuntimeError> {
        if self.fs.is_none() {
            return Ok(());
        }
        let (name, fs) = self.fs_of(run_id)?;
        fs.discard(run_id, &name).map_err(RuntimeError::new)?;
        self.unpin_all(&name).map_err(RuntimeError::new)
    }

    /// Removes the pins of the lease `name`, its image's and its image layers' (one of a
    /// runtime that gives no layers has none of the latter), and its hold on the image
    /// budget.
    pub(super) fn unpin_all(&self, name: &str) -> Result<(), String> {
        let pins = std::iter::once(name.to_owned())
            .chain((0..limits::MAX_LAYERS).map(|k| layer_pin(name, k)));
        let unpinned: Result<Vec<()>, _> = pins.map(|pin| self.oci.unpin_image(&pin)).collect();
        self.images.release(name);
        unpinned.map(drop).map_err(|why| why.to_string())
    }

    /// Unmounts every mount of the lease `name` from its list and removes its directory of
    /// mounts; nothing for a runtime that gives no layers.
    ///
    /// # Errors
    /// A mount could not be removed.
    pub(super) fn release_fs(&self, name: &str) -> Result<(), String> {
        self.fs.as_ref().map_or(Ok(()), |fs| fs.release(name))
    }

    /// After the leases of an earlier process are adopted or removed: the directory of
    /// mounts of every lease of this agent that it holds no run of (an assembly whose run
    /// was never made, by a process that ended) is unmounted and removed, and its pins with
    /// it, reported as a lease removed with no record.
    pub(super) fn sweep_mounts(&self, report: &mut SweepReport) {
        let prefix = self.lease_prefix();
        let leases = self.fs.as_ref().map(Fs::leases).unwrap_or_default();
        let left = leases
            .into_iter()
            .filter(|name| name.starts_with(&prefix) && !self.runs.contains_key(name));
        for name in left {
            let run_id = name[prefix.len()..].to_owned();
            let removed = self.release_fs(&name).and_then(|()| self.unpin_all(&name));
            match removed {
                Ok(()) => report.removed.push(Removed {
                    run_id,
                    name,
                    labels: Default::default(),
                    reason: Reason::NoRecord,
                }),
                Err(why) => report.failed.push(Failed { run_id, name, why }),
            }
        }
    }

    /// What an adopted lease `name` cloned, for the check after it ends: none for a lease
    /// with no directory of mounts.
    pub(super) fn run_fs_of(&self, name: &str) -> Option<RunFs> {
        let fs = self.fs.as_ref()?;
        fs.dir_of(name)
            .is_dir()
            .then(|| RunFs::new(fs.sources_of(name)))
    }

    /// What the assembly of a run made, given to its container: the root it mounted
    /// (writable, in the place of the image's tree) and its mounts, bound after the run's
    /// scratch directory and before its files.
    pub(super) fn give(container: &mut ContainerSpec, assembled: &Assembled) {
        container.mounted_root.clone_from(&assembled.root);
        container
            .mounts
            .splice(1..1, assembled.mounts.iter().cloned());
    }

    /// The status of the run `name`, whose container ended with `exit`, once its sources
    /// have answered: a run that cloned host paths and did not succeed is held as running
    /// until the check of its sources ([`Fs::check`]) has answered or its bound passed.
    pub(super) fn checked(&mut self, name: &str, exit: Exit) -> Status {
        let now = (self.hooks.now_ms)();
        let Self { runs, fs, .. } = self;
        let ended = Status::Exited(exit);
        let (Some(run), Some(fs)) = (runs.get_mut(name), fs.as_ref()) else {
            return ended;
        };
        let Some(sources) = run.fs.as_mut() else {
            return ended;
        };
        let succeeded = exit.code == 0 && !exit.oom_killed;
        match (&sources.check, succeeded) {
            (_, true) | (Check::Done(_), _) => ended,
            (Check::Unasked, false) => {
                sources.check = Check::Asked(fs.check(name, sources.sources, now));
                Status::Running
            }
            (Check::Asked(pending), false) => match pending.poll(now) {
                Poll::Waiting => Status::Running,
                Poll::Answered(found) => {
                    sources.check = Check::Done(found);
                    ended
                }
                Poll::Unanswered(why) => {
                    sources.check = Check::Done(Some(format!("the check {why}")));
                    ended
                }
            },
        }
    }

    /// [`Runtime::input_failure`](crate::lifecycle::Runtime::input_failure): `input-unreadable`
    /// for a run whose check found a source that did not answer.
    pub(super) fn unreadable(&self, name: &str) -> Option<String> {
        let run = self.runs.get(name)?;
        match &run.fs.as_ref()?.check {
            Check::Done(Some(why)) => {
                tracing::warn!(lease = %name, "a source did not answer after the job: {why}");
                Some(refusal::INPUT_UNREADABLE.to_owned())
            }
            _ => None,
        }
    }

    /// [`Runtime::check_layer_image`](crate::lifecycle::Runtime::check_layer_image): the
    /// repository of `image` must be on an entry of the registries allowed.
    pub(super) fn check_layer(&self, image: &str) -> Result<(), String> {
        let reference = pinned(image)?.reference;
        let repository = reference
            .rsplit_once('@')
            .map_or(reference.as_str(), |(name, _)| name);
        let location = js_oci::Location::parse(repository).map_err(|why| why.to_string())?;
        match self.fs.as_ref().is_some_and(|fs| fs.allows(&location)) {
            true => Ok(()),
            false => Err(format!(
                "the image {image} is on no --image-allow or --image-allow-loopback entry"
            )),
        }
    }
}
